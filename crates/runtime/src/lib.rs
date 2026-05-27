//! Deterministic IO abstraction layer for cracked-db.
//!
//! This crate provides the [`Env`] trait which abstracts every source of nondeterminism
//! in the system. Two implementations are provided:
//!
//! - [`RealEnv`]: Production implementation using actual system calls
//! - [`SimEnv`]: Deterministic, seedable implementation for simulation testing
//!
//! # The Foundation Rule
//!
//! **No crate above `runtime` may import `std::time`, `std::fs`, `std::thread`,
//! `std::sync::Mutex`, or `rand::random`.** All such operations must go through
//! the [`Env`] trait. Violation of this rule breaks the deterministic simulation
//! foundation.
//!
//! # Example
//!
//! ```
//! use runtime::{Env, RealEnv, Instant, Duration};
//!
//! fn example<E: Env>(env: &E) {
//!     let now = env.now();
//!     let random = env.rand_u64();
//!     // All IO and timing goes through the env
//! }
//!
//! // In production
//! let real_env = RealEnv::new();
//! example(&real_env);
//! ```

mod env;
mod error;
mod file;
mod real;
mod sim;
mod sim_faults;

pub use env::{Env, JoinHandle, OpenOptions};
pub use error::{Error, Result};
pub use file::File;
pub use real::RealEnv;
pub use sim::{SimEnv, SimEnvConfig};
pub use sim_faults::{FaultConfig, FaultEvent};

/// A monotonic instant in time.
///
/// This is intentionally a simple wrapper to avoid exposing `std::time::Instant`
/// directly, which would allow circumventing the deterministic runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instant(u64);

impl Instant {
    /// Creates a new instant from nanoseconds since an arbitrary epoch.
    #[inline]
    pub const fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    /// Returns the number of nanoseconds since the epoch.
    #[inline]
    pub const fn as_nanos(&self) -> u64 {
        self.0
    }

    /// Returns the duration since an earlier instant.
    ///
    /// Returns `None` if `earlier` is after `self`.
    #[inline]
    pub fn checked_duration_since(&self, earlier: Instant) -> Option<Duration> {
        self.0.checked_sub(earlier.0).map(Duration::from_nanos)
    }

    /// Adds a duration to this instant.
    ///
    /// Returns `None` on overflow.
    #[inline]
    pub fn checked_add(&self, duration: Duration) -> Option<Instant> {
        self.0.checked_add(duration.as_nanos() as u64).map(Instant)
    }
}

impl std::ops::Add<Duration> for Instant {
    type Output = Instant;

    #[inline]
    fn add(self, rhs: Duration) -> Self::Output {
        Instant(self.0 + rhs.as_nanos() as u64)
    }
}

impl std::ops::Sub<Instant> for Instant {
    type Output = Duration;

    #[inline]
    fn sub(self, rhs: Instant) -> Self::Output {
        Duration::from_nanos(self.0.saturating_sub(rhs.0))
    }
}

/// A duration of time.
///
/// Re-exported from `std::time` for convenience. This is safe because
/// `Duration` itself contains no nondeterminism—it's just a number.
pub use std::time::Duration;

/// Path type re-export for convenience.
pub use std::path::{Path, PathBuf};
