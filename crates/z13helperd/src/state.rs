use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use z13helper_core::protocol::{ApplyRequest, DaemonState};

pub const STATE_VERSION: u32 = 1;

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

    pub fn load(&self) -> Result<PersistedState, String> {
        if !self.path.exists() {
            return Ok(PersistedState::default());
        }
        let data = fs::read(&self.path)
            .map_err(|error| format!("read {}: {error}", self.path.display()))?;
        let state: PersistedState = match serde_json::from_slice(&data) {
            Ok(state) => state,
            Err(error) => {
                let corrupt = self.path.with_extension("json.corrupt");
                fs::rename(&self.path, &corrupt).map_err(|rename_error| {
                    format!(
                        "state is corrupt ({error}) and could not be preserved at {}: {rename_error}",
                        corrupt.display()
                    )
                })?;
                return Err(format!(
                    "state was corrupt and preserved at {}",
                    corrupt.display()
                ));
            }
        };
        if state.version != STATE_VERSION {
            return Err(format!(
                "unsupported state version {}; expected {STATE_VERSION}",
                state.version
            ));
        }
        Ok(state)
    }

    pub fn save(&self, state: &PersistedState) -> Result<(), String> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
        let temporary = self.path.with_extension("json.tmp");
        let mut file = fs::File::create(&temporary)
            .map_err(|error| format!("create {}: {error}", temporary.display()))?;
        serde_json::to_writer_pretty(&mut file, state)
            .map_err(|error| format!("serialize state: {error}"))?;
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
        store.save(&state).unwrap();
        assert_eq!(store.load().unwrap().state.generation, 9);
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
}
