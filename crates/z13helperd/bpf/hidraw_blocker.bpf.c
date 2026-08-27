// SPDX-License-Identifier: GPL-2.0-only
//
// A deliberately narrow BPF LSM program. It blocks reads only when both the
// current PID is in blocked_pids and the target file is a hidraw character
// device. The GPL declaration below is required by the kernel for the CO-RE
// helper used to read i_rdev.
//
// Kernel i_rdev uses MINORBITS=20 encoding, so MAJOR is (dev >> 20). That is
// not the glibc userspace rdev layout.

#include <linux/bpf.h>
#include <linux/types.h>
#include <bpf/bpf_core_read.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>

#define EAGAIN 11
#define MAY_READ 4
#define MAJOR(dev) ((unsigned int)((dev) >> 20))
#define MAX_BLOCKED_PIDS 64
// The userspace owner updates additions before removals so a failed update can
// be rolled back. Two bounded generations fit during a full-set replacement;
// the map is still daemon-owned, unpinned, and cannot grow without limit.
#define BLOCKED_PID_MAP_CAPACITY (MAX_BLOCKED_PIDS * 2)

// Keys are numeric TGIDs because the userspace owner supplies a PID set.
// Numeric identity can be reused after exit; userspace must refresh and clear
// this set while the capture lease owns the attached link.

struct inode {
	__u64 i_ino;
	__u64 i_mode;
	__u64 i_rdev;
} __attribute__((preserve_access_index));

struct file {
	struct inode *f_inode;
} __attribute__((preserve_access_index));

struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, BLOCKED_PID_MAP_CAPACITY);
	__type(key, __u32);
	__type(value, __u8);
} blocked_pids SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, 1);
	__type(key, __u32);
	__type(value, __u32);
} hidraw_config SEC(".maps");

SEC("lsm/file_permission")
int BPF_PROG(hidraw_block, struct file *file, int mask, int ret)
{
	if (ret != 0)
		return ret;

	if (!(mask & MAY_READ))
		return 0;

	__u32 pid = bpf_get_current_pid_tgid() >> 32;
	if (!bpf_map_lookup_elem(&blocked_pids, &pid))
		return 0;

	__u64 rdev = BPF_CORE_READ(file, f_inode, i_rdev);
	__u32 major = MAJOR(rdev);

	__u32 key = 0;
	__u32 *hidraw_major = bpf_map_lookup_elem(&hidraw_config, &key);
	if (!hidraw_major || major != *hidraw_major)
		return 0;

	return -EAGAIN;
}

char LICENSE[] SEC("license") = "GPL";
