use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::Converter;

/// Identifies one conversion result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey(blake3::Hash);

impl CacheKey {
    pub fn new(converter: &dyn Converter, input: &[u8]) -> Self {
        let mut h = blake3::Hasher::new();
        h.update(converter.id().as_bytes());
        h.update(&[0]);
        h.update(&converter.version().to_le_bytes());
        h.update(input);
        CacheKey(h.finalize())
    }

    pub fn to_hex(&self) -> String {
        self.0.to_hex().to_string()
    }
}

/// Content-addressed store under the user's cache directory.
#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
}

impl Cache {
    /// Opens (creating) a cache at `root`. Refuses locations inside the repo.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        victory_core::ensure_outside_repo(&root)?;
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(Cache { root })
    }

    pub fn path_for(&self, key: &CacheKey) -> PathBuf {
        let hex = key.to_hex();
        self.root.join(&hex[..2]).join(&hex[2..])
    }

    /// Returns the cached output for `input`, converting it first if needed.
    pub fn get_or_convert(&self, converter: &dyn Converter, input: &[u8]) -> Result<PathBuf> {
        let key = CacheKey::new(converter, input);
        let path = self.path_for(&key);
        if path.is_file() {
            tracing::trace!(converter = converter.id(), key = %key.to_hex(), "cache hit");
            return Ok(path);
        }
        let output = converter
            .convert(input)
            .with_context(|| format!("converter {} v{}", converter.id(), converter.version()))?;
        let dir = path.parent().expect("hashed path has a parent");
        std::fs::create_dir_all(dir)?;
        // Write-then-rename so readers never see partial output.
        let (tmp, mut file) = create_staging_file(dir)?;
        let written = file.write_all(&output);
        drop(file);
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        match std::fs::rename(&tmp, &path) {
            Ok(()) => Ok(path),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                // Another writer/reader holding the published file on Windows can
                // block replacement; the complete target is already on disk.
                if path.is_file() {
                    Ok(path)
                } else {
                    Err(e.into())
                }
            }
        }
    }
}

fn create_staging_file(dir: &Path) -> std::io::Result<(PathBuf, std::fs::File)> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    for attempt in 0..64 {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".tmp-{pid}-{n}"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt < 63 => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!("attempt 63 returns on the final iteration")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Upper {
        version: u32,
        runs: AtomicUsize,
    }

    impl Converter for Upper {
        fn id(&self) -> &'static str {
            "test.upper"
        }
        fn version(&self) -> u32 {
            self.version
        }
        fn convert(&self, input: &[u8]) -> Result<Vec<u8>> {
            self.runs.fetch_add(1, Ordering::Relaxed);
            Ok(input.to_ascii_uppercase())
        }
    }

    #[test]
    fn converts_once_then_hits() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let a = cache.get_or_convert(&conv, b"abc").unwrap();
        let b = cache.get_or_convert(&conv, b"abc").unwrap();
        assert_eq!(a, b);
        assert_eq!(std::fs::read(&a).unwrap(), b"ABC");
        assert_eq!(conv.runs.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn failed_publication_removes_the_staging_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let path = cache.path_for(&CacheKey::new(&conv, b"abc"));
        std::fs::create_dir_all(&path).unwrap();

        assert!(cache.get_or_convert(&conv, b"abc").is_err());
        assert!(path.is_dir());
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn version_bump_invalidates() {
        let v1 = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let v2 = Upper {
            version: 2,
            runs: AtomicUsize::new(0),
        };
        assert_ne!(CacheKey::new(&v1, b"x"), CacheKey::new(&v2, b"x"));
    }

    #[test]
    fn refuses_cache_inside_repo() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("cache");
        assert!(Cache::open(inside).is_err());
    }

    #[test]
    fn concurrent_winner_holding_open_file_still_succeeds() {
        use std::sync::Mutex;

        struct RacyWinner {
            target: PathBuf,
            held: Mutex<Option<std::fs::File>>,
        }
        impl Converter for RacyWinner {
            fn id(&self) -> &'static str {
                "test.upper"
            }
            fn version(&self) -> u32 {
                1
            }
            fn convert(&self, input: &[u8]) -> Result<Vec<u8>> {
                let out = input.to_ascii_uppercase();
                std::fs::create_dir_all(self.target.parent().unwrap()).unwrap();
                std::fs::write(&self.target, &out).unwrap();
                let mut opts = std::fs::OpenOptions::new();
                opts.read(true);
                #[cfg(windows)]
                {
                    use std::os::windows::fs::OpenOptionsExt;
                    opts.share_mode(1); // FILE_SHARE_READ (without FILE_SHARE_DELETE)
                }
                *self.held.lock().unwrap() = Some(opts.open(&self.target).unwrap());
                Ok(out)
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let probe = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let target = cache.path_for(&CacheKey::new(&probe, b"abc"));
        let conv = RacyWinner {
            target: target.clone(),
            held: Mutex::new(None),
        };
        let path = cache.get_or_convert(&conv, b"abc").unwrap();
        assert_eq!(path, target);
        drop(conv.held.lock().unwrap().take());
        assert_eq!(std::fs::read(&path).unwrap(), b"ABC");
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn concurrent_conversions_of_the_same_key_all_succeed() {
        use std::sync::{Arc, Barrier};

        let tmp = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(tmp.path().join("cache")).unwrap());
        let conv = Arc::new(Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        });
        let barrier = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let cache = cache.clone();
                let conv = conv.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    cache.get_or_convert(conv.as_ref(), b"shared").unwrap()
                })
            })
            .collect();

        let paths: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        for path in &paths {
            assert_eq!(path, &paths[0]);
            assert_eq!(std::fs::read(path).unwrap(), b"SHARED");
        }
        assert_eq!(
            std::fs::read_dir(paths[0].parent().unwrap())
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn stale_staging_file_does_not_fail_publication() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let path = cache.path_for(&CacheKey::new(&conv, b"stale"));
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        let pid = std::process::id();
        for n in 0..32u64 {
            std::fs::write(dir.join(format!(".tmp-{pid}-{n}")), b"stale").unwrap();
        }
        let published = cache.get_or_convert(&conv, b"stale").unwrap();
        assert_eq!(published, path);
        assert_eq!(std::fs::read(&published).unwrap(), b"STALE");
    }
}
