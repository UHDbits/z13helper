//! Config load/save/migrate for z13-helper.
//!
//! Persists to `$XDG_CONFIG_HOME/z13-helper/config.json` (default
//! `~/.config/z13-helper/config.json`). Writes are atomic (temp + rename),
//! keep a `.bak`, and include a `version` field with a migration path.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::profile::{Base, Profile};

pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported config version {0}")]
    UnsupportedVersion(u32),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    pub version: u32,
    pub active_profile: String,
    pub auto_switch_on_power_source: bool,
    pub last_profile_on_ac: String,
    pub last_profile_on_battery: String,
    pub power_source_debounce_ms: u64,
    pub show_hud: bool,
    pub fan_clamp_to_grid: bool,
    pub profiles: Vec<Profile>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            active_profile: "balanced".into(),
            auto_switch_on_power_source: true,
            last_profile_on_ac: "turbo".into(),
            last_profile_on_battery: "silent".into(),
            power_source_debounce_ms: 2000,
            show_hud: true,
            fan_clamp_to_grid: true,
            profiles: builtin_profiles(),
        }
    }
}

pub fn builtin_profiles() -> Vec<Profile> {
    vec![
        Profile::builtin("silent", "Silent", Base::Quiet),
        Profile::builtin("balanced", "Balanced", Base::Balanced),
        Profile::builtin("turbo", "Turbo", Base::Performance),
    ]
}

impl Config {
    /// Default config path under XDG_CONFIG_HOME.
    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."));
                home.join(".config")
            });
        base.join("z13-helper").join("config.json")
    }

    pub fn load_or_default(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            let cfg = Self::default();
            cfg.save(path)?;
            return Ok(cfg);
        }
        match Self::load(path) {
            Ok(cfg) => Ok(cfg),
            Err(ConfigError::Json(_)) => {
                // Corrupt file: keep a .corrupt copy and reseeds.
                let corrupt = path.with_extension("json.corrupt");
                let _ = fs::rename(path, &corrupt);
                let cfg = Self::default();
                cfg.save(path)?;
                Ok(cfg)
            }
            Err(e) => Err(e),
        }
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path)?;
        let value: Value = serde_json::from_str(&text)?;
        let migrated = migrate(value)?;
        let mut cfg: Config = serde_json::from_value(migrated)?;
        cfg.ensure_builtins();
        Ok(cfg)
    }

    /// Atomic write via temp-file + rename, with `.bak` of the previous file.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if path.exists() {
            let bak = path.with_extension("json.bak");
            let _ = fs::copy(path, &bak);
        }
        let tmp = path.with_extension("json.tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            let text = serde_json::to_string_pretty(self)?;
            f.write_all(text.as_bytes())?;
            f.write_all(b"\n")?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Ensure the three built-ins exist (re-seed if a user deleted them).
    pub fn ensure_builtins(&mut self) {
        for builtin in builtin_profiles() {
            if !self.profiles.iter().any(|p| p.id == builtin.id) {
                self.profiles.insert(0, builtin);
            }
        }
        // Keep builtins first in Silent/Balanced/Turbo order.
        self.profiles.sort_by_key(|p| match p.id.as_str() {
            "silent" => 0,
            "balanced" => 1,
            "turbo" => 2,
            _ => 100,
        });
    }

    pub fn find(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut Profile> {
        self.profiles.iter_mut().find(|p| p.id == id)
    }

    pub fn active(&self) -> Option<&Profile> {
        self.find(&self.active_profile)
    }

    /// Create a custom profile by copying the currently active one.
    pub fn add_custom(&mut self) -> &Profile {
        let next_n = self
            .profiles
            .iter()
            .filter(|p| !p.builtin)
            .count()
            + 1;
        let source = self
            .active()
            .cloned()
            .unwrap_or_else(|| Profile::builtin("balanced", "Balanced", Base::Balanced));
        let id = format!("custom-{next_n}");
        let mut profile = source;
        profile.id = id.clone();
        profile.name = format!("Custom {next_n}");
        profile.builtin = false;
        self.profiles.push(profile);
        self.active_profile = id;
        self.profiles.last().unwrap()
    }

    pub fn rename(&mut self, id: &str, name: &str) -> bool {
        if let Some(p) = self.find_mut(id) {
            if p.builtin {
                return false;
            }
            p.name = name.into();
            return true;
        }
        false
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let Some(idx) = self.profiles.iter().position(|p| p.id == id) else {
            return false;
        };
        if self.profiles[idx].builtin {
            return false;
        }
        self.profiles.remove(idx);
        if self.active_profile == id {
            self.active_profile = "balanced".into();
        }
        true
    }

    pub fn set_active_for_power_source(&mut self, id: &str, on_battery: bool) {
        self.active_profile = id.into();
        if on_battery {
            self.last_profile_on_battery = id.into();
        } else {
            self.last_profile_on_ac = id.into();
        }
    }
}

fn migrate(mut value: Value) -> Result<Value, ConfigError> {
    let version = value
        .get("version")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;

    if version > CONFIG_VERSION {
        return Err(ConfigError::UnsupportedVersion(version));
    }

    // v0 (missing version) → v1: inject defaults for any missing keys.
    if version < 1 {
        let defaults = serde_json::to_value(Config::default())?;
        if let (Value::Object(dst), Value::Object(src)) = (&mut value, defaults) {
            for (k, v) in src {
                dst.entry(k).or_insert(v);
            }
            dst.insert("version".into(), Value::from(CONFIG_VERSION));
        }
    }

    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("z13-helper-cfg-{name}-{nanos}.json"))
    }

    #[test]
    fn default_has_three_builtins() {
        let cfg = Config::default();
        assert_eq!(cfg.profiles.len(), 3);
        assert!(cfg.find("silent").unwrap().builtin);
        assert!(!cfg.find("silent").unwrap().apply_power_limits);
        assert_eq!(cfg.find("silent").unwrap().pl1_spl, 40);
        assert_eq!(cfg.find("balanced").unwrap().pl1_spl, 52);
        assert_eq!(cfg.find("turbo").unwrap().pl1_spl, 70);
    }

    #[test]
    fn roundtrip_save_load() {
        let path = tmp_path("roundtrip");
        let mut cfg = Config::default();
        cfg.active_profile = "turbo".into();
        cfg.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.active_profile, "turbo");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn save_creates_bak() {
        let path = tmp_path("bak");
        let cfg = Config::default();
        cfg.save(&path).unwrap();
        let mut cfg2 = Config::default();
        cfg2.active_profile = "silent".into();
        cfg2.save(&path).unwrap();
        assert!(path.with_extension("json.bak").exists());
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("json.bak"));
    }

    #[test]
    fn corrupt_file_reseeds() {
        let path = tmp_path("corrupt");
        fs::write(&path, b"{not json!!!").unwrap();
        let cfg = Config::load_or_default(&path).unwrap();
        assert_eq!(cfg.profiles.len(), 3);
        assert!(path.with_extension("json.corrupt").exists());
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("json.corrupt"));
    }

    #[test]
    fn migrate_v0_injects_version() {
        let raw = serde_json::json!({
            "active_profile": "silent",
            "profiles": []
        });
        let migrated = migrate(raw).unwrap();
        assert_eq!(migrated["version"], 1);
        assert_eq!(migrated["active_profile"], "silent");
        // Empty profiles get builtins via ensure_builtins on load.
        let mut cfg: Config = serde_json::from_value(migrated).unwrap();
        cfg.ensure_builtins();
        assert_eq!(cfg.profiles.len(), 3);
    }

    #[test]
    fn add_rename_remove_custom() {
        let mut cfg = Config::default();
        let p = cfg.add_custom();
        assert_eq!(p.name, "Custom 1");
        assert!(!p.builtin);
        let id = p.id.clone();
        assert!(cfg.rename(&id, "Gaming"));
        assert_eq!(cfg.find(&id).unwrap().name, "Gaming");
        assert!(!cfg.rename("balanced", "Nope"));
        assert!(cfg.remove(&id));
        assert!(!cfg.remove("silent"));
        assert_eq!(cfg.active_profile, "balanced");
    }

    #[test]
    fn future_version_rejected() {
        let raw = serde_json::json!({"version": 99});
        assert!(matches!(
            migrate(raw),
            Err(ConfigError::UnsupportedVersion(99))
        ));
    }
}
