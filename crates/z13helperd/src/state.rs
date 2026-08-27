use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use z13helper_core::protocol::{ApplyRequest, DaemonState};

pub const STATE_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 1024 * 1024;
static SIBLING_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FailurePoint {
    Write,
    FileSync,
    Rename,
    ParentSync,
}

#[cfg(test)]
thread_local! {
    static FAILURE_POINT: std::cell::Cell<Option<FailurePoint>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn take_failure(point: FailurePoint) -> bool {
    FAILURE_POINT.with(|slot| {
        if slot.get() == Some(point) {
            slot.set(None);
            true
        } else {
            false
        }
    })
}

#[cfg(test)]
struct FailureGuard;

#[cfg(test)]
impl Drop for FailureGuard {
    fn drop(&mut self) {
        FAILURE_POINT.with(|slot| slot.set(None));
    }
}

#[cfg(test)]
fn fail_once_at(point: FailurePoint) -> FailureGuard {
    FAILURE_POINT.with(|slot| slot.set(Some(point)));
    FailureGuard
}

fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn sibling_candidate(base: &Path, suffix: u64) -> std::io::Result<PathBuf> {
    let name = base.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} has no file name", base.display()),
        )
    })?;
    let name = name.to_string_lossy();
    let candidate = if suffix == 0 {
        name.into_owned()
    } else {
        format!("{name}.{suffix}")
    };
    Ok(base.with_file_name(candidate))
}

fn write_all(file: &mut fs::File, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(test)]
    if take_failure(FailurePoint::Write) {
        return Err(std::io::Error::other("injected state write failure"));
    }
    file.write_all(bytes)
}

fn sync_file(file: &fs::File) -> std::io::Result<()> {
    #[cfg(test)]
    if take_failure(FailurePoint::FileSync) {
        return Err(std::io::Error::other("injected state file sync failure"));
    }
    file.sync_all()
}

fn rename(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if take_failure(FailurePoint::Rename) {
        return Err(std::io::Error::other("injected state rename failure"));
    }
    fs::rename(from, to)
}

fn create_unique_sibling(base: &Path) -> std::io::Result<(PathBuf, fs::File)> {
    for suffix in 0..10_000_u64 {
        let candidate = sibling_candidate(base, suffix)?;
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&candidate);
        match file {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("no available sibling name for {}", base.display()),
    ))
}

fn write_unique_sibling(base: &Path, bytes: &[u8]) -> std::io::Result<PathBuf> {
    let (candidate, mut file) = create_unique_sibling(base)?;
    let result = (|| {
        write_all(&mut file, bytes)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        sync_file(&file)
    })();
    drop(file);
    // Do not unlink by pathname after failure: a concurrent writer could have
    // replaced that name before cleanup, turning cleanup into data loss.
    result?;
    Ok(candidate)
}

fn sync_parent(path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if take_failure(FailurePoint::ParentSync) {
        return Err(std::io::Error::other("injected state parent sync failure"));
    }
    fs::File::open(parent_dir(path))?.sync_all()
}

fn temp_base(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "state".into());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = SIBLING_COUNTER.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(
        ".{name}.{}-{now}-{sequence}.tmp",
        std::process::id()
    ))
}

fn reject_unexpected_state_path(path: &Path) -> std::io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("state path {} is a symlink", path.display()),
        )),
        Ok(metadata) if !metadata.is_file() => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("state path {} is not a regular file", path.display()),
        )),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[derive(Debug)]
pub enum StateLoadError {
    Io(String),
    Corrupt(String),
    Unsupported(u32),
}

impl std::fmt::Display for StateLoadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) | Self::Corrupt(error) => formatter.write_str(error),
            Self::Unsupported(version) => write!(
                formatter,
                "unsupported state version {version}; expected {STATE_VERSION}"
            ),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PersistedState {
    pub version: u32,
    pub desired: Option<ApplyRequest>,
    pub state: DaemonState,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            desired: None,
            state: DaemonState::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct StateStore {
    path: PathBuf,
}

impl Default for StateStore {
    fn default() -> Self {
        Self::new("/var/lib/z13helper/state.json")
    }
}

impl StateStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn load(&self) -> Result<PersistedState, StateLoadError> {
        let file = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&self.path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PersistedState::default());
            }
            Err(error) => {
                return Err(StateLoadError::Io(format!(
                    "read {}: {error}",
                    self.path.display()
                )));
            }
        };
        let metadata = file.metadata().map_err(|error| {
            StateLoadError::Io(format!("stat {}: {error}", self.path.display()))
        })?;
        if !metadata.is_file() {
            return Err(StateLoadError::Io(format!(
                "state path {} is not a regular file",
                self.path.display()
            )));
        }
        if metadata.len() > MAX_STATE_BYTES {
            return Err(StateLoadError::Io(format!(
                "state {} exceeds {MAX_STATE_BYTES} bytes",
                self.path.display()
            )));
        }
        let mut data = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_STATE_BYTES + 1)
            .read_to_end(&mut data)
            .map_err(|error| {
                StateLoadError::Io(format!("read {}: {error}", self.path.display()))
            })?;
        if data.len() as u64 > MAX_STATE_BYTES {
            return Err(StateLoadError::Io(format!(
                "state {} exceeds {MAX_STATE_BYTES} bytes",
                self.path.display()
            )));
        }
        let state: PersistedState = match serde_json::from_slice(&data) {
            Ok(state) => state,
            Err(error) => {
                let corrupt_base = self.path.with_extension("json.corrupt");
                let corrupt = write_unique_sibling(&corrupt_base, &data).and_then(|corrupt| {
                    sync_parent(&self.path)?;
                    Ok(corrupt)
                });
                let corrupt = corrupt.map_err(|preserve_error| {
                    StateLoadError::Corrupt(format!(
                        "state is corrupt ({error}) and could not be preserved beside {}: {preserve_error}",
                        self.path.display()
                    ))
                })?;
                return Err(StateLoadError::Corrupt(format!(
                    "state was corrupt and preserved at {}",
                    corrupt.display()
                )));
            }
        };
        if state.version != STATE_VERSION {
            return Err(StateLoadError::Unsupported(state.version));
        }
        Ok(state)
    }

    pub fn save(&self, state: &PersistedState) -> Result<(), String> {
        let parent = parent_dir(&self.path);
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
        reject_unexpected_state_path(&self.path)
            .map_err(|error| format!("inspect {}: {error}", self.path.display()))?;
        let bytes = serde_json::to_vec_pretty(state)
            .map_err(|error| format!("serialize state: {error}"))?;
        let temporary_base = temp_base(&self.path);
        let (temporary, mut file) = create_unique_sibling(&temporary_base)
            .map_err(|error| format!("create {}: {error}", temporary_base.display()))?;
        let result = (|| {
            write_all(&mut file, &bytes).and_then(|_| write_all(&mut file, b"\n"))?;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            sync_file(&file)
        })();
        // Failed unique candidates are intentionally left for diagnostics; a
        // pathname unlink here would have a TOCTOU deletion race.
        if let Err(error) = result {
            return Err(format!("write {}: {error}", temporary.display()));
        }
        drop(file);
        if let Err(error) = rename(&temporary, &self.path) {
            return Err(format!("replace {}: {error}", self.path.display()));
        }
        sync_parent(&self.path).map_err(|error| format!("sync {}: {error}", parent.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "z13helper-state-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        directory.join("state.json")
    }

    #[test]
    fn roundtrip_is_atomic_and_versioned() {
        let path = path("roundtrip");
        let store = StateStore::new(&path);
        let mut state = PersistedState::default();
        state.state.generation = 9;
        state.state.battery_limit = Some(80);
        state.state.battery_one_time_charge = true;
        store.save(&state).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let loaded = store.load().unwrap();
        assert_eq!(loaded.state.generation, 9);
        assert_eq!(loaded.state.battery_limit, Some(80));
        assert!(loaded.state.battery_one_time_charge);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn corrupt_state_is_preserved() {
        let path = path("corrupt");
        let original = b"{";
        fs::write(&path, original).unwrap();
        let store = StateStore::new(&path);
        assert!(store.load().is_err());
        assert_eq!(
            fs::read(path.with_extension("json.corrupt")).unwrap(),
            original
        );
        assert_eq!(
            fs::metadata(path.with_extension("json.corrupt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn unsupported_state_is_left_untouched() {
        let path = path("unsupported");
        let original = br##"{"version":99,"desired":null,"state":{}}"##;
        fs::write(&path, original).unwrap();
        assert!(matches!(
            StateStore::new(&path).load(),
            Err(StateLoadError::Unsupported(99))
        ));
        assert_eq!(fs::read(&path).unwrap(), original);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn existing_temp_file_is_never_overwritten() {
        let path = path("exclusive");
        let store = StateStore::new(&path);
        let stale = path.with_extension("json.tmp");
        fs::write(&stale, "keep me").unwrap();
        store.save(&PersistedState::default()).unwrap();
        assert_eq!(fs::read_to_string(&stale).unwrap(), "keep me");
        let _ = fs::remove_file(stale);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn corrupt_state_collision_keeps_all_old_samples_and_bytes() {
        let path = path("corrupt-collision");
        let first_sample = b"first old sample";
        let second_sample = b"second corrupt state";
        let corrupt = path.with_extension("json.corrupt");
        fs::write(&corrupt, first_sample).unwrap();
        fs::write(&path, second_sample).unwrap();
        let store = StateStore::new(&path);
        assert!(matches!(
            store.load(),
            Err(StateLoadError::Corrupt(message)) if message.contains("json.corrupt.1")
        ));
        assert_eq!(fs::read(&corrupt).unwrap(), first_sample);
        assert_eq!(
            fs::read(path.with_extension("json.corrupt.1")).unwrap(),
            second_sample
        );
        assert_eq!(
            fs::metadata(path.with_extension("json.corrupt.1"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(fs::read(&path).unwrap(), second_sample);

        let third_sample = b"third corrupt state";
        fs::write(&path, third_sample).unwrap();
        assert!(store.load().is_err());
        assert_eq!(fs::read(&corrupt).unwrap(), first_sample);
        assert_eq!(
            fs::read(path.with_extension("json.corrupt.1")).unwrap(),
            second_sample
        );
        assert_eq!(
            fs::read(path.with_extension("json.corrupt.2")).unwrap(),
            third_sample
        );
    }

    #[test]
    fn state_save_rejects_unexpected_directory() {
        let path = path("directory");
        fs::remove_file(&path).unwrap_or(());
        fs::create_dir(&path).unwrap();
        assert!(
            StateStore::new(&path)
                .save(&PersistedState::default())
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn state_save_rejects_symlink_without_touching_target() {
        let path = path("save-symlink");
        let target = path.with_file_name("target.json");
        fs::write(&target, b"target bytes").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(
            StateStore::new(&path)
                .save(&PersistedState::default())
                .is_err()
        );
        assert_eq!(fs::read(&target).unwrap(), b"target bytes");
        assert!(path.symlink_metadata().unwrap().file_type().is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_state_is_rejected_without_following_it() {
        let state_path = path("symlink");
        let target = path("symlink-target");
        fs::write(
            &target,
            serde_json::to_vec(&PersistedState::default()).unwrap(),
        )
        .unwrap();
        std::os::unix::fs::symlink(&target, &state_path).unwrap();
        assert!(matches!(
            StateStore::new(&state_path).load(),
            Err(StateLoadError::Io(_))
        ));
        let _ = fs::remove_file(state_path);
        let _ = fs::remove_file(target);
    }

    #[test]
    fn oversized_state_is_rejected_before_deserialization() {
        let path = path("oversized");
        fs::write(&path, vec![b' '; MAX_STATE_BYTES as usize + 1]).unwrap();
        assert!(matches!(
            StateStore::new(&path).load(),
            Err(StateLoadError::Io(_))
        ));
        let _ = fs::remove_file(path);
    }

    fn temp_files(path: &Path) -> Vec<PathBuf> {
        let prefix = format!(".{}.", path.file_name().unwrap().to_string_lossy());
        fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|candidate| {
                candidate
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
            })
            .collect()
    }

    #[test]
    fn save_failure_points_are_atomic_and_leave_private_temp_on_pre_rename_failure() {
        for point in [
            FailurePoint::Write,
            FailurePoint::FileSync,
            FailurePoint::Rename,
            FailurePoint::ParentSync,
        ] {
            let path = path(&format!("fault-{point:?}"));
            let replacement = PersistedState::default();
            let _failure = fail_once_at(point);
            assert!(StateStore::new(&path).save(&replacement).is_err());

            match point {
                FailurePoint::ParentSync => {
                    assert_eq!(
                        StateStore::new(&path).load().unwrap().version,
                        STATE_VERSION
                    );
                }
                _ => assert!(!path.exists()),
            }

            let temporary = temp_files(&path);
            if point == FailurePoint::ParentSync {
                assert!(temporary.is_empty());
            } else {
                assert_eq!(temporary.len(), 1, "failure point {point:?}");
                assert_eq!(
                    fs::metadata(&temporary[0]).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
    }

    #[test]
    fn concurrent_state_writers_only_publish_complete_states() {
        let path = path("concurrent");
        let mut writers = Vec::new();
        for generation in 0..5_u64 {
            let path = path.clone();
            writers.push(std::thread::spawn(move || {
                let mut state = PersistedState::default();
                state.state.generation = generation;
                StateStore::new(&path).save(&state)
            }));
        }
        for writer in writers {
            writer.join().unwrap().unwrap();
        }
        let loaded = StateStore::new(&path).load().unwrap();
        assert!(loaded.state.generation < 5);
    }
}
