//! Consistent local backups with optional Windows user-bound protection.

use anyhow::{Context, Result, bail};
use chrono::Utc;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::db::Db;

const PROTECTED_HEADER: &[u8] = b"SHIYUE-DPAPI-1\0";
pub const DEFAULT_BACKUP_KEEP: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupProtection {
    Plain,
    WindowsUser,
}

#[derive(Debug, Clone)]
pub struct BackupEntry {
    pub path: PathBuf,
    pub size: u64,
    pub modified: SystemTime,
    pub protected: bool,
}

#[derive(Debug, Clone)]
pub struct BackupStore {
    directory: PathBuf,
}

impl BackupStore {
    pub fn open(directory: impl Into<PathBuf>) -> Result<Self> {
        let directory = directory.into();
        fs::create_dir_all(&directory)?;
        Ok(Self { directory })
    }

    pub fn create(&self, db: &Db, protection: BackupProtection) -> Result<BackupEntry> {
        let entry = self.create_unpruned(db, protection)?;
        self.prune_keep(DEFAULT_BACKUP_KEEP)?;
        Ok(entry)
    }

    fn create_unpruned(&self, db: &Db, protection: BackupProtection) -> Result<BackupEntry> {
        let stamp = Utc::now().format("%Y%m%d-%H%M%S-%3f");
        let temporary = self.directory.join(format!(".shiyue-{stamp}.tmp.db"));
        db.backup_to(&temporary)?;
        let final_path = match protection {
            BackupProtection::Plain => {
                let path = self.directory.join(format!("shiyue-{stamp}.db"));
                if let Err(error) = fs::rename(&temporary, &path) {
                    let _ = remove_temporary_database(&temporary);
                    return Err(error.into());
                }
                remove_database_sidecars(&temporary);
                path
            }
            BackupProtection::WindowsUser => {
                let protected_temp = self.directory.join(format!(".shiyue-{stamp}.tmp.sybak"));
                let path = self.directory.join(format!("shiyue-{stamp}.sybak"));
                let result = (|| -> Result<PathBuf> {
                    let plain = fs::read(&temporary)?;
                    let protected = protect_for_current_user(&plain)?;
                    let mut encoded = Vec::with_capacity(PROTECTED_HEADER.len() + protected.len());
                    encoded.extend_from_slice(PROTECTED_HEADER);
                    encoded.extend_from_slice(&protected);
                    fs::write(&protected_temp, encoded)?;
                    fs::rename(&protected_temp, &path)?;
                    Ok(path.clone())
                })();
                // A protected backup must never leave its temporary plaintext
                // copy behind, including when DPAPI or the final write fails.
                let plain_cleanup = remove_temporary_database(&temporary);
                if result.is_err() {
                    let _ = fs::remove_file(&protected_temp);
                    let _ = fs::remove_file(&path);
                }
                match (result, plain_cleanup) {
                    (Ok(final_path), Ok(())) => final_path,
                    (Ok(_), Err(cleanup)) => {
                        bail!("加密备份已生成，但明文临时文件清理失败：{cleanup}")
                    }
                    (Err(error), Ok(())) => return Err(error),
                    (Err(error), Err(cleanup)) => {
                        return Err(
                            error.context(format!("加密失败后，明文临时文件清理也失败：{cleanup}"))
                        );
                    }
                }
            }
        };
        backup_entry(final_path)
    }

    /// Restore only after a fresh unencrypted safety backup has completed.
    /// The returned entry is that rollback point.
    pub fn restore(&self, db: &mut Db, entry: &BackupEntry) -> Result<BackupEntry> {
        // Do not prune here: the selected source can itself be the oldest
        // backup. Pruning it before restore would make a valid choice vanish.
        let safety = self.create_unpruned(db, BackupProtection::Plain)?;
        let source = if entry.protected {
            let encoded = fs::read(&entry.path)?;
            let payload = encoded
                .strip_prefix(PROTECTED_HEADER)
                .context("加密备份格式无法识别")?;
            let plain = unprotect_for_current_user(payload)?;
            let temp = self.directory.join(format!(
                ".restore-{}-{}.tmp.db",
                std::process::id(),
                Utc::now().timestamp_millis()
            ));
            fs::write(&temp, plain)?;
            Some(temp)
        } else {
            None
        };
        let restore_path = source.as_deref().unwrap_or(&entry.path);
        let result = db.restore_from(restore_path);
        let cleanup = if let Some(temp) = source {
            remove_temporary_database(&temp)
        } else {
            remove_database_sidecars(&entry.path);
            Ok(())
        };
        match (result, cleanup) {
            (Ok(()), Ok(())) => Ok(safety),
            (Ok(()), Err(cleanup)) => {
                bail!("数据库已经恢复，但解密临时文件清理失败：{cleanup}")
            }
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(cleanup)) => {
                Err(error.context(format!("恢复失败后，解密临时文件清理也失败：{cleanup}")))
            }
        }
    }

    pub fn list(&self) -> Result<Vec<BackupEntry>> {
        let mut entries = Vec::new();
        for item in fs::read_dir(&self.directory)? {
            let path = item?.path();
            let extension = path.extension().and_then(|value| value.to_str());
            if matches!(extension, Some("db" | "sybak"))
                && !path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .is_some_and(|name| name.starts_with('.'))
            {
                entries.push(backup_entry(path)?);
            }
        }
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.modified));
        Ok(entries)
    }

    pub fn prune_keep(&self, keep: usize) -> Result<u64> {
        let entries = self.list()?;
        let mut removed = 0;
        for entry in entries.into_iter().skip(keep) {
            removed += entry.size;
            fs::remove_file(entry.path)?;
        }
        Ok(removed)
    }
}

fn backup_entry(path: PathBuf) -> Result<BackupEntry> {
    let metadata =
        fs::metadata(&path).with_context(|| format!("读取备份信息失败：{}", path.display()))?;
    Ok(BackupEntry {
        protected: path.extension().and_then(|value| value.to_str()) == Some("sybak"),
        size: metadata.len(),
        modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        path,
    })
}

fn database_sidecars(path: &Path) -> [PathBuf; 2] {
    let display = path.as_os_str().to_string_lossy();
    [
        PathBuf::from(format!("{display}-wal")),
        PathBuf::from(format!("{display}-shm")),
    ]
}

fn remove_database_sidecars(path: &Path) {
    for sidecar in database_sidecars(path) {
        let _ = fs::remove_file(sidecar);
    }
}

fn remove_temporary_database(path: &Path) -> std::io::Result<()> {
    remove_database_sidecars(path);
    fs::remove_file(path)
}

#[cfg(windows)]
fn protect_for_current_user(data: &[u8]) -> Result<Vec<u8>> {
    use std::ptr;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData,
    };

    let length = u32::try_from(data.len()).context("备份文件过大，无法交给 Windows 凭据保护")?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: length,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let ok = unsafe {
        CryptProtectData(
            &input,
            ptr::null(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        bail!(
            "Windows 用户凭据加密失败：{}",
            std::io::Error::last_os_error()
        );
    }
    let protected =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        LocalFree(output.pbData.cast());
    }
    Ok(protected)
}

#[cfg(windows)]
fn unprotect_for_current_user(data: &[u8]) -> Result<Vec<u8>> {
    use std::ptr;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
    };

    let length = u32::try_from(data.len()).context("加密备份过大")?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: length,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            ptr::null_mut(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        bail!(
            "无法用当前 Windows 用户解密备份：{}",
            std::io::Error::last_os_error()
        );
    }
    let plain =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        LocalFree(output.pbData.cast());
    }
    Ok(plain)
}

#[cfg(not(windows))]
fn protect_for_current_user(_data: &[u8]) -> Result<Vec<u8>> {
    bail!("当前平台暂不支持系统用户凭据加密")
}

#[cfg(not(windows))]
fn unprotect_for_current_user(_data: &[u8]) -> Result<Vec<u8>> {
    bail!("当前平台暂不支持系统用户凭据解密")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "shiyue-backup-{name}-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ))
    }

    #[test]
    fn plain_backup_restores_and_keeps_a_safety_copy() {
        let root = temp_root("plain");
        fs::create_dir_all(&root).unwrap();
        let mut db = Db::open(&root.join("live.db")).unwrap();
        db.add_feed("https://one.example/feed", 1).unwrap();
        let store = BackupStore::open(root.join("backups")).unwrap();
        let backup = store.create(&db, BackupProtection::Plain).unwrap();
        let second = db.add_feed("https://two.example/feed", 2).unwrap();
        let safety = store.restore(&mut db, &backup).unwrap();
        assert!(safety.path.exists());
        assert!(db.get_feed(second).is_err());
        drop(db);
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_user_protection_round_trips() {
        let protected = protect_for_current_user(b"private library").unwrap();
        assert_ne!(protected, b"private library");
        assert_eq!(
            unprotect_for_current_user(&protected).unwrap(),
            b"private library"
        );
    }

    #[cfg(windows)]
    #[test]
    fn encrypted_database_backup_restores_without_plaintext_temps() {
        let root = temp_root("encrypted");
        fs::create_dir_all(&root).unwrap();
        let mut db = Db::open(&root.join("live.db")).unwrap();
        db.add_feed("https://one.example/feed", 1).unwrap();
        let store = BackupStore::open(root.join("backups")).unwrap();
        let backup = store.create(&db, BackupProtection::WindowsUser).unwrap();
        let encoded = fs::read(&backup.path).unwrap();
        assert!(encoded.starts_with(PROTECTED_HEADER));
        assert!(!encoded.windows(16).any(|part| part == b"SQLite format 3\0"));
        assert!(
            fs::read_dir(root.join("backups")).unwrap().all(|item| !item
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with('.'))
        );

        let extra = db.add_feed("https://two.example/feed", 2).unwrap();
        store.restore(&mut db, &backup).unwrap();
        assert!(db.get_feed(extra).is_err());
        drop(db);
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }
}
