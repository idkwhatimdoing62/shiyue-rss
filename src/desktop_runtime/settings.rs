//! Versioned desktop settings and path resolution.
//!
//! The GUI never receives a settings path or a persistence primitive. The
//! production Adapter writes a same-directory temporary file, flushes it, and
//! atomically replaces the active TOML before publishing the new snapshot.

use anyhow::{Context, Result};
use directories::ProjectDirs;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::{CURRENT_SETTINGS_VERSION, Config};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub(crate) struct Paths {
    pub data_dir: PathBuf,
    pub config_file: PathBuf,
    pub db_file: PathBuf,
    pub log_file: PathBuf,
    pub image_cache_dir: PathBuf,
    pub backup_dir: PathBuf,
}

impl Paths {
    pub(crate) fn resolve() -> Result<Self> {
        if cfg!(debug_assertions)
            && let Some(root) = std::env::var_os("SHIYUE_TEST_ROOT")
        {
            let root = PathBuf::from(root);
            return Self::under(root.join("config"), root.join("data"));
        }
        // Preserve the historical `rrss` identity so upgrades keep using the
        // user's existing subscriptions and local library.
        let project = ProjectDirs::from("", "", "rrss").context("无法确定用户目录")?;
        Self::under(
            project.config_dir().to_path_buf(),
            project.data_local_dir().to_path_buf(),
        )
    }

    fn under(config_dir: PathBuf, data_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&config_dir)?;
        fs::create_dir_all(&data_dir)?;
        let image_cache_dir = data_dir.join("image-cache");
        let backup_dir = data_dir.join("backups");
        fs::create_dir_all(&image_cache_dir)?;
        fs::create_dir_all(&backup_dir)?;
        Ok(Self {
            config_file: config_dir.join("config.toml"),
            db_file: data_dir.join("rrss.db"),
            log_file: data_dir.join("rrss.log"),
            data_dir,
            image_cache_dir,
            backup_dir,
        })
    }
}

pub(super) trait SettingsStore {
    fn load(&self) -> Result<Config>;
    fn save(&self, settings: &Config) -> Result<()>;
}

pub(super) struct AtomicTomlSettingsStore {
    path: PathBuf,
}

impl AtomicTomlSettingsStore {
    pub(super) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn decode(&self, text: &str) -> Result<Config> {
        let mut settings: Config = toml::from_str(text).context("解析 config.toml 失败")?;
        settings.validate()?;
        if settings.settings_version == 0 {
            settings.settings_version = CURRENT_SETTINGS_VERSION;
        }
        Ok(settings)
    }
}

impl SettingsStore for AtomicTomlSettingsStore {
    fn load(&self) -> Result<Config> {
        if self.path.exists() {
            return self.decode(&fs::read_to_string(&self.path)?);
        }
        let settings = Config::default();
        self.save(&settings)?;
        Ok(settings)
    }

    fn save(&self, settings: &Config) -> Result<()> {
        settings.validate()?;
        let encoded = toml::to_string_pretty(settings)?;
        atomic_replace(&self.path, encoded.as_bytes())
            .with_context(|| format!("保存配置失败：{}", self.path.display()))
    }
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("配置文件没有父目录")?;
    fs::create_dir_all(parent)?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("settings"),
        std::process::id(),
        sequence
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // SAFETY: both pointers refer to NUL-terminated UTF-16 buffers that remain
    // alive for the duration of the Windows call.
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        return Err(std::io::Error::last_os_error()).context("原子替换配置文件失败");
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination).context("原子替换配置文件失败")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn test_directory(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("shiyue-{label}-{}-{nonce}", std::process::id()))
    }

    struct MemorySettingsStore {
        value: RefCell<Config>,
        fail_save: Cell<bool>,
    }

    impl SettingsStore for MemorySettingsStore {
        fn load(&self) -> Result<Config> {
            Ok(self.value.borrow().clone())
        }

        fn save(&self, settings: &Config) -> Result<()> {
            if self.fail_save.get() {
                anyhow::bail!("injected save failure");
            }
            *self.value.borrow_mut() = settings.clone();
            Ok(())
        }
    }

    #[test]
    fn legacy_toml_is_upgraded_without_losing_values() {
        let store = AtomicTomlSettingsStore::new(PathBuf::from("unused"));
        let settings = store
            .decode("notifications = false\nui_scale_percent = 110")
            .unwrap();
        assert_eq!(settings.settings_version, CURRENT_SETTINGS_VERSION);
        assert!(!settings.notifications);
        assert_eq!(settings.ui_scale_percent, 110);
    }

    #[test]
    fn invalid_scale_and_future_version_fail_explicitly() {
        let store = AtomicTomlSettingsStore::new(PathBuf::from("unused"));
        assert!(store.decode("ui_scale_percent = 777").is_err());
        assert!(
            store
                .decode("settings_version = 999\nui_scale_percent = 100")
                .is_err()
        );
    }

    #[test]
    fn failed_save_does_not_publish_a_new_snapshot() {
        let original = Config::default();
        let store = MemorySettingsStore {
            value: RefCell::new(original.clone()),
            fail_save: Cell::new(true),
        };
        let mut changed = original.clone();
        changed.ui_scale_percent = 125;
        assert!(store.save(&changed).is_err());
        assert_eq!(
            store.load().unwrap().ui_scale_percent,
            original.ui_scale_percent
        );
    }

    #[test]
    fn serialized_settings_never_contain_credentials() {
        let text = toml::to_string(&Config::default()).unwrap();
        assert!(!text.contains("api_key"));
        assert!(!text.contains("sk-"));
    }

    #[test]
    fn atomic_store_round_trips_and_leaves_no_temporary_file() {
        let directory = test_directory("settings-roundtrip");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("config.toml");
        let store = AtomicTomlSettingsStore::new(path.clone());
        let settings = Config {
            ui_scale_percent: 125,
            ..Config::default()
        };
        store.save(&settings).unwrap();
        assert_eq!(store.load().unwrap().ui_scale_percent, 125);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn corrupt_config_is_reported_without_being_overwritten() {
        let directory = test_directory("settings-corrupt");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("config.toml");
        let original = "this is not valid toml = [";
        fs::write(&path, original).unwrap();
        let store = AtomicTomlSettingsStore::new(path.clone());
        assert!(store.load().is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        fs::remove_dir_all(&directory).unwrap();
    }
}
