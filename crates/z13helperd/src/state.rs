use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use z13helper_core::protocol::{ApplyRequest, DaemonState};

pub const STATE_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 1024 * 1024;

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
                let corrupt = self.path.with_extension("json.corrupt");
                fs::rename(&self.path, &corrupt).map_err(|rename_error| {
                    StateLoadError::Corrupt(format!(
                        "state is corrupt ({error}) and could not be preserved at {}: {rename_error}",
                        corrupt.display()
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
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temporary = self
            .path
            .with_extension(format!("json.{}-{nonce}.tmp", std::process::id()));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&temporary)
            .map_err(|error| format!("create {}: {error}", temporary.display()))?;
        serde_json::to_writer_pretty(&mut file, state)
            .map_err(|error| format!("serialize state: {error}"))?;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("protect {}: {error}", temporary.display()))?;
        file.write_all(b"\n")
            .and_then(|_| file.sync_all())
            .map_err(|error| format!("sync {}: {error}", temporary.display()))?;
        fs::rename(&temporary, &self.path)
            .map_err(|error| format!("replace {}: {error}", self.path.display()))?;
        if let Ok(directory) = fs::File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "z13helper-state-{name}-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
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
        fs::write(&path, "{").unwrap();
        let store = StateStore::new(&path);
        assert!(store.load().is_err());
        assert!(path.with_extension("json.corrupt").exists());
        let _ = fs::remove_file(path.with_extension("json.corrupt"));
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
}
