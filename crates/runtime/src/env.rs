//! The core Env trait that abstracts all nondeterminism.

use crate::{Duration, File, Instant, Path, PathBuf, Result};

/// Options for opening a file.
#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    /// Open for reading.
    pub read: bool,
    /// Open for writing.
    pub write: bool,
    /// Create the file if it doesn't exist.
    pub create: bool,
    /// Create a new file, failing if it already exists.
    pub create_new: bool,
    /// Truncate the file to zero length on open.
    pub truncate: bool,
    /// Append to the file (writes go to end).
    pub append: bool,
}

impl OpenOptions {
    /// Creates options for reading an existing file.
    pub fn read() -> Self {
        Self {
            read: true,
            ..Default::default()
        }
    }

    /// Creates options for writing to a file, creating it if necessary.
    pub fn write() -> Self {
        Self {
            read: true,
            write: true,
            create: true,
            ..Default::default()
        }
    }

    /// Creates options for creating a new file, failing if it exists.
    pub fn create_new() -> Self {
        Self {
            read: true,
            write: true,
            create_new: true,
            ..Default::default()
        }
    }

    /// Creates options for appending to a file.
    pub fn append() -> Self {
        Self {
            write: true,
            create: true,
            append: true,
            ..Default::default()
        }
    }
}

/// A handle to a spawned task.
///
/// Real implementation deferred; spawn is currently unused. See ADR-006.
///
/// When implemented:
/// - In `RealEnv`: wraps a real OS thread
/// - In `SimEnv`: wraps a cooperative task for deterministic scheduling
pub struct JoinHandle<T> {
    _phantom: std::marker::PhantomData<T>,
}

impl<T> JoinHandle<T> {
    /// Creates a new join handle.
    ///
    /// This is a placeholder; the real implementation will be added when
    /// something actually needs spawn().
    pub fn new() -> Self {
        Self {
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<T> Default for JoinHandle<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// The environment trait that abstracts all sources of nondeterminism.
///
/// This is the most important trait in the entire codebase. Every source of
/// nondeterminism—time, randomness, file I/O, task scheduling—must go through
/// this trait.
///
/// # Implementations
///
/// - [`RealEnv`](crate::RealEnv): Uses actual system calls for production.
/// - [`SimEnv`](crate::SimEnv): Deterministic, seedable implementation for testing.
///
/// # Rules
///
/// No code outside this crate may:
/// - Use `std::time::Instant::now()` or `std::time::SystemTime::now()`
/// - Use `std::fs` functions directly
/// - Use `std::thread::spawn` or `std::thread::sleep`
/// - Use `rand::random` or similar
/// - Use `tokio::time` or `tokio::fs` directly
///
/// All such operations must go through an `Env` implementation.
///
/// # Note on async
///
/// This trait is intentionally synchronous. See ADR-006 for rationale.
/// Concurrency in production uses real OS threads via RealEnv.
/// Concurrency in simulation is modeled by the driver explicitly interleaving
/// synchronous operations from a deterministic schedule.
pub trait Env: Send + Sync + 'static {
    /// The file type returned by this environment.
    type File: File + 'static;

    /// Returns the current instant in time.
    ///
    /// In `RealEnv`, this uses the system monotonic clock.
    /// In `SimEnv`, this returns a deterministic simulated time.
    fn now(&self) -> Instant;

    /// Sleeps for the specified duration.
    ///
    /// In `RealEnv`, this blocks the current thread.
    /// In `SimEnv`, this advances simulated time deterministically.
    fn sleep(&self, duration: Duration);

    /// Returns a random u64.
    ///
    /// In `RealEnv`, this uses a fast non-cryptographic RNG (xorshift64)
    /// seeded from system randomness at construction.
    /// In `SimEnv`, this uses a seeded deterministic RNG (xorshift64).
    fn rand_u64(&self) -> u64;

    /// Spawns a task to run concurrently.
    ///
    /// **Currently unimplemented.** See ADR-006.
    ///
    /// When implemented:
    /// - In `RealEnv`, this will spawn a real OS thread.
    /// - In `SimEnv`, this will schedule the task for deterministic interleaving.
    ///
    /// The closure `f` will be executed to completion. The returned JoinHandle
    /// can be used to wait for the result.
    fn spawn<F, T>(&self, f: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static;

    /// Opens a file with the given options.
    fn open(&self, path: &Path, opts: OpenOptions) -> Result<Self::File>;

    /// Removes a file or directory.
    fn remove(&self, path: &Path) -> Result<()>;

    /// Renames a file or directory.
    fn rename(&self, from: &Path, to: &Path) -> Result<()>;

    /// Lists the contents of a directory.
    fn list_dir(&self, path: &Path) -> Result<Vec<PathBuf>>;

    /// Creates a directory and all parent directories.
    fn create_dir_all(&self, path: &Path) -> Result<()>;

    /// Returns true if the path exists.
    fn exists(&self, path: &Path) -> bool;

    /// Returns true if the path is a directory.
    fn is_dir(&self, path: &Path) -> bool;

    /// Returns true if the path is a file.
    fn is_file(&self, path: &Path) -> bool;

    // Convenience methods with default implementations

    /// Fills the buffer with random bytes.
    fn fill_random(&self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let random = self.rand_u64().to_le_bytes();
            let len = chunk.len().min(8);
            chunk.copy_from_slice(&random[..len]);
        }
    }

    /// Returns a random u32.
    fn rand_u32(&self) -> u32 {
        self.rand_u64() as u32
    }

    /// Returns a random usize.
    fn rand_usize(&self) -> usize {
        self.rand_u64() as usize
    }

    /// Returns a random f64 in [0, 1).
    fn rand_f64(&self) -> f64 {
        (self.rand_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Returns a random value in [0, n).
    fn rand_range(&self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        // Rejection sampling to avoid modulo bias
        let threshold = (u64::MAX - n + 1) % n;
        loop {
            let r = self.rand_u64();
            if r >= threshold {
                return r % n;
            }
        }
    }
}
