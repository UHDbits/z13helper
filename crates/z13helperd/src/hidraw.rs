//! Narrow BPF LSM blocker for Steam's hidraw reads.
//!
//! It is scoped to Steam PIDs and the kernel's hidraw character-device major.

use std::collections::BTreeSet;
use std::ffi::{c_char, c_int, c_long, c_void};
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
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
            return Err(format!(
                "open BPF object: {}",
                std::io::Error::last_os_error()
            ));
        }
        if unsafe { bpf_object__load(object) } != 0 {
            unsafe { bpf_object__close(object) };
            return Err(format!(
                "load BPF object: {}",
                std::io::Error::last_os_error()
            ));
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
            unsafe { bpf_object__close(object) };
            return Err(format!(
                "configure hidraw BPF map: {}",
                std::io::Error::last_os_error()
            ));
        }
        let link = unsafe { bpf_program__attach_lsm(program) };
        if link.is_null() || unsafe { libbpf_get_error(link.cast::<c_void>()) } != 0 {
            unsafe { bpf_object__close(object) };
            return Err(format!(
                "attach hidraw BPF LSM: {}",
                std::io::Error::last_os_error()
            ));
        }
        tracing::info!(hidraw_major, "attached hidraw BPF LSM blocker");
        Ok(Self {
            object,
            link,
            blocked_pids_fd,
            blocked: BTreeSet::new(),
        })
    }

    pub fn set_blocked_pids(&mut self, pids: impl IntoIterator<Item = u32>) -> Result<(), String> {
        let target: BTreeSet<u32> = pids.into_iter().filter(|pid| *pid != 0).take(64).collect();
        for pid in self.blocked.difference(&target) {
            let result = unsafe {
                bpf_map_delete_elem(self.blocked_pids_fd, (pid as *const u32).cast::<c_void>())
            };
            if result != 0 {
                return Err(format!(
                    "unblock hidraw PID {pid}: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        let enabled = 1_u8;
        for pid in target.difference(&self.blocked) {
            let result = unsafe {
                bpf_map_update_elem(
                    self.blocked_pids_fd,
                    (pid as *const u32).cast::<c_void>(),
                    (&enabled as *const u8).cast::<c_void>(),
                    0,
                )
            };
            if result != 0 {
                return Err(format!(
                    "block hidraw PID {pid}: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        self.blocked = target;
        Ok(())
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

fn bpf_lsm_enabled() -> bool {
    fs::read_to_string("/sys/kernel/security/lsm")
        .ok()
        .is_some_and(|modules| modules.split(',').any(|module| module.trim() == "bpf"))
}

fn hidraw_major() -> Result<u32, String> {
    let entries = fs::read_dir("/dev").map_err(|error| error.to_string())?;
    entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("hidraw"))
        .find_map(|entry| {
            let metadata = entry.metadata().ok()?;
            metadata
                .file_type()
                .is_char_device()
                .then(|| device_major(metadata.rdev()))
        })
        .ok_or_else(|| "no hidraw controller is currently connected".into())
}

/// Linux stores the device major in two fields within `dev_t`; this matches
/// `MAJOR(dev)` in the kernel rather than the legacy 12-bit-only form.
fn device_major(rdev: u64) -> u32 {
    (((rdev >> 8) & 0x0fff) | ((rdev >> 32) & 0xffff_f000)) as u32
}

#[cfg(test)]
mod tests {
    use super::device_major;

    #[test]
    fn extracts_linux_device_major() {
        assert_eq!(device_major(244 << 8), 244);
        assert_eq!(device_major((0x12_000_u64 << 32) | (0x345 << 8)), 0x1_2345);
    }
}
