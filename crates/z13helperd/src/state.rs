use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use z13helper_core::config::{
    create_unique_sibling, open_nofollow, parent_dir, preserve_corrupt, read_limited,
    reject_unexpected_path, rename, sync_parent, temp_base, write_private,
};
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
        let path = self.path.display();
        let file = match open_nofollow(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PersistedState::default());
            }
            Err(error) => return Err(StateLoadError::Io(format!("read {path}: {error}"))),
        };
        let metadata = file
            .metadata()
            .map_err(|error| StateLoadError::Io(format!("stat {path}: {error}")))?;
        if !metadata.is_file() {
            return Err(StateLoadError::Io(format!(
                "state path {path} is not a regular file"
            )));
        }
        let data = read_limited(file, metadata.len(), MAX_STATE_BYTES)
            .map_err(|error| StateLoadError::Io(format!("read {path}: {error}")))?
            .ok_or_else(|| {
                StateLoadError::Io(format!("state {path} exceeds {MAX_STATE_BYTES} bytes"))
            })?;
        let state: PersistedState = match serde_json::from_slice(&data) {
            Ok(state) => state,
            Err(error) => {
                // Unpreserved bytes must not be overwritten, so this is Io.
                let corrupt = preserve_corrupt(&self.path, &data).map_err(|preserve_error| {
                    StateLoadError::Io(format!(
                        "state is corrupt ({error}) and could not be preserved beside {path}: {preserve_error}"
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
        reject_unexpected_path(&self.path, "state")
            .map_err(|error| format!("inspect {}: {error}", self.path.display()))?;
        let mut bytes = serde_json::to_vec_pretty(state)
            .map_err(|error| format!("serialize state: {error}"))?;
        bytes.push(b'\n');
        let temporary_base = temp_base(&self.path);
        let (temporary, mut file) = create_unique_sibling(&temporary_base)
            .map_err(|error| format!("create {}: {error}", temporary_base.display()))?;
        // Failed unique candidates are intentionally left for diagnostics; a
        // pathname unlink here would have a TOCTOU deletion race.
        write_private(&mut file, &bytes)
            .map_err(|error| format!("write {}: {error}", temporary.display()))?;
        drop(file);
        rename(&temporary, &self.path)
            .map_err(|error| format!("replace {}: {error}", self.path.display()))?;
        sync_parent(&self.path).map_err(|error| format!("sync {}: {error}", parent.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use z13helper_core::config::{FailurePoint, fail_once_at};

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
    fn corrupt_state_is_preserved_and_collisions_keep_all_old_samples_and_bytes() {
        let path = path("corrupt-collision");
        let first_sample = b"first old sample";
        let second_sample = b"second corrupt state";
        let corrupt = path.with_extension("json.corrupt");
        let store = StateStore::new(&path);

        // With nothing to collide with, the copy lands at `.corrupt` and the
        // original stays in place.
        fs::write(&path, first_sample).unwrap();
        assert!(store.load().is_err());
        assert_eq!(fs::read(&corrupt).unwrap(), first_sample);
        assert_eq!(
            fs::metadata(&corrupt).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read(&path).unwrap(), first_sample);

        fs::write(&path, second_sample).unwrap();
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
