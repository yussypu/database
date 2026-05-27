//! Deterministic simulation environment for testing.
//!
//! This module provides [`SimEnv`], a fully deterministic implementation of the
//! [`Env`] trait. Given the same seed, it will produce byte-identical behavior
//! across any number of runs on any machine.
//!
//! # Key Properties
//!
//! - **Deterministic time**: Time only advances when explicitly told to.
//! - **Deterministic randomness**: RNG is seeded from the environment seed.
//! - **In-memory filesystem**: All file operations happen in memory.
//! - **Fault injection**: Deterministic faults via [`FaultConfig`](crate::FaultConfig).
//!
//! # Fault Injection
//!
//! SimEnv supports deterministic fault injection through [`FaultConfig`](crate::FaultConfig).
//! Use [`SimEnv::new_with_faults`] to enable faults. Every fault decision uses
//! the deterministic RNG, so same seed + same config = same fault sequence.
//!
//! # Note on async
//!
//! This module is intentionally synchronous. See ADR-006 for rationale.
//! Concurrency is modeled by the simulation driver explicitly interleaving
//! synchronous operations from a deterministic schedule.

use crate::sim_faults::{FaultConfig, FaultEvent};
use crate::{Duration, Env, Error, File, Instant, JoinHandle, OpenOptions, Path, PathBuf, Result};
use parking_lot::{Mutex, RwLock};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Configuration for the simulation environment.
#[derive(Debug, Clone)]
pub struct SimEnvConfig {
    /// The random seed for this simulation.
    pub seed: u64,
    /// Initial simulated time in nanoseconds.
    pub initial_time_nanos: u64,
}

impl Default for SimEnvConfig {
    fn default() -> Self {
        Self {
            seed: 0xDEADBEEF,
            initial_time_nanos: 0,
        }
    }
}

impl SimEnvConfig {
    /// Creates a new config with the given seed.
    pub fn with_seed(seed: u64) -> Self {
        Self {
            seed,
            ..Default::default()
        }
    }
}

/// A deterministic simulation environment.
///
/// This is the core of the simulation testing framework. Every operation is
/// deterministic given the same seed.
///
/// # Example
///
/// ```
/// use runtime::{SimEnv, SimEnvConfig, Env};
///
/// // Two environments with the same seed produce identical results
/// let env1 = SimEnv::new(SimEnvConfig::with_seed(42));
/// let env2 = SimEnv::new(SimEnvConfig::with_seed(42));
///
/// assert_eq!(env1.rand_u64(), env2.rand_u64());
/// assert_eq!(env1.now(), env2.now());
/// ```
#[derive(Clone)]
pub struct SimEnv {
    inner: Arc<SimEnvInner>,
}

struct SimEnvInner {
    /// Current simulated time in nanoseconds.
    /// Using Mutex instead of AtomicU64 for simplicity - simulation is single-threaded.
    current_time: Mutex<u64>,

    /// Random number generator state.
    rng_state: Mutex<u64>,

    /// The in-memory filesystem.
    filesystem: RwLock<SimFilesystem>,

    /// The original seed (for debugging/logging).
    seed: u64,

    /// Fault injection configuration.
    fault_config: FaultConfig,

    /// Log of all fault events that fired (deterministic ordering).
    fault_event_log: Mutex<Vec<FaultEvent>>,
}

/// The in-memory filesystem for simulation.
struct SimFilesystem {
    /// Maps path -> file data.
    /// BTreeMap for deterministic iteration order.
    files: BTreeMap<PathBuf, SimFileData>,
    /// Directories that exist (implicitly or explicitly).
    /// BTreeSet for deterministic iteration order.
    dirs: BTreeSet<PathBuf>,
    /// Total bytes written across all files (for disk_full tracking).
    total_bytes_written: u64,
    /// Whether disk_full has been triggered (sticky until recovery).
    disk_full_triggered: bool,
}

impl SimFilesystem {
    fn new() -> Self {
        let mut dirs = BTreeSet::new();
        dirs.insert(PathBuf::from("/"));
        Self {
            files: BTreeMap::new(),
            dirs,
            total_bytes_written: 0,
            disk_full_triggered: false,
        }
    }
}

/// Data for a single simulated file.
struct SimFileData {
    /// The file contents.
    data: Vec<u8>,
    /// How much of the file has been synced.
    /// In fault injection, unsynced data may be lost.
    synced_len: usize,
}

impl SimFileData {
    fn new() -> Self {
        Self {
            data: Vec::new(),
            synced_len: 0,
        }
    }
}

impl SimEnv {
    /// Creates a new simulation environment with the given configuration.
    ///
    /// All faults are disabled by default. Use [`SimEnv::new_with_faults`]
    /// to enable fault injection.
    pub fn new(config: SimEnvConfig) -> Self {
        Self::new_with_faults(config, FaultConfig::default())
    }

    /// Creates a new simulation environment with fault injection enabled.
    ///
    /// The `fault_config` controls which faults are enabled and their probabilities.
    /// All fault decisions use the deterministic RNG, so same seed + same config
    /// produces identical fault sequences.
    pub fn new_with_faults(config: SimEnvConfig, fault_config: FaultConfig) -> Self {
        let seed = if config.seed == 0 { 1 } else { config.seed };
        Self {
            inner: Arc::new(SimEnvInner {
                current_time: Mutex::new(config.initial_time_nanos),
                rng_state: Mutex::new(seed),
                filesystem: RwLock::new(SimFilesystem::new()),
                seed,
                fault_config,
                fault_event_log: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Returns the seed this environment was created with.
    pub fn seed(&self) -> u64 {
        self.inner.seed
    }

    /// Returns the fault configuration for this environment.
    pub fn fault_config(&self) -> &FaultConfig {
        &self.inner.fault_config
    }

    /// Returns a copy of all fault events that have fired.
    ///
    /// Events are ordered by the time they occurred. This log is deterministic
    /// given the same seed and fault configuration.
    pub fn fault_events(&self) -> Vec<FaultEvent> {
        self.inner.fault_event_log.lock().clone()
    }

    /// Records a fault event in the log.
    fn record_fault_event(&self, event: FaultEvent) {
        self.inner.fault_event_log.lock().push(event);
    }

    /// Returns the current simulated time as a Duration (for fault event timestamps).
    fn sim_time_as_duration(&self) -> Duration {
        Duration::from_nanos(*self.inner.current_time.lock())
    }

    /// Returns the total bytes written to the simulated filesystem.
    pub fn total_bytes_written(&self) -> u64 {
        self.inner.filesystem.read().total_bytes_written
    }

    /// Returns true if disk full has been triggered.
    pub fn is_disk_full(&self) -> bool {
        self.inner.filesystem.read().disk_full_triggered
    }

    /// Simulates recovery from disk full condition.
    ///
    /// Clears the disk_full_triggered flag so writes can succeed again.
    /// Does not clear total_bytes_written, so if threshold is still exceeded,
    /// the next write will trigger disk full again.
    pub fn simulate_disk_recovery(&self) {
        self.inner.filesystem.write().disk_full_triggered = false;
    }

    /// Resets total bytes written counter (for testing scenarios).
    pub fn reset_bytes_written(&self) {
        self.inner.filesystem.write().total_bytes_written = 0;
    }

    /// Advances simulated time by the given duration.
    pub fn advance_time(&self, duration: Duration) {
        let nanos = duration.as_nanos() as u64;
        let mut time = self.inner.current_time.lock();
        *time += nanos;
    }

    /// Sets the simulated time to a specific value.
    ///
    /// Use with caution - this can break causality assumptions.
    pub fn set_time(&self, instant: Instant) {
        let mut time = self.inner.current_time.lock();
        *time = instant.as_nanos();
    }

    /// Returns the current RNG state (for snapshot comparison in tests).
    pub fn rng_state(&self) -> u64 {
        *self.inner.rng_state.lock()
    }

    /// Returns a snapshot of the filesystem state for debugging.
    /// Returns BTreeMap for deterministic iteration order.
    pub fn filesystem_snapshot(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        let fs = self.inner.filesystem.read();
        fs.files
            .iter()
            .map(|(k, v)| (k.clone(), v.data.clone()))
            .collect()
    }

    /// Clears all files from the simulated filesystem.
    ///
    /// Useful for simulating a crash where unsynced data is lost.
    /// Also resets disk full state and total bytes written.
    pub fn clear_filesystem(&self) {
        let mut fs = self.inner.filesystem.write();
        fs.files.clear();
        fs.dirs.clear();
        fs.dirs.insert(PathBuf::from("/"));
        fs.total_bytes_written = 0;
        fs.disk_full_triggered = false;
    }

    /// Simulates a crash by truncating all files to their synced length.
    ///
    /// Any data written but not synced is lost.
    /// Does NOT clear disk full state - use `simulate_disk_recovery()` for that.
    pub fn simulate_crash(&self) {
        let mut fs = self.inner.filesystem.write();
        for file in fs.files.values_mut() {
            file.data.truncate(file.synced_len);
        }
    }

    /// Clears the fault event log.
    ///
    /// Useful when running multiple sub-tests in the same environment.
    pub fn clear_fault_events(&self) {
        self.inner.fault_event_log.lock().clear();
    }
}

/// xorshift64 random number generator.
fn xorshift64(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

impl Env for SimEnv {
    type File = SimFile;

    fn now(&self) -> Instant {
        let true_time = *self.inner.current_time.lock();

        // Check for clock skew fault
        let config = &self.inner.fault_config;
        if config.clock_skew_prob > 0.0 && config.clock_skew_max.as_nanos() > 0 {
            let roll = self.rand_f64();
            if roll < config.clock_skew_prob {
                // Skew is applied: randomly forward or backward
                let is_forward = self.rand_u64() % 2 == 0;
                let skew_nanos = self.rand_range(config.clock_skew_max.as_nanos() as u64);
                let skew = Duration::from_nanos(skew_nanos);

                // Record the fault event
                self.record_fault_event(FaultEvent::ClockSkew {
                    skew_magnitude: skew,
                    is_forward,
                    at_sim_time: Duration::from_nanos(true_time),
                });

                // Apply skew, clamping to avoid negative time
                let skewed_time = if is_forward {
                    true_time.saturating_add(skew_nanos)
                } else {
                    true_time.saturating_sub(skew_nanos)
                };

                return Instant::from_nanos(skewed_time);
            }
        }

        Instant::from_nanos(true_time)
    }

    fn sleep(&self, duration: Duration) {
        // Check for process pause fault
        let config = &self.inner.fault_config;
        let actual_duration =
            if config.process_pause_prob > 0.0 && config.process_pause_duration.as_nanos() > 0 {
                let roll = self.rand_f64();
                if roll < config.process_pause_prob {
                    let extended = duration + config.process_pause_duration;

                    // Record the fault event
                    self.record_fault_event(FaultEvent::ProcessPause {
                        requested: duration,
                        actual: extended,
                        at_sim_time: self.sim_time_as_duration(),
                    });

                    extended
                } else {
                    duration
                }
            } else {
                duration
            };

        // Advance simulated time
        self.advance_time(actual_duration);
    }

    fn rand_u64(&self) -> u64 {
        let mut state = self.inner.rng_state.lock();
        xorshift64(&mut state)
    }

    fn spawn<F, T>(&self, _f: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        // See ADR-006: spawn is intentionally unimplemented until needed.
        // When implemented, this will integrate with a deterministic scheduler
        // that interleaves task execution based on the seed.
        unimplemented!("spawn not yet implemented - see ADR-006")
    }

    fn open(&self, path: &Path, opts: OpenOptions) -> Result<Self::File> {
        let path = normalize_path(path);
        let mut fs = self.inner.filesystem.write();

        // Check if parent directory exists
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !fs.dirs.contains(parent) {
                return Err(Error::NotFound(parent.to_path_buf()));
            }
        }

        let exists = fs.files.contains_key(&path);

        if opts.create_new && exists {
            return Err(Error::AlreadyExists(path));
        }

        if !opts.create && !opts.create_new && !exists {
            return Err(Error::NotFound(path));
        }

        if !exists {
            fs.files.insert(path.clone(), SimFileData::new());
        }

        if opts.truncate {
            if let Some(file) = fs.files.get_mut(&path) {
                file.data.clear();
                file.synced_len = 0;
            }
        }

        Ok(SimFile {
            sim_env: self.clone(),
            path,
            append: opts.append,
        })
    }

    fn remove(&self, path: &Path) -> Result<()> {
        let path = normalize_path(path);
        let mut fs = self.inner.filesystem.write();

        if fs.files.remove(&path).is_some() {
            return Ok(());
        }

        if fs.dirs.remove(&path) {
            // Also remove all files in this directory
            fs.files.retain(|k, _| !k.starts_with(&path));
            fs.dirs.retain(|d| !d.starts_with(&path) || d == &path);
            return Ok(());
        }

        Err(Error::NotFound(path))
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        let from = normalize_path(from);
        let to = normalize_path(to);
        let mut fs = self.inner.filesystem.write();

        if let Some(data) = fs.files.remove(&from) {
            fs.files.insert(to, data);
            Ok(())
        } else {
            Err(Error::NotFound(from))
        }
    }

    fn list_dir(&self, path: &Path) -> Result<Vec<PathBuf>> {
        let path = normalize_path(path);
        let fs = self.inner.filesystem.read();

        if !fs.dirs.contains(&path) {
            return Err(Error::NotFound(path));
        }

        let mut entries = Vec::new();
        for file_path in fs.files.keys() {
            if let Some(parent) = file_path.parent() {
                if parent == path {
                    entries.push(file_path.clone());
                }
            }
        }
        for dir_path in &fs.dirs {
            if let Some(parent) = dir_path.parent() {
                if parent == path && dir_path != &path {
                    entries.push(dir_path.clone());
                }
            }
        }

        // Already sorted due to BTreeMap/BTreeSet, but sort again for consistency
        entries.sort();
        Ok(entries)
    }

    fn create_dir_all(&self, path: &Path) -> Result<()> {
        let path = normalize_path(path);
        let mut fs = self.inner.filesystem.write();

        // Add all parent directories
        let mut current = PathBuf::new();
        for component in path.components() {
            current.push(component);
            fs.dirs.insert(current.clone());
        }
        Ok(())
    }

    fn exists(&self, path: &Path) -> bool {
        let path = normalize_path(path);
        let fs = self.inner.filesystem.read();
        fs.files.contains_key(&path) || fs.dirs.contains(&path)
    }

    fn is_dir(&self, path: &Path) -> bool {
        let path = normalize_path(path);
        let fs = self.inner.filesystem.read();
        fs.dirs.contains(&path)
    }

    fn is_file(&self, path: &Path) -> bool {
        let path = normalize_path(path);
        let fs = self.inner.filesystem.read();
        fs.files.contains_key(&path)
    }
}

/// A simulated file handle.
pub struct SimFile {
    /// Reference to the simulation environment (for fault injection).
    sim_env: SimEnv,
    /// Path of this file in the simulated filesystem.
    path: PathBuf,
    /// Whether writes append to end of file.
    append: bool,
}

impl File for SimFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        let fs = self.sim_env.inner.filesystem.read();
        let file = fs
            .files
            .get(&self.path)
            .ok_or_else(|| Error::NotFound(self.path.clone()))?;

        let offset = offset as usize;
        if offset >= file.data.len() {
            return Ok(0);
        }

        let available = file.data.len() - offset;
        let to_read = buf.len().min(available);
        buf[..to_read].copy_from_slice(&file.data[offset..offset + to_read]);
        Ok(to_read)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> Result<usize> {
        // Get references we need before acquiring locks
        let config = &self.sim_env.inner.fault_config;

        // Check disk full fault BEFORE acquiring write lock
        // This needs the filesystem state, so we check inside
        {
            let fs = self.sim_env.inner.filesystem.read();
            if config.disk_full_threshold > 0 {
                // Check if already triggered (sticky)
                if fs.disk_full_triggered {
                    drop(fs);
                    self.sim_env.record_fault_event(FaultEvent::DiskFull {
                        path: self.path.clone(),
                        attempted_bytes: buf.len(),
                        at_sim_time: self.sim_env.sim_time_as_duration(),
                    });
                    return Err(Error::DiskFull);
                }

                // Check if this write would exceed threshold
                if fs.total_bytes_written + buf.len() as u64 > config.disk_full_threshold {
                    drop(fs);
                    // Set the sticky flag
                    self.sim_env.inner.filesystem.write().disk_full_triggered = true;

                    self.sim_env.record_fault_event(FaultEvent::DiskFull {
                        path: self.path.clone(),
                        attempted_bytes: buf.len(),
                        at_sim_time: self.sim_env.sim_time_as_duration(),
                    });
                    return Err(Error::DiskFull);
                }
            }
        }

        // Check slow write fault BEFORE the write
        if config.slow_write_prob > 0.0 && config.slow_write_duration.as_nanos() > 0 {
            let roll = self.sim_env.rand_f64();
            if roll < config.slow_write_prob {
                let delay = config.slow_write_duration;
                self.sim_env.record_fault_event(FaultEvent::SlowWrite {
                    path: self.path.clone(),
                    bytes: buf.len(),
                    delay,
                    at_sim_time: self.sim_env.sim_time_as_duration(),
                });
                // Advance simulated time (no real sleep!)
                self.sim_env.advance_time(delay);
            }
        }

        // Determine actual bytes to write (may be truncated by partial_write)
        let actual_bytes = if config.partial_write_prob > 0.0 && !buf.is_empty() {
            let roll = self.sim_env.rand_f64();
            if roll < config.partial_write_prob {
                // Partial write: write 1 to buf.len()-1 bytes (at least 1 byte, less than full)
                // For single-byte writes, still write the 1 byte (can't do a partial single byte)
                let max_write = if buf.len() == 1 {
                    1
                } else {
                    // Write 1 to buf.len()-1 bytes
                    1 + self.sim_env.rand_range((buf.len() - 1) as u64) as usize
                };
                self.sim_env.record_fault_event(FaultEvent::PartialWrite {
                    path: self.path.clone(),
                    intended_bytes: buf.len(),
                    actual_bytes: max_write,
                    at_sim_time: self.sim_env.sim_time_as_duration(),
                });
                max_write
            } else {
                buf.len()
            }
        } else {
            buf.len()
        };

        // Actually perform the write
        let buf_to_write = &buf[..actual_bytes];
        let mut fs = self.sim_env.inner.filesystem.write();
        let file = fs
            .files
            .get_mut(&self.path)
            .ok_or_else(|| Error::NotFound(self.path.clone()))?;

        let offset = if self.append {
            file.data.len()
        } else {
            offset as usize
        };

        // Extend file if needed
        if offset + buf_to_write.len() > file.data.len() {
            file.data.resize(offset + buf_to_write.len(), 0);
        }

        file.data[offset..offset + buf_to_write.len()].copy_from_slice(buf_to_write);

        // Track total bytes written for disk_full threshold
        fs.total_bytes_written += buf_to_write.len() as u64;

        Ok(actual_bytes)
    }

    fn sync(&self) -> Result<()> {
        let mut fs = self.sim_env.inner.filesystem.write();
        let file = fs
            .files
            .get_mut(&self.path)
            .ok_or_else(|| Error::NotFound(self.path.clone()))?;

        file.synced_len = file.data.len();
        Ok(())
    }

    fn len(&self) -> Result<u64> {
        let fs = self.sim_env.inner.filesystem.read();
        let file = fs
            .files
            .get(&self.path)
            .ok_or_else(|| Error::NotFound(self.path.clone()))?;
        Ok(file.data.len() as u64)
    }

    fn truncate(&self, len: u64) -> Result<()> {
        let mut fs = self.sim_env.inner.filesystem.write();
        let file = fs
            .files
            .get_mut(&self.path)
            .ok_or_else(|| Error::NotFound(self.path.clone()))?;

        file.data.resize(len as usize, 0);
        if file.synced_len > len as usize {
            file.synced_len = len as usize;
        }
        Ok(())
    }
}

/// Normalize a path for consistent comparison.
fn normalize_path(path: &Path) -> PathBuf {
    // Simple normalization - in a real implementation we'd handle more cases
    let path_str = path.to_string_lossy();
    if path_str.starts_with('/') {
        path.to_path_buf()
    } else {
        PathBuf::from("/").join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_random_same_seed() {
        let env1 = SimEnv::new(SimEnvConfig::with_seed(42));
        let env2 = SimEnv::new(SimEnvConfig::with_seed(42));

        let seq1: Vec<u64> = (0..100).map(|_| env1.rand_u64()).collect();
        let seq2: Vec<u64> = (0..100).map(|_| env2.rand_u64()).collect();

        assert_eq!(
            seq1, seq2,
            "Same seed must produce identical random sequence"
        );
    }

    #[test]
    fn deterministic_random_different_seed() {
        let env1 = SimEnv::new(SimEnvConfig::with_seed(42));
        let env2 = SimEnv::new(SimEnvConfig::with_seed(43));

        let seq1: Vec<u64> = (0..100).map(|_| env1.rand_u64()).collect();
        let seq2: Vec<u64> = (0..100).map(|_| env2.rand_u64()).collect();

        assert_ne!(
            seq1, seq2,
            "Different seeds should produce different sequences"
        );
    }

    #[test]
    fn deterministic_time() {
        let env = SimEnv::new(SimEnvConfig::with_seed(42));

        let t1 = env.now();
        assert_eq!(t1.as_nanos(), 0);

        env.advance_time(Duration::from_secs(1));
        let t2 = env.now();
        assert_eq!(t2.as_nanos(), 1_000_000_000);

        env.advance_time(Duration::from_millis(500));
        let t3 = env.now();
        assert_eq!(t3.as_nanos(), 1_500_000_000);
    }

    #[test]
    fn sleep_advances_time() {
        let env = SimEnv::new(SimEnvConfig::with_seed(42));

        let t1 = env.now();
        assert_eq!(t1.as_nanos(), 0);

        env.sleep(Duration::from_secs(1));
        let t2 = env.now();
        assert_eq!(t2.as_nanos(), 1_000_000_000);
    }

    #[test]
    fn filesystem_basic_ops() {
        let env = SimEnv::new(SimEnvConfig::with_seed(42));

        // Create directory
        env.create_dir_all(Path::new("/test")).unwrap();
        assert!(env.is_dir(Path::new("/test")));

        // Create file
        let file = env
            .open(Path::new("/test/file.txt"), OpenOptions::write())
            .unwrap();
        file.write_all_at(b"hello world", 0).unwrap();
        file.sync().unwrap();

        // Read back
        let mut buf = [0u8; 11];
        file.read_exact_at(&mut buf, 0).unwrap();
        assert_eq!(&buf, b"hello world");

        // Check exists
        assert!(env.exists(Path::new("/test/file.txt")));
        assert!(env.is_file(Path::new("/test/file.txt")));

        // List directory
        let entries = env.list_dir(Path::new("/test")).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], PathBuf::from("/test/file.txt"));

        // Remove file
        env.remove(Path::new("/test/file.txt")).unwrap();
        assert!(!env.exists(Path::new("/test/file.txt")));
    }

    #[test]
    fn filesystem_crash_simulation() {
        let env = SimEnv::new(SimEnvConfig::with_seed(42));

        env.create_dir_all(Path::new("/data")).unwrap();
        let file = env
            .open(Path::new("/data/file.txt"), OpenOptions::write())
            .unwrap();

        // Write and sync some data
        file.write_all_at(b"committed", 0).unwrap();
        file.sync().unwrap();

        // Write more data but don't sync
        file.write_all_at(b"uncommitted", 9).unwrap();

        // Verify all data is there before crash
        assert_eq!(file.len().unwrap(), 20);

        // Simulate crash
        drop(file);
        env.simulate_crash();

        // Reopen and verify only synced data remains
        let file = env
            .open(Path::new("/data/file.txt"), OpenOptions::read())
            .unwrap();
        assert_eq!(file.len().unwrap(), 9);

        let mut buf = [0u8; 9];
        file.read_exact_at(&mut buf, 0).unwrap();
        assert_eq!(&buf, b"committed");
    }

    #[test]
    fn identical_operations_same_seed() {
        // This is the key test: given the same seed, two simulation runs
        // must produce byte-identical results.

        fn run_simulation(seed: u64) -> (Vec<u64>, BTreeMap<PathBuf, Vec<u8>>) {
            let env = SimEnv::new(SimEnvConfig::with_seed(seed));

            // Generate some random numbers
            let randoms: Vec<u64> = (0..10).map(|_| env.rand_u64()).collect();

            // Create some files based on random decisions
            env.create_dir_all(Path::new("/test")).unwrap();

            for i in 0..5 {
                let should_create = env.rand_u64() % 2 == 0;
                if should_create {
                    let path = format!("/test/file_{}.txt", i);
                    let file = env.open(Path::new(&path), OpenOptions::write()).unwrap();
                    let content = format!("content_{}", env.rand_u64());
                    file.write_all_at(content.as_bytes(), 0).unwrap();
                    file.sync().unwrap();
                }
            }

            let snapshot = env.filesystem_snapshot();
            (randoms, snapshot)
        }

        let (randoms1, fs1) = run_simulation(12345);
        let (randoms2, fs2) = run_simulation(12345);

        assert_eq!(randoms1, randoms2, "Random sequences must match");
        assert_eq!(fs1, fs2, "Filesystem state must match");
    }

    /// Phase 0 acceptance test: 1000 runs of the same seed must produce
    /// byte-identical state. This is the foundation of deterministic simulation.
    #[test]
    #[ignore] // Run with: cargo test --release --package runtime determinism_acceptance_1000_iterations -- --ignored
    fn determinism_acceptance_1000_iterations() {
        const SEED: u64 = 0xDEADBEEF;
        const ITERATIONS: usize = 1000;

        /// Run a complex workload and return the final state for comparison.
        fn run_workload(seed: u64) -> (u64, BTreeMap<PathBuf, Vec<u8>>) {
            let env = SimEnv::new(SimEnvConfig::with_seed(seed));

            // Advance time randomly
            for _ in 0..10 {
                let duration_ms = env.rand_range(1000);
                env.sleep(Duration::from_millis(duration_ms));
            }

            // Create directories
            env.create_dir_all(Path::new("/data")).unwrap();
            env.create_dir_all(Path::new("/logs")).unwrap();

            // Create files with random content
            for i in 0..20 {
                let path = if env.rand_u64() % 2 == 0 {
                    format!("/data/file_{:03}.dat", i)
                } else {
                    format!("/logs/log_{:03}.txt", i)
                };

                let file = env.open(Path::new(&path), OpenOptions::write()).unwrap();

                // Write random content
                let content_len = (env.rand_range(100) + 10) as usize;
                let mut content = vec![0u8; content_len];
                env.fill_random(&mut content);
                file.write_all_at(&content, 0).unwrap();

                // Randomly sync
                if env.rand_u64() % 3 == 0 {
                    file.sync().unwrap();
                }
            }

            // Do some random file operations
            for _ in 0..10 {
                match env.rand_range(3) {
                    0 => {
                        // Read a random file
                        let entries = env.list_dir(Path::new("/data")).unwrap_or_default();
                        if !entries.is_empty() {
                            let idx = env.rand_range(entries.len() as u64) as usize;
                            if let Ok(file) = env.open(&entries[idx], OpenOptions::read()) {
                                let mut buf = vec![0u8; 100];
                                let _ = file.read_at(&mut buf, 0);
                            }
                        }
                    }
                    1 => {
                        // Remove a random file
                        let entries = env.list_dir(Path::new("/logs")).unwrap_or_default();
                        if !entries.is_empty() {
                            let idx = env.rand_range(entries.len() as u64) as usize;
                            let _ = env.remove(&entries[idx]);
                        }
                    }
                    2 => {
                        // Simulate crash and verify synced data
                        env.simulate_crash();
                    }
                    _ => unreachable!(),
                }
            }

            // Return RNG state and filesystem snapshot
            let rng_state = env.rng_state();
            let fs_snapshot = env.filesystem_snapshot();
            (rng_state, fs_snapshot)
        }

        // Run the workload once to get the reference state
        let (ref_rng, ref_fs) = run_workload(SEED);

        // Run 999 more times and verify all match
        for iteration in 1..ITERATIONS {
            let (rng_state, fs_snapshot) = run_workload(SEED);

            assert_eq!(
                rng_state, ref_rng,
                "RNG state mismatch at iteration {}",
                iteration
            );
            assert_eq!(
                fs_snapshot, ref_fs,
                "Filesystem state mismatch at iteration {}",
                iteration
            );
        }
    }

    // ========================================
    // Part A: Fault infrastructure tests
    // ========================================

    #[test]
    fn fault_config_defaults_disable_all_faults() {
        // Construct SimEnv with default config, run a small workload, verify fault_events is empty.
        let env = SimEnv::new(SimEnvConfig::with_seed(0xFA17));

        // Run a small workload
        env.create_dir_all(Path::new("/test")).unwrap();
        let file = env
            .open(Path::new("/test/data.bin"), OpenOptions::write())
            .unwrap();
        file.write_all_at(b"hello world", 0).unwrap();
        file.sync().unwrap();

        // Call now() and sleep() which could trigger clock_skew and process_pause
        let _t = env.now();
        env.sleep(Duration::from_millis(100));
        let _t2 = env.now();

        // Verify no fault events were recorded
        let events = env.fault_events();
        assert!(
            events.is_empty(),
            "Expected no fault events with default FaultConfig, got {:?}",
            events
        );

        // Verify the default config is all disabled
        assert!(env.fault_config().all_disabled());
    }

    #[test]
    fn rand_f64_is_deterministic() {
        // Same seed produces same f64 sequence
        let env1 = SimEnv::new(SimEnvConfig::with_seed(0xF64_5EED));
        let env2 = SimEnv::new(SimEnvConfig::with_seed(0xF64_5EED));

        let seq1: Vec<f64> = (0..100).map(|_| env1.rand_f64()).collect();
        let seq2: Vec<f64> = (0..100).map(|_| env2.rand_f64()).collect();

        assert_eq!(seq1, seq2, "Same seed must produce identical f64 sequence");

        // All values should be in [0, 1)
        for (i, &v) in seq1.iter().enumerate() {
            assert!(
                (0.0..1.0).contains(&v),
                "rand_f64 value {} at index {} is out of range [0,1)",
                v,
                i
            );
        }
    }

    #[test]
    fn new_with_faults_stores_config() {
        let config = FaultConfig {
            partial_write_prob: 0.5,
            disk_full_threshold: 1024,
            slow_write_prob: 0.1,
            slow_write_duration: Duration::from_millis(50),
            clock_skew_prob: 0.2,
            clock_skew_max: Duration::from_secs(1),
            process_pause_prob: 0.15,
            process_pause_duration: Duration::from_millis(200),
        };

        let env = SimEnv::new_with_faults(SimEnvConfig::with_seed(42), config.clone());

        assert_eq!(env.fault_config().partial_write_prob, 0.5);
        assert_eq!(env.fault_config().disk_full_threshold, 1024);
        assert_eq!(env.fault_config().slow_write_prob, 0.1);
        assert_eq!(env.fault_config().clock_skew_prob, 0.2);
        assert_eq!(env.fault_config().process_pause_prob, 0.15);
        assert!(!env.fault_config().all_disabled());
    }

    #[test]
    fn fault_event_log_initially_empty() {
        let env = SimEnv::new_with_faults(
            SimEnvConfig::with_seed(42),
            FaultConfig::with_partial_writes(0.5),
        );

        // Even with faults enabled, no events until we actually do IO
        assert!(env.fault_events().is_empty());
    }

    #[test]
    fn clear_fault_events_works() {
        let env = SimEnv::new(SimEnvConfig::with_seed(42));

        // Manually verify clear works (even though nothing was logged)
        env.clear_fault_events();
        assert!(env.fault_events().is_empty());
    }

    #[test]
    fn total_bytes_written_initially_zero() {
        let env = SimEnv::new(SimEnvConfig::with_seed(42));
        assert_eq!(env.total_bytes_written(), 0);
    }

    #[test]
    fn disk_full_initially_false() {
        let env = SimEnv::new(SimEnvConfig::with_seed(42));
        assert!(!env.is_disk_full());
    }

    #[test]
    fn simulate_disk_recovery_clears_flag() {
        let env = SimEnv::new(SimEnvConfig::with_seed(42));
        // Even if disk wasn't full, calling recovery should be safe
        env.simulate_disk_recovery();
        assert!(!env.is_disk_full());
    }

    // ========================================
    // Part B: Fault implementation tests
    // ========================================

    #[test]
    fn partial_write_fault_truncates_writes() {
        // Enable partial_write_prob = 1.0, write 100 bytes, verify <100 bytes are persisted
        let env = SimEnv::new_with_faults(
            SimEnvConfig::with_seed(0x7A271A1),
            FaultConfig::with_partial_writes(1.0),
        );

        env.create_dir_all(Path::new("/test")).unwrap();
        let file = env
            .open(Path::new("/test/data.bin"), OpenOptions::write())
            .unwrap();

        // Write 100 bytes
        let data = vec![0xAB_u8; 100];
        let written = file.write_at(&data, 0).unwrap();

        // With partial_write_prob = 1.0, the write should be truncated
        // (writes 1 to 99 bytes, never 0 or full amount)
        assert!(
            (1..100).contains(&written),
            "Partial write should write 1 to n-1 bytes, got {}",
            written
        );

        // Check that an event was recorded
        let events = env.fault_events();
        assert_eq!(events.len(), 1, "Expected 1 partial write event");
        match &events[0] {
            FaultEvent::PartialWrite {
                intended_bytes,
                actual_bytes,
                ..
            } => {
                assert_eq!(*intended_bytes, 100);
                assert_eq!(*actual_bytes, written);
            }
            other => panic!("Expected PartialWrite event, got {:?}", other),
        }
    }

    #[test]
    fn disk_full_fault_returns_enospc() {
        // Enable disk_full_threshold = 1024, write 2048 bytes, second write returns Err
        let env = SimEnv::new_with_faults(
            SimEnvConfig::with_seed(0xD15F011),
            FaultConfig::with_disk_full(1024),
        );

        env.create_dir_all(Path::new("/test")).unwrap();
        let file = env
            .open(Path::new("/test/data.bin"), OpenOptions::write())
            .unwrap();

        // First write should succeed (under threshold)
        let data1 = vec![0xAA; 512];
        let result1 = file.write_at(&data1, 0);
        assert!(result1.is_ok(), "First write should succeed");
        assert_eq!(env.total_bytes_written(), 512);

        // Second write should trigger disk full (would exceed 1024)
        let data2 = vec![0xBB; 600];
        let result2 = file.write_at(&data2, 512);
        assert!(result2.is_err(), "Second write should fail with disk full");
        assert!(env.is_disk_full(), "Disk full flag should be set");

        // Check error type
        match result2 {
            Err(Error::DiskFull) => {}
            other => panic!("Expected DiskFull error, got {:?}", other),
        }

        // Check that an event was recorded
        let events = env.fault_events();
        assert_eq!(events.len(), 1, "Expected 1 disk full event");
        assert_eq!(events[0].fault_type(), "disk_full");

        // Third write should also fail (sticky)
        let data3 = vec![0xCC; 10];
        let result3 = file.write_at(&data3, 0);
        assert!(
            result3.is_err(),
            "Write should still fail (sticky disk full)"
        );
        assert_eq!(
            env.fault_events().len(),
            2,
            "Should record another disk full event"
        );

        // Recovery should allow writes again (but will immediately fail since threshold still exceeded)
        env.simulate_disk_recovery();
        assert!(!env.is_disk_full());
    }

    #[test]
    fn slow_write_advances_sim_time() {
        // Enable slow_write_prob = 1.0, write, verify env.now() advanced by slow_write_duration
        let delay = Duration::from_millis(500);
        let env = SimEnv::new_with_faults(
            SimEnvConfig::with_seed(0x5108),
            FaultConfig::with_slow_writes(1.0, delay),
        );

        env.create_dir_all(Path::new("/test")).unwrap();
        let file = env
            .open(Path::new("/test/data.bin"), OpenOptions::write())
            .unwrap();

        let t1 = env.now();

        // Write should trigger slow write fault
        let data = vec![0xAB; 100];
        file.write_at(&data, 0).unwrap();

        let t2 = env.now();

        // Time should have advanced by the slow_write_duration
        let elapsed = t2 - t1;
        assert_eq!(
            elapsed, delay,
            "Expected time to advance by {:?}, got {:?}",
            delay, elapsed
        );

        // Check that an event was recorded
        let events = env.fault_events();
        assert_eq!(events.len(), 1, "Expected 1 slow write event");
        assert_eq!(events[0].fault_type(), "slow_write");
    }

    #[test]
    fn clock_skew_returns_skewed_time() {
        // Enable clock_skew_prob = 1.0 with clock_skew_max = 1s
        // Call env.now() repeatedly, verify some readings differ from underlying simulated time
        let max_skew = Duration::from_secs(1);
        let env = SimEnv::new_with_faults(
            SimEnvConfig::with_seed(0xC10C0),
            FaultConfig::with_clock_skew(1.0, max_skew),
        );

        // Get multiple readings
        let _readings: Vec<Instant> = (0..10).map(|_| env.now()).collect();

        // All readings should have triggered clock skew events
        let events = env.fault_events();
        assert_eq!(events.len(), 10, "Expected 10 clock skew events");

        // Check that all events are clock skew
        for event in &events {
            assert_eq!(event.fault_type(), "clock_skew");
        }

        // The skews should vary (since they're random)
        // We can't check exact values, but we can verify the skew magnitudes are within bounds
        for event in &events {
            if let FaultEvent::ClockSkew { skew_magnitude, .. } = event {
                assert!(
                    *skew_magnitude <= max_skew,
                    "Skew {:?} exceeds max {:?}",
                    skew_magnitude,
                    max_skew
                );
            }
        }

        // With prob=1.0, all readings should have been skewed
        // This means the events capture the behavior
        assert!(!events.is_empty());
    }

    #[test]
    fn process_pause_extends_sleep() {
        // Enable process_pause_prob = 1.0, sleep(100ms), verify env.now() advanced by ~100ms + pause_duration
        let pause_duration = Duration::from_millis(500);
        let requested_sleep = Duration::from_millis(100);
        let env = SimEnv::new_with_faults(
            SimEnvConfig::with_seed(0x7A05E),
            FaultConfig::with_process_pause(1.0, pause_duration),
        );

        let t1 = env.now();
        env.sleep(requested_sleep);
        let t2 = env.now();

        let elapsed = t2 - t1;
        let expected = requested_sleep + pause_duration;

        assert_eq!(
            elapsed, expected,
            "Expected sleep + pause ({:?}), got {:?}",
            expected, elapsed
        );

        // Check that an event was recorded
        let events = env.fault_events();
        assert_eq!(events.len(), 1, "Expected 1 process pause event");
        match &events[0] {
            FaultEvent::ProcessPause {
                requested, actual, ..
            } => {
                assert_eq!(*requested, requested_sleep);
                assert_eq!(*actual, expected);
            }
            other => panic!("Expected ProcessPause event, got {:?}", other),
        }
    }

    #[test]
    fn faults_recorded_in_event_log() {
        // Enable each fault at 100%, trigger, verify event log contains the expected event
        let env = SimEnv::new_with_faults(
            SimEnvConfig::with_seed(0xE4E475),
            FaultConfig {
                partial_write_prob: 1.0,
                disk_full_threshold: 0, // disabled
                slow_write_prob: 1.0,
                slow_write_duration: Duration::from_millis(10),
                clock_skew_prob: 1.0,
                clock_skew_max: Duration::from_millis(100),
                process_pause_prob: 1.0,
                process_pause_duration: Duration::from_millis(50),
            },
        );

        env.create_dir_all(Path::new("/test")).unwrap();
        let file = env
            .open(Path::new("/test/data.bin"), OpenOptions::write())
            .unwrap();

        // Trigger clock_skew via now()
        let _ = env.now();

        // Trigger process_pause via sleep()
        env.sleep(Duration::from_millis(10));

        // Trigger slow_write and partial_write via write_at()
        let _ = file.write_at(&[0xAA; 50], 0);

        let events = env.fault_events();

        // Count event types
        let clock_skew_count = events
            .iter()
            .filter(|e| matches!(e, FaultEvent::ClockSkew { .. }))
            .count();
        let process_pause_count = events
            .iter()
            .filter(|e| matches!(e, FaultEvent::ProcessPause { .. }))
            .count();
        let slow_write_count = events
            .iter()
            .filter(|e| matches!(e, FaultEvent::SlowWrite { .. }))
            .count();
        let partial_write_count = events
            .iter()
            .filter(|e| matches!(e, FaultEvent::PartialWrite { .. }))
            .count();

        assert!(clock_skew_count >= 1, "Expected at least 1 ClockSkew event");
        assert!(
            process_pause_count >= 1,
            "Expected at least 1 ProcessPause event"
        );
        assert!(slow_write_count >= 1, "Expected at least 1 SlowWrite event");
        assert!(
            partial_write_count >= 1,
            "Expected at least 1 PartialWrite event"
        );
    }

    #[test]
    fn faults_are_deterministic() {
        // Two SimEnv instances with same seed + same FaultConfig produce identical fault_events sequences
        let config = FaultConfig {
            partial_write_prob: 0.5,
            disk_full_threshold: 0,
            slow_write_prob: 0.3,
            slow_write_duration: Duration::from_millis(50),
            clock_skew_prob: 0.4,
            clock_skew_max: Duration::from_secs(1),
            process_pause_prob: 0.2,
            process_pause_duration: Duration::from_millis(100),
        };

        fn run_workload(seed: u64, config: &FaultConfig) -> Vec<FaultEvent> {
            let env = SimEnv::new_with_faults(SimEnvConfig::with_seed(seed), config.clone());

            env.create_dir_all(Path::new("/test")).unwrap();

            // Do a series of operations that will trigger faults probabilistically
            for i in 0..20 {
                // now() may trigger clock skew
                let _ = env.now();

                // sleep may trigger process pause
                env.sleep(Duration::from_millis(10));

                // write may trigger partial_write and slow_write
                let path = format!("/test/file_{}.dat", i);
                if let Ok(file) = env.open(Path::new(&path), OpenOptions::write()) {
                    let data = vec![0xAB; 100];
                    let _ = file.write_at(&data, 0);
                }
            }

            env.fault_events()
        }

        let seed = 0xDE7E8010;
        let events1 = run_workload(seed, &config);
        let events2 = run_workload(seed, &config);

        assert_eq!(
            events1.len(),
            events2.len(),
            "Fault event count mismatch: {} vs {}",
            events1.len(),
            events2.len()
        );

        for (i, (e1, e2)) in events1.iter().zip(events2.iter()).enumerate() {
            assert_eq!(
                e1, e2,
                "Fault event mismatch at index {}: {:?} vs {:?}",
                i, e1, e2
            );
        }
    }

    #[test]
    fn total_bytes_written_tracks_writes() {
        let env = SimEnv::new(SimEnvConfig::with_seed(42));

        env.create_dir_all(Path::new("/test")).unwrap();
        let file = env
            .open(Path::new("/test/data.bin"), OpenOptions::write())
            .unwrap();

        assert_eq!(env.total_bytes_written(), 0);

        file.write_at(&[0xAA; 100], 0).unwrap();
        assert_eq!(env.total_bytes_written(), 100);

        file.write_at(&[0xBB; 50], 100).unwrap();
        assert_eq!(env.total_bytes_written(), 150);
    }
}
