//! Production implementation of the Env trait using real system calls.
//!
//! This module is the ONLY place in the entire codebase that may use:
//! - `std::time::Instant`
//! - `std::fs`
//! - `std::thread`
//! - System randomness
//!
//! All other crates must go through the `Env` trait.

use crate::{Duration, Env, Error, File, Instant, JoinHandle, OpenOptions, Path, PathBuf, Result};
use parking_lot::Mutex;
use std::sync::Arc;

/// Production implementation of [`Env`] using real system calls.
///
/// This is the implementation used in production. It delegates to the
/// actual operating system for time, randomness, and file I/O.
///
/// # Randomness
///
/// Uses xorshift64, a fast non-cryptographic PRNG seeded from system
/// randomness at construction. This matches [`SimEnv`](crate::SimEnv)
/// for algorithm consistency. See ADR-001.
#[derive(Clone)]
pub struct RealEnv {
    /// The epoch for converting between std::time::Instant and our Instant.
    /// We use the instant at RealEnv creation as epoch 0.
    epoch: std::time::Instant,
    /// Random number generator state (xorshift64).
    rng_state: Arc<Mutex<u64>>,
}

impl RealEnv {
    /// Creates a new production environment.
    ///
    /// Seeds the internal RNG from system randomness (`/dev/urandom` on Unix,
    /// `BCryptGenRandom` on Windows).
    ///
    /// # Panics
    ///
    /// Panics if system randomness is unavailable (e.g., `/dev/urandom` missing).
    pub fn new() -> Self {
        let mut seed_bytes = [0u8; 8];
        getrandom(&mut seed_bytes).expect("failed to get system randomness");
        let seed = u64::from_le_bytes(seed_bytes);

        Self {
            epoch: std::time::Instant::now(),
            rng_state: Arc::new(Mutex::new(if seed == 0 { 1 } else { seed })),
        }
    }

    /// Creates a new production environment with a specific RNG seed.
    ///
    /// This is useful for testing but should not be used in production.
    pub fn with_seed(seed: u64) -> Self {
        Self {
            epoch: std::time::Instant::now(),
            rng_state: Arc::new(Mutex::new(if seed == 0 { 1 } else { seed })),
        }
    }

    fn instant_to_ours(&self, instant: std::time::Instant) -> Instant {
        let duration = instant.duration_since(self.epoch);
        Instant::from_nanos(duration.as_nanos() as u64)
    }
}

impl Default for RealEnv {
    fn default() -> Self {
        Self::new()
    }
}

/// Get random bytes from the OS.
///
/// Returns `Ok(())` on success, `Err` if system randomness is unavailable.
fn getrandom(buf: &mut [u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Read;
        let mut file = std::fs::File::open("/dev/urandom")?;
        file.read_exact(buf)?;
        Ok(())
    }
    #[cfg(windows)]
    {
        // Windows implementation using BCryptGenRandom
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        // Fallback using time and process id - not ideal but works
        // TODO: Use proper BCryptGenRandom via windows-sys crate
        let mut hasher = DefaultHasher::new();
        std::time::SystemTime::now().hash(&mut hasher);
        std::process::id().hash(&mut hasher);
        std::thread::current().id().hash(&mut hasher);
        let hash = hasher.finish();
        for (i, b) in buf.iter_mut().enumerate() {
            *b = ((hash >> ((i % 8) * 8)) & 0xff) as u8;
        }
        Ok(())
    }
}

/// xorshift64 random number generator.
///
/// Fast, non-cryptographic PRNG with period 2^64-1.
/// See ADR-001 for rationale.
fn xorshift64(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

impl Env for RealEnv {
    type File = RealFile;

    fn now(&self) -> Instant {
        self.instant_to_ours(std::time::Instant::now())
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn rand_u64(&self) -> u64 {
        let mut state = self.rng_state.lock();
        xorshift64(&mut state)
    }

    fn spawn<F, T>(&self, _f: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        // Real implementation deferred until something actually needs spawn.
        // When implemented, this will spawn a real OS thread.
        // See ADR-006 for rationale.
        unimplemented!("spawn not yet implemented - see ADR-006")
    }

    fn open(&self, path: &Path, opts: OpenOptions) -> Result<Self::File> {
        let mut std_opts = std::fs::OpenOptions::new();
        std_opts
            .read(opts.read)
            .write(opts.write)
            .create(opts.create)
            .create_new(opts.create_new)
            .truncate(opts.truncate)
            .append(opts.append);

        let file = std_opts
            .open(path)
            .map_err(|e| Error::from_io(e, Some(path.to_path_buf())))?;

        Ok(RealFile {
            inner: Arc::new(file),
            path: path.to_path_buf(),
        })
    }

    fn remove(&self, path: &Path) -> Result<()> {
        // Try remove_file first, then fall back to remove_dir_all.
        // This avoids TOCTOU: the previous is_dir() check could race
        // with another process creating/deleting the path.
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) => {
                // Check if the error indicates it's a directory.
                // EISDIR = 21 on macOS/Linux, 20 on some BSDs.
                // We also check for EPERM which some systems return for directories.
                let is_dir_error = match e.raw_os_error() {
                    Some(21) => true, // EISDIR on macOS/Linux
                    Some(20) => true, // EISDIR on some BSDs
                    Some(1) if cfg!(unix) => {
                        // EPERM - macOS returns this for directories in some cases
                        path.is_dir()
                    }
                    _ => false,
                };

                if is_dir_error || e.kind() == std::io::ErrorKind::PermissionDenied && path.is_dir()
                {
                    std::fs::remove_dir_all(path)
                        .map_err(|e| Error::from_io(e, Some(path.to_path_buf())))
                } else {
                    Err(Error::from_io(e, Some(path.to_path_buf())))
                }
            }
        }
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        std::fs::rename(from, to).map_err(|e| Error::from_io(e, Some(from.to_path_buf())))
    }

    fn list_dir(&self, path: &Path) -> Result<Vec<PathBuf>> {
        let entries =
            std::fs::read_dir(path).map_err(|e| Error::from_io(e, Some(path.to_path_buf())))?;

        let mut result = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| Error::from_io(e, Some(path.to_path_buf())))?;
            result.push(entry.path());
        }
        Ok(result)
    }

    fn create_dir_all(&self, path: &Path) -> Result<()> {
        std::fs::create_dir_all(path).map_err(|e| Error::from_io(e, Some(path.to_path_buf())))
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }
}

/// A real file handle.
pub struct RealFile {
    inner: Arc<std::fs::File>,
    path: PathBuf,
}

impl File for RealFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.inner
                .read_at(buf, offset)
                .map_err(|e| Error::from_io(e, Some(self.path.clone())))
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            self.inner
                .seek_read(buf, offset)
                .map_err(|e| Error::from_io(e, Some(self.path.clone())))
        }
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> Result<usize> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.inner
                .write_at(buf, offset)
                .map_err(|e| Error::from_io(e, Some(self.path.clone())))
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            self.inner
                .seek_write(buf, offset)
                .map_err(|e| Error::from_io(e, Some(self.path.clone())))
        }
    }

    fn sync(&self) -> Result<()> {
        self.inner
            .sync_all()
            .map_err(|e| Error::from_io(e, Some(self.path.clone())))
    }

    fn len(&self) -> Result<u64> {
        self.inner
            .metadata()
            .map(|m| m.len())
            .map_err(|e| Error::from_io(e, Some(self.path.clone())))
    }

    fn truncate(&self, len: u64) -> Result<()> {
        self.inner
            .set_len(len)
            .map_err(|e| Error::from_io(e, Some(self.path.clone())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_env_time_advances() {
        let env = RealEnv::new();
        let t1 = env.now();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let t2 = env.now();
        assert!(t2 > t1);
    }

    #[test]
    fn real_env_random_produces_different_values() {
        let env = RealEnv::new();
        let r1 = env.rand_u64();
        let r2 = env.rand_u64();
        // Very unlikely to be equal
        assert_ne!(r1, r2);
    }

    #[test]
    fn real_env_seeded_random_is_deterministic() {
        let env1 = RealEnv::with_seed(42);
        let env2 = RealEnv::with_seed(42);

        for _ in 0..100 {
            assert_eq!(env1.rand_u64(), env2.rand_u64());
        }
    }

    #[test]
    fn real_env_sleep_blocks() {
        let env = RealEnv::new();
        let t1 = env.now();
        env.sleep(Duration::from_millis(50));
        let t2 = env.now();
        // Should have advanced by at least 50ms
        assert!(t2.as_nanos() - t1.as_nanos() >= 50_000_000);
    }

    #[test]
    fn real_env_file_ops() {
        let env = RealEnv::new();
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join("cracked_db_test_file");

        // Clean up if exists
        let _ = env.remove(&test_file);

        // Create and write
        let file = env.open(&test_file, OpenOptions::write()).unwrap();
        file.write_all_at(b"hello world", 0).unwrap();
        file.sync().unwrap();

        // Read back
        let mut buf = [0u8; 11];
        file.read_exact_at(&mut buf, 0).unwrap();
        assert_eq!(&buf, b"hello world");

        // Check length
        assert_eq!(file.len().unwrap(), 11);

        // Clean up
        drop(file);
        env.remove(&test_file).unwrap();
    }

    #[test]
    fn real_env_remove_handles_files_and_dirs() {
        let env = RealEnv::new();
        let temp_dir = std::env::temp_dir();

        // Test removing a file
        let test_file = temp_dir.join("cracked_db_test_remove_file");
        let _ = env.remove(&test_file);
        let file = env.open(&test_file, OpenOptions::write()).unwrap();
        file.write_all_at(b"test", 0).unwrap();
        drop(file);
        assert!(env.exists(&test_file));
        env.remove(&test_file).unwrap();
        assert!(!env.exists(&test_file));

        // Test removing a directory
        let test_dir = temp_dir.join("cracked_db_test_remove_dir");
        let _ = env.remove(&test_dir);
        env.create_dir_all(&test_dir).unwrap();
        assert!(env.is_dir(&test_dir));
        env.remove(&test_dir).unwrap();
        assert!(!env.exists(&test_dir));
    }
}
