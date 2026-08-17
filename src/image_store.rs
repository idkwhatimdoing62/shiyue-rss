//! Persistent content-addressed image cache.
//!
//! URLs are references only. Image bytes are stored once under their SHA-256
//! digest, so identical CDN responses share one local object.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

pub const DEFAULT_LIMIT_BYTES: u64 = 1024 * 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub objects: usize,
    pub references: usize,
    pub bytes: u64,
}

#[derive(Debug)]
pub struct ImageStore {
    objects: PathBuf,
    references: PathBuf,
    mutation: Mutex<()>,
}

impl ImageStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let store = Self {
            objects: root.join("objects"),
            references: root.join("refs"),
            mutation: Mutex::new(()),
        };
        fs::create_dir_all(&store.objects)?;
        fs::create_dir_all(&store.references)?;
        Ok(store)
    }

    pub fn get(&self, url: &str) -> Result<Option<Vec<u8>>> {
        let _guard = self.mutation.lock().unwrap_or_else(|err| err.into_inner());
        let reference = self.references.join(digest(url.as_bytes()));
        let Ok(hash) = fs::read_to_string(&reference) else {
            return Ok(None);
        };
        let hash = hash.trim();
        if !valid_digest(hash) {
            let _ = fs::remove_file(reference);
            return Ok(None);
        }
        let object = self.objects.join(hash);
        let Ok(bytes) = fs::read(&object) else {
            let _ = fs::remove_file(reference);
            return Ok(None);
        };
        if digest(&bytes) != hash {
            let _ = fs::remove_file(object);
            let _ = fs::remove_file(reference);
            return Ok(None);
        }
        // Rewriting the tiny reference updates its modification time, which is
        // the cache's portable least-recently-used signal.
        let _ = fs::write(&reference, hash);
        Ok(Some(bytes))
    }

    pub fn put(&self, url: &str, bytes: &[u8]) -> Result<String> {
        let _guard = self.mutation.lock().unwrap_or_else(|err| err.into_inner());
        if bytes.is_empty() {
            bail!("不能缓存空图片");
        }
        let hash = digest(bytes);
        let object = self.objects.join(&hash);
        if !object.exists() {
            atomic_write(&object, bytes)?;
        }
        atomic_write(
            &self.references.join(digest(url.as_bytes())),
            hash.as_bytes(),
        )?;
        Ok(hash)
    }

    pub fn stats(&self) -> Result<CacheStats> {
        Ok(CacheStats {
            objects: regular_files(&self.objects)?.len(),
            references: regular_files(&self.references)?.len(),
            bytes: regular_files(&self.objects)?
                .into_iter()
                .filter_map(|path| fs::metadata(path).ok().map(|meta| meta.len()))
                .sum(),
        })
    }

    /// Remove least-recently-used URL references, then sweep objects that are
    /// no longer reachable from any reference.
    pub fn prune_to(&self, limit: u64) -> Result<u64> {
        let _guard = self.mutation.lock().unwrap_or_else(|err| err.into_inner());
        let before = self.stats()?.bytes;
        let mut refs = self.reference_entries()?;
        refs.sort_by_key(|entry| entry.0);
        while referenced_bytes(&self.objects, &refs) > limit && !refs.is_empty() {
            let (_, path, _) = refs.remove(0);
            fs::remove_file(path)?;
        }
        self.sweep_unreferenced(&refs)?;
        Ok(before.saturating_sub(self.stats()?.bytes))
    }

    pub fn clear(&self) -> Result<u64> {
        let _guard = self.mutation.lock().unwrap_or_else(|err| err.into_inner());
        let before = self.stats()?.bytes;
        for directory in [&self.references, &self.objects] {
            for path in regular_files(directory)? {
                fs::remove_file(path)?;
            }
        }
        Ok(before)
    }

    fn reference_entries(&self) -> Result<Vec<(SystemTime, PathBuf, String)>> {
        let mut entries = Vec::new();
        for path in regular_files(&self.references)? {
            let hash = fs::read_to_string(&path).unwrap_or_default();
            let hash = hash.trim().to_owned();
            if !valid_digest(&hash) {
                let _ = fs::remove_file(path);
                continue;
            }
            let modified = fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            entries.push((modified, path, hash));
        }
        Ok(entries)
    }

    fn sweep_unreferenced(&self, refs: &[(SystemTime, PathBuf, String)]) -> Result<()> {
        let live = refs
            .iter()
            .map(|(_, _, hash)| hash.as_str())
            .collect::<HashSet<_>>();
        for path in regular_files(&self.objects)? {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if !live.contains(name) {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }
}

fn referenced_bytes(objects: &Path, refs: &[(SystemTime, PathBuf, String)]) -> u64 {
    refs.iter()
        .map(|(_, _, hash)| hash)
        .collect::<HashSet<_>>()
        .into_iter()
        .filter_map(|hash| fs::metadata(objects.join(hash)).ok().map(|meta| meta.len()))
        .sum()
}

fn regular_files(directory: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in
        fs::read_dir(directory).with_context(|| format!("读取目录失败：{}", directory.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            files.push(entry.path());
        }
    }
    Ok(files)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = path.with_extension(format!("tmp-{}-{sequence}", std::process::id()));
    fs::write(&temp, bytes)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(&temp, path).or_else(|error| {
        let _ = fs::remove_file(&temp);
        Err(error)
    })?;
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "shiyue-image-store-{name}-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn deduplicates_bytes_and_resolves_each_url() {
        let root = temp_root("dedupe");
        let store = ImageStore::open(&root).unwrap();
        store.put("https://a.example/x", b"same image").unwrap();
        store.put("https://b.example/y", b"same image").unwrap();
        assert_eq!(
            store.get("https://a.example/x").unwrap().unwrap(),
            b"same image"
        );
        assert_eq!(store.stats().unwrap().objects, 1);
        assert_eq!(store.stats().unwrap().references, 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prune_removes_unreferenced_objects() {
        let root = temp_root("prune");
        let store = ImageStore::open(&root).unwrap();
        store.put("https://a.example/x", b"first").unwrap();
        store.put("https://b.example/y", b"second image").unwrap();
        assert!(store.prune_to(0).unwrap() > 0);
        assert_eq!(store.stats().unwrap(), CacheStats::default());
        fs::remove_dir_all(root).unwrap();
    }
}
