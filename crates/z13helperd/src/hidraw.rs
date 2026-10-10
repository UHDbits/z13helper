//! Narrow BPF LSM blocker for Steam's hidraw reads.
//!
//! It is scoped to Steam PIDs and the kernel's hidraw character-device major.

use std::collections::BTreeSet;
use std::ffi::{c_char, c_int, c_long, c_void};
use std::fs;
use std::io;
use std::ptr;

const BLOCKER: &[u8] = include_bytes!("../bpf/hidraw_blocker.bpf.o");
const MAP_BLOCKED_PIDS: &[u8] = b"blocked_pids\0";
const MAP_HIDRAW_CONFIG: &[u8] = b"hidraw_config\0";
const PROGRAM_HIDRAW_BLOCK: &[u8] = b"hidraw_block\0";

#[repr(C)]
struct BpfObject {
    _private: [u8; 0],
}

#[repr(C)]
struct BpfMap {
    _private: [u8; 0],
}

#[repr(C)]
struct BpfProgram {
    _private: [u8; 0],
}

#[repr(C)]
struct BpfLink {
    _private: [u8; 0],
}

#[link(name = "bpf")]
unsafe extern "C" {
    fn bpf_object__open_mem(
        object: *const c_void,
        object_size: usize,
        options: *const c_void,
    ) -> *mut BpfObject;
    fn bpf_object__load(object: *mut BpfObject) -> c_int;
    fn bpf_object__close(object: *mut BpfObject);
    fn bpf_object__find_map_by_name(object: *const BpfObject, name: *const c_char) -> *mut BpfMap;
    fn bpf_map__fd(map: *const BpfMap) -> c_int;
    fn bpf_object__find_program_by_name(
        object: *const BpfObject,
        name: *const c_char,
    ) -> *mut BpfProgram;
    fn bpf_program__attach_lsm(program: *const BpfProgram) -> *mut BpfLink;
    fn bpf_link__destroy(link: *mut BpfLink) -> c_int;
    fn bpf_map_update_elem(
        fd: c_int,
        key: *const c_void,
        value: *const c_void,
        flags: u64,
    ) -> c_int;
    fn bpf_map_delete_elem(fd: c_int, key: *const c_void) -> c_int;
    fn libbpf_get_error(pointer: *const c_void) -> c_long;
}

pub struct HidrawBlocker {
    object: *mut BpfObject,
    link: *mut BpfLink,
    blocked_pids_fd: c_int,
    blocked: BTreeSet<u32>,
}

impl HidrawBlocker {
    pub fn new() -> Result<Self, String> {
        if !bpf_lsm_enabled() {
            return Err("BPF LSM is not enabled".into());
        }
        let hidraw_major = hidraw_major()?;
        // SAFETY: the included object is immutable ELF data for the process
        // lifetime, and libbpf accepts a null options pointer.
        let object = unsafe {
            bpf_object__open_mem(
                BLOCKER.as_ptr().cast::<c_void>(),
                BLOCKER.len(),
                ptr::null(),
            )
        };
        if object.is_null() {
            return Err(format!("open BPF object: {}", io::Error::last_os_error()));
        }
        if unsafe { bpf_object__load(object) } != 0 {
            return Err(close_failed(object, "load BPF object"));
        }
        let config = unsafe {
            bpf_object__find_map_by_name(object, MAP_HIDRAW_CONFIG.as_ptr().cast::<c_char>())
        };
        let blocked = unsafe {
            bpf_object__find_map_by_name(object, MAP_BLOCKED_PIDS.as_ptr().cast::<c_char>())
        };
        let program = unsafe {
            bpf_object__find_program_by_name(object, PROGRAM_HIDRAW_BLOCK.as_ptr().cast::<c_char>())
        };
        if config.is_null() || blocked.is_null() || program.is_null() {
            unsafe { bpf_object__close(object) };
            return Err("BPF object is missing a required map or program".into());
        }
        let config_fd = unsafe { bpf_map__fd(config) };
        let blocked_pids_fd = unsafe { bpf_map__fd(blocked) };
        if config_fd < 0 || blocked_pids_fd < 0 {
            unsafe { bpf_object__close(object) };
            return Err("BPF map file descriptor is unavailable".into());
        }
        let key = 0_u32;
        let update = unsafe {
            bpf_map_update_elem(
                config_fd,
                (&key as *const u32).cast::<c_void>(),
                (&hidraw_major as *const u32).cast::<c_void>(),
                0,
            )
        };
        if update != 0 {
            return Err(close_failed(object, "configure hidraw BPF map"));
        }
        let link = unsafe { bpf_program__attach_lsm(program) };
        if link.is_null() || unsafe { libbpf_get_error(link.cast::<c_void>()) } != 0 {
            return Err(close_failed(object, "attach hidraw BPF LSM"));
        }
        tracing::info!(hidraw_major, "attached hidraw BPF LSM blocker");
        Ok(Self {
            object,
            link,
            blocked_pids_fd,
            blocked: BTreeSet::new(),
        })
    }

    pub fn has_blocked_pids(&self) -> bool {
        !self.blocked.is_empty()
    }

    pub fn set_blocked_pids(&mut self, pids: impl IntoIterator<Item = u32>) -> Result<(), String> {
        let fd = self.blocked_pids_fd;
        let enabled = 1_u8;
        sync_blocked_pids(
            &mut self.blocked,
            pids,
            |pid| {
                map_result(unsafe {
                    bpf_map_update_elem(
                        fd,
                        (&pid as *const u32).cast::<c_void>(),
                        (&enabled as *const u8).cast::<c_void>(),
                        0,
                    )
                })
            },
            |pid| match map_result(unsafe {
                bpf_map_delete_elem(fd, (&pid as *const u32).cast::<c_void>())
            }) {
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => Ok(()),
                result => result,
            },
        )
    }
}

impl Drop for HidrawBlocker {
    fn drop(&mut self) {
        if !self.link.is_null() {
            let _ = unsafe { bpf_link__destroy(self.link) };
        }
        if !self.object.is_null() {
            unsafe { bpf_object__close(self.object) };
        }
    }
}

fn map_result(result: c_int) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Moves the PID map from `blocked` to the first 64 non-zero `pids` in caller
/// order. Additions go first and a failed one undoes the partial step. Removals
/// are best-effort so a release clears every PID it can; `blocked` keeps the
/// ones still in the map for the next call.
fn sync_blocked_pids(
    blocked: &mut BTreeSet<u32>,
    pids: impl IntoIterator<Item = u32>,
    mut insert: impl FnMut(u32) -> io::Result<()>,
    mut remove: impl FnMut(u32) -> io::Result<()>,
) -> Result<(), String> {
    let target: BTreeSet<u32> = pids.into_iter().filter(|pid| *pid != 0).take(64).collect();
    let additions: Vec<u32> = target.difference(blocked).copied().collect();
    for (index, &pid) in additions.iter().enumerate() {
        if let Err(error) = insert(pid) {
            for &added in &additions[..index] {
                if remove(added).is_err() {
                    blocked.insert(added);
                }
            }
            return Err(format!("block hidraw PID {pid}: {error}"));
        }
    }
    blocked.extend(additions);
    let removals: Vec<u32> = blocked.difference(&target).copied().collect();
    let mut failed = Vec::new();
    for pid in removals {
        match remove(pid) {
            Ok(()) => {
                blocked.remove(&pid);
            }
            Err(error) => failed.push(format!("hidraw PID {pid}: {error}")),
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("unblock {}", failed.join(", ")))
    }
}

/// Closes `object` after capturing errno from the libbpf call that just
/// failed, so cleanup cannot clobber the reported cause.
fn close_failed(object: *mut BpfObject, context: &str) -> String {
    let error = io::Error::last_os_error();
    unsafe { bpf_object__close(object) };
    format!("{context}: {error}")
}

fn bpf_lsm_enabled() -> bool {
    fs::read_to_string("/sys/kernel/security/lsm")
        .ok()
        .is_some_and(|modules| modules.split(',').any(|module| module.trim() == "bpf"))
}

/// Character-device major for hidraw, matching what the BPF program extracts
/// from kernel `i_rdev` with `(dev >> 20)`.
fn hidraw_major() -> Result<u32, String> {
    let devices = fs::read_to_string("/proc/devices").map_err(|error| error.to_string())?;
    for line in devices.lines() {
        let mut fields = line.split_whitespace();
        let Some(major) = fields.next() else {
            continue;
        };
        if fields.next() == Some("hidraw") && fields.next().is_none() {
            return major
                .parse()
                .map_err(|error| format!("parse hidraw major {major:?}: {error}"));
        }
    }
    Err("hidraw not found in /proc/devices".into())
}

#[cfg(test)]
mod tests {
    use super::{hidraw_major, sync_blocked_pids};
    use std::cell::RefCell;
    use std::collections::BTreeSet;
    use std::io;

    #[test]
    fn release_clears_every_pid_it_can_and_tracks_only_the_rest() {
        let map = RefCell::new(BTreeSet::from([1, 2, 3]));
        let mut blocked = map.borrow().clone();
        let result = sync_blocked_pids(
            &mut blocked,
            [],
            |pid| {
                map.borrow_mut().insert(pid);
                Ok(())
            },
            |pid| {
                if pid == 2 {
                    return Err(io::Error::from_raw_os_error(libc::EBUSY));
                }
                map.borrow_mut().remove(&pid);
                Ok(())
            },
        );
        assert!(result.unwrap_err().contains("hidraw PID 2"));
        assert_eq!(*map.borrow(), BTreeSet::from([2]));
        assert_eq!(blocked, BTreeSet::from([2]));
    }

    #[test]
    fn capacity_cut_keeps_caller_order_and_failed_block_restores_the_map() {
        let mut blocked = BTreeSet::new();
        sync_blocked_pids(&mut blocked, (1..=100).rev(), |_| Ok(()), |_| Ok(())).unwrap();
        assert_eq!(blocked, (37..=100).collect());

        let map = RefCell::new(blocked.clone());
        let result = sync_blocked_pids(
            &mut blocked,
            [200, 201, 202],
            |pid| {
                if pid == 202 {
                    return Err(io::Error::from_raw_os_error(libc::E2BIG));
                }
                map.borrow_mut().insert(pid);
                Ok(())
            },
            |pid| {
                map.borrow_mut().remove(&pid);
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(*map.borrow(), (37..=100).collect());
        assert_eq!(blocked, (37..=100).collect());
    }

    #[test]
    fn reads_hidraw_major_from_proc_devices() {
        match hidraw_major() {
            Ok(major) => assert_ne!(major, 0, "hidraw major should be non-zero"),
            Err(error) => {
                // CI and containers may lack the hidraw driver.
                assert!(
                    error.contains("hidraw not found") || error.contains("No such file"),
                    "unexpected hidraw major error: {error}"
                );
            }
        }
    }
}
