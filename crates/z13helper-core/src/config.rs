//! Fresh v1 configuration for z13helper.
//!
//! Persists to `$XDG_CONFIG_HOME/z13helper/config.json` (default
//! `~/.config/z13helper/config.json`). Writes are atomic (temp + rename),
//! keep a `.bak`, and intentionally implement no schema migrations.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::profile::Profile;

pub const CONFIG_VERSION: u32 = 1;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid v1 config: {0}")]
    Invalid(String),
    #[error("could not preserve corrupt config: {0}")]
    Preservation(#[source] std::io::Error),
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
    #[serde(default)]
    pub panel_overdrive_always_on: bool,
    #[serde(default)]
    pub disable_high_power_fan_protection: bool,
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
            panel_overdrive_always_on: false,
            disable_high_power_fan_protection: false,
            profiles: builtin_profiles(),
        }
    }
}

pub fn builtin_profiles() -> Vec<Profile> {
    vec![
        Profile::builtin("silent", "Silent"),
        Profile::builtin("balanced", "Balanced"),
        Profile::builtin("turbo", "Turbo"),
    ]
}

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

fn open_regular(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        // Avoid blocking on a FIFO if another same-UID process swaps the
        // pathname before metadata can reject the non-regular file.
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    Ok(file)
}

fn read_regular(path: &Path) -> io::Result<Vec<u8>> {
    let file = open_regular(path)?;
    let metadata = file.metadata()?;
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config {} exceeds {MAX_CONFIG_BYTES} bytes", path.display()),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_CONFIG_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config {} exceeds {MAX_CONFIG_BYTES} bytes", path.display()),
        ));
    }
    Ok(bytes)
}

fn write_all(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    #[cfg(test)]
    if take_failure(FailurePoint::Write) {
        return Err(io::Error::other("injected config write failure"));
    }
    file.write_all(bytes)
}

fn sync_file(file: &File) -> io::Result<()> {
    #[cfg(test)]
    if take_failure(FailurePoint::FileSync) {
        return Err(io::Error::other("injected config file sync failure"));
    }
    file.sync_all()
}

fn rename(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(test)]
    if take_failure(FailurePoint::Rename) {
        return Err(io::Error::other("injected config rename failure"));
    }
    fs::rename(from, to)
}

fn sibling_candidate(base: &Path, suffix: u64) -> io::Result<PathBuf> {
    let name = base.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
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

fn create_unique_sibling(base: &Path) -> io::Result<(PathBuf, File)> {
    for suffix in 0..10_000_u64 {
        let candidate = sibling_candidate(base, suffix)?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&candidate);
        match file {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("no available sibling name for {}", base.display()),
    ))
}

fn backup_destination(base: &Path) -> io::Result<PathBuf> {
    for suffix in 0..10_000_u64 {
        let candidate = sibling_candidate(base, suffix)?;
        match fs::symlink_metadata(&candidate) {
            Ok(metadata) if metadata.is_file() => return Ok(candidate),
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(candidate),
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("no safe backup name for {}", base.display()),
    ))
}

fn write_unique_sibling(base: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
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

fn sync_parent(path: &Path) -> io::Result<()> {
    #[cfg(test)]
    if take_failure(FailurePoint::ParentSync) {
        return Err(io::Error::other("injected config parent sync failure"));
    }
    File::open(parent_dir(path))?.sync_all()
}

fn temp_base(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".into());
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

fn reject_unexpected_config_path(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("config path {} is a symlink", path.display()),
        )),
        Ok(metadata) if !metadata.is_file() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("config path {} is not a regular file", path.display()),
        )),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
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
        base.join("z13helper").join("config.json")
    }

    pub fn load_or_default(path: &Path) -> Result<Self, ConfigError> {
        if !reject_unexpected_config_path(path)? {
            let cfg = Self::default();
            cfg.save(path)?;
            return Ok(cfg);
        }
        let bytes = read_regular(path)?;
        match Self::parse_bytes(&bytes) {
            Ok(cfg) => Ok(cfg),
            Err(ConfigError::Json(_)) => {
                // Syntax corruption: preserve the original before replacing it.
                let corrupt_base = path.with_extension("json.corrupt");
                let _corrupt = write_unique_sibling(&corrupt_base, &bytes)
                    .and_then(|corrupt| {
                        sync_parent(path)?;
                        Ok(corrupt)
                    })
                    .map_err(ConfigError::Preservation)?;
                let cfg = Self::default();
                cfg.save(path)?;
                Ok(cfg)
            }
            Err(e) => Err(e),
        }
    }

    #[cfg(test)]
    fn load(path: &Path) -> Result<Self, ConfigError> {
        let bytes = read_regular(path)?;
        Self::parse_bytes(&bytes)
    }

    fn parse_bytes(bytes: &[u8]) -> Result<Self, ConfigError> {
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        let version = value.get("version").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        if version != CONFIG_VERSION {
            return Err(ConfigError::UnsupportedVersion(version));
        }
        let cfg: Config = serde_json::from_value(value)
            .map_err(|error| ConfigError::Invalid(error.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Atomic write via temp-file + rename, with `.bak` of the previous file.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        self.validate()?;
        let parent = parent_dir(path);
        fs::create_dir_all(parent)?;
        let had_existing = reject_unexpected_config_path(path)?;
        if had_existing {
            let current = read_regular(path)?;
            let backup_base = path.with_extension("json.bak");
            let backup_destination = backup_destination(&backup_base)?;
            let backup_temporary = write_unique_sibling(&temp_base(&backup_destination), &current)?;
            // Atomic replacement does not follow an attacker-selected
            // destination symlink and rotates one safe backup slot.
            rename(&backup_temporary, &backup_destination)?;
            sync_parent(path)?;
        }
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        let temporary = write_unique_sibling(&temp_base(path), &bytes).map_err(ConfigError::Io)?;
        if let Err(error) = rename(&temporary, path) {
            return Err(ConfigError::Io(error));
        }
        sync_parent(path)?;
        Ok(())
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
        let next_n = (1..)
            .find(|n| {
                let id = format!("custom-{n}");
                !self.profiles.iter().any(|profile| profile.id == id)
            })
            .unwrap();
        let source = self
            .active()
            .cloned()
            .unwrap_or_else(|| Profile::builtin("balanced", "Balanced"));
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
        if self.last_profile_on_ac == id {
            self.last_profile_on_ac = "balanced".into();
        }
        if self.last_profile_on_battery == id {
            self.last_profile_on_battery = "balanced".into();
        }
        true
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.version != CONFIG_VERSION {
            return Err(ConfigError::UnsupportedVersion(self.version));
        }

        let mut ids = std::collections::HashSet::new();
        for profile in &self.profiles {
            if profile.id.trim().is_empty() {
                return Err(ConfigError::Invalid("profile id must not be empty".into()));
            }
            if !ids.insert(profile.id.clone()) {
                return Err(ConfigError::Invalid(format!(
                    "duplicate profile id {:?}",
                    profile.id
                )));
            }
            let expected_builtin = matches!(profile.id.as_str(), "silent" | "balanced" | "turbo");
            if profile.builtin != expected_builtin {
                return Err(ConfigError::Invalid(format!(
                    "profile {:?} has an invalid builtin flag",
                    profile.id
                )));
            }
            profile.validate().map_err(ConfigError::Invalid)?;
        }

        for id in [
            &self.active_profile,
            &self.last_profile_on_ac,
            &self.last_profile_on_battery,
        ] {
            if !ids.contains(id.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "profile reference {:?} does not exist",
                    id
                )));
            }
        }
        for id in ["silent", "balanced", "turbo"] {
            if !ids.contains(id) {
                return Err(ConfigError::Invalid(format!(
                    "missing built-in profile {id:?}"
                )));
            }
        }
        Ok(())
    }

    pub fn set_active_for_power_source(&mut self, id: &str, on_battery: bool) {
        self.active_profile = id.into();
        if on_battery {
            self.last_profile_on_battery = id.into();
        } else {
            self.last_profile_on_ac = id.into();
        }
    }

    pub fn panel_overdrive_enabled(&self, on_battery: bool) -> bool {
        self.panel_overdrive_always_on || !on_battery
    }
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
        let directory = std::env::temp_dir().join(format!(
            "z13helper-cfg-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        directory.join("config.json")
    }

    #[test]
    fn defaults_have_three_builtins_and_panel_policy() {
        let cfg = Config::default();
        assert_eq!(cfg.profiles.len(), 3);
        assert!(cfg.find("silent").unwrap().builtin);
        assert!(!cfg.find("silent").unwrap().apply_power_limits);
        assert_eq!(cfg.find("silent").unwrap().pl1_spl, 40);
        assert_eq!(cfg.find("balanced").unwrap().pl1_spl, 52);
        assert_eq!(cfg.find("turbo").unwrap().pl1_spl, 70);
        assert!(!cfg.panel_overdrive_always_on);
        assert!(cfg.panel_overdrive_enabled(false));
        assert!(!cfg.panel_overdrive_enabled(true));
        let cfg = Config {
            panel_overdrive_always_on: true,
            ..Config::default()
        };
        assert!(cfg.panel_overdrive_enabled(false));
        assert!(cfg.panel_overdrive_enabled(true));
    }

    #[test]
    fn roundtrip_save_load() {
        let path = tmp_path("roundtrip");
        let cfg = Config {
            active_profile: "turbo".into(),
            ..Config::default()
        };
        cfg.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.active_profile, "turbo");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn corrupt_file_reseeds() {
        let path = tmp_path("corrupt");
        fs::write(&path, b"{not json!!!").unwrap();
        let cfg = Config::load_or_default(&path).unwrap();
        assert_eq!(cfg.profiles.len(), 3);
        assert!(path.with_extension("json.corrupt").exists());
        assert_eq!(
            fs::metadata(path.with_extension("json.corrupt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("json.corrupt"));
    }

    #[test]
    fn corrupt_preservation_skips_existing_sibling_without_overwriting_bytes() {
        let path = tmp_path("corrupt-preservation-failure");
        let original = b"{not json!!!";
        fs::write(&path, original).unwrap();
        let existing = path.with_extension("json.corrupt");
        fs::write(&existing, b"keep this old sample").unwrap();
        Config::load_or_default(&path).unwrap();
        assert_eq!(fs::read(existing).unwrap(), b"keep this old sample");
        assert_eq!(
            fs::read(path.with_extension("json.corrupt.1")).unwrap(),
            original
        );
        assert_eq!(
            fs::metadata(path.with_extension("json.corrupt.1"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(Config::load(&path).is_ok());
    }

    #[test]
    fn invalid_v1_values_are_preserved_and_rejected() {
        for (name, key, value) in [
            (
                "invalid-v1",
                "active_profile",
                serde_json::Value::String("missing".into()),
            ),
            (
                "invalid-v1-profile",
                "profiles",
                serde_json::Value::from(94),
            ),
        ] {
            let path = tmp_path(name);
            let mut object = serde_json::to_value(Config::default()).unwrap();
            if key == "active_profile" {
                object[key] = value;
            } else {
                object["profiles"][0]["pl1_spl"] = value;
            }
            let original = serde_json::to_string(&object).unwrap();
            fs::write(&path, &original).unwrap();
            assert!(matches!(
                Config::load_or_default(&path),
                Err(ConfigError::Invalid(_))
            ));
            assert_eq!(fs::read_to_string(&path).unwrap(), original);
            let _ = fs::remove_file(path);
        }
    }

    #[test]
    fn unsupported_versions_are_rejected_without_touching_files() {
        for (name, original, version) in [
            ("no-migration", r#"{"profiles":[]}"#, 0),
            ("future", r#"{"version":99,"future_data":"keep me"}"#, 99),
        ] {
            let path = tmp_path(name);
            fs::write(&path, original).unwrap();
            assert!(
                matches!(Config::load_or_default(&path), Err(ConfigError::UnsupportedVersion(v)) if v == version)
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), original);
            let _ = fs::remove_file(path);
        }
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
    fn custom_ids_stay_unique_after_deletion() {
        let mut cfg = Config::default();
        let first = cfg.add_custom().id.clone();
        let second = cfg.add_custom().id.clone();
        cfg.remove(&first);
        assert_eq!(cfg.add_custom().id, first);
        assert_ne!(first, second);
    }

    #[test]
    fn removing_profile_repairs_remembered_references() {
        let mut cfg = Config::default();
        let id = cfg.add_custom().id.clone();
        cfg.last_profile_on_ac = id.clone();
        cfg.last_profile_on_battery = id.clone();
        assert!(cfg.remove(&id));
        assert_eq!(cfg.last_profile_on_ac, "balanced");
        assert_eq!(cfg.last_profile_on_battery, "balanced");
    }

    #[test]
    fn fixed_temp_collision_is_not_reused_or_overwritten() {
        let path = tmp_path("temp-collision");
        let stale = path.with_extension("json.tmp");
        fs::write(&stale, b"keep me").unwrap();
        Config::default().save(&path).unwrap();
        assert_eq!(fs::read(stale).unwrap(), b"keep me");
        assert!(Config::load(&path).is_ok());
    }

    #[test]
    fn backup_rotates_the_latest_previous_byte_sequence() {
        let path = tmp_path("backup-collision");
        let first = Config::default();
        let second = Config {
            active_profile: "silent".into(),
            ..Config::default()
        };
        let third = Config {
            active_profile: "turbo".into(),
            ..Config::default()
        };
        first.save(&path).unwrap();
        second.save(&path).unwrap();
        let second_bytes = fs::read(&path).unwrap();
        third.save(&path).unwrap();
        assert_eq!(
            fs::read(path.with_extension("json.bak")).unwrap(),
            second_bytes
        );
        assert_eq!(
            fs::metadata(path.with_extension("json.bak"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(!path.with_extension("json.bak.1").exists());
    }

    #[cfg(unix)]
    #[test]
    fn config_symlinks_are_rejected_without_following_or_replacing_them() {
        let path = tmp_path("symlink");
        let target = path.with_file_name("target.json");
        fs::write(&target, b"target bytes").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(matches!(Config::load(&path), Err(ConfigError::Io(_))));
        assert!(matches!(
            Config::default().save(&path),
            Err(ConfigError::Io(_))
        ));
        assert!(path.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read(target).unwrap(), b"target bytes");
    }

    #[cfg(unix)]
    #[test]
    fn config_fifo_is_rejected_without_blocking_for_a_writer() {
        use std::os::unix::ffi::OsStrExt;

        let path = tmp_path("fifo");
        let path_c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: path_c is a valid NUL-terminated path and mkfifo does not
        // retain the pointer after returning.
        assert_eq!(unsafe { libc::mkfifo(path_c.as_ptr(), 0o600) }, 0);
        assert!(matches!(Config::load(&path), Err(ConfigError::Io(_))));
        fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn backup_symlink_is_not_followed_or_overwritten() {
        let path = tmp_path("backup-symlink");
        Config::default().save(&path).unwrap();
        let target = path.with_file_name("backup-target");
        let backup = path.with_extension("json.bak");
        fs::write(&target, b"backup target bytes").unwrap();
        std::os::unix::fs::symlink(&target, &backup).unwrap();
        Config {
            active_profile: "silent".into(),
            ..Config::default()
        }
        .save(&path)
        .unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"backup target bytes");
        assert!(backup.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(path.with_extension("json.bak.1").exists());
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
            let path = tmp_path(&format!("fault-{point:?}"));
            let replacement = Config {
                active_profile: "turbo".into(),
                ..Config::default()
            };
            let _failure = fail_once_at(point);
            assert!(replacement.save(&path).is_err());

            match point {
                FailurePoint::ParentSync => {
                    assert_eq!(Config::load(&path).unwrap(), replacement);
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
    fn oversized_config_is_rejected_before_deserialization() {
        let path = tmp_path("oversized");
        fs::write(&path, vec![b' '; MAX_CONFIG_BYTES as usize + 1]).unwrap();
        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::Io(error)) if error.kind() == io::ErrorKind::InvalidData
        ));
    }

    #[test]
    fn concurrent_config_writers_only_publish_complete_configs() {
        let path = tmp_path("concurrent");
        Config::default().save(&path).unwrap();
        let mut writers = Vec::new();
        for active_profile in ["silent", "balanced", "turbo", "silent", "turbo"] {
            let path = path.clone();
            let active_profile = active_profile.to_owned();
            writers.push(std::thread::spawn(move || {
                Config {
                    active_profile,
                    ..Config::default()
                }
                .save(&path)
            }));
        }
        for writer in writers {
            writer.join().unwrap().unwrap();
        }
        let loaded = Config::load(&path).unwrap();
        assert!(matches!(
            loaded.active_profile.as_str(),
            "silent" | "balanced" | "turbo"
        ));
    }
}
