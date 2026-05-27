//! File abstraction trait.

use crate::Result;

/// A file handle that supports positioned reads and writes.
///
/// This trait abstracts file operations to enable deterministic simulation.
/// All implementations must be `Send + Sync` to allow files to be shared between
/// threads (required for watermark callbacks in the storage engine).
///
/// # Positioned I/O
///
/// Unlike `std::fs::File`, this trait uses positioned I/O (`read_at`, `write_at`)
/// rather than a mutable file position. This is more amenable to concurrent access
/// and easier to reason about in a simulation context.
pub trait File: Send + Sync {
    /// Reads bytes from the file at the given offset.
    ///
    /// Returns the number of bytes read. A return value of 0 indicates EOF.
    /// The file position is not changed.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize>;

    /// Writes bytes to the file at the given offset.
    ///
    /// Returns the number of bytes written. In simulation, this may return
    /// fewer bytes than requested to simulate partial writes.
    /// The file position is not changed.
    fn write_at(&self, buf: &[u8], offset: u64) -> Result<usize>;

    /// Ensures all written data is durably persisted to storage.
    ///
    /// This is equivalent to `fsync(2)`. In simulation, this is where
    /// crash injection can discard unflushed writes.
    fn sync(&self) -> Result<()>;

    /// Returns the current length of the file in bytes.
    fn len(&self) -> Result<u64>;

    /// Returns true if the file is empty.
    fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Truncates or extends the file to the specified length.
    ///
    /// If `len` is less than the current length, the file is truncated.
    /// If `len` is greater, the file is extended with zeros.
    fn truncate(&self, len: u64) -> Result<()>;

    /// Reads exactly `buf.len()` bytes from the file at the given offset.
    ///
    /// Returns an error if EOF is reached before the buffer is filled.
    fn read_exact_at(&self, buf: &mut [u8], mut offset: u64) -> Result<()> {
        let mut total = 0;
        while total < buf.len() {
            let n = self.read_at(&mut buf[total..], offset)?;
            if n == 0 {
                return Err(crate::Error::Io {
                    path: None,
                    message: "unexpected EOF".to_string(),
                    source: None,
                });
            }
            total += n;
            offset += n as u64;
        }
        Ok(())
    }

    /// Writes all bytes to the file at the given offset.
    ///
    /// Returns an error if not all bytes could be written.
    fn write_all_at(&self, buf: &[u8], mut offset: u64) -> Result<()> {
        let mut total = 0;
        while total < buf.len() {
            let n = self.write_at(&buf[total..], offset)?;
            if n == 0 {
                return Err(crate::Error::Io {
                    path: None,
                    message: "write returned 0".to_string(),
                    source: None,
                });
            }
            total += n;
            offset += n as u64;
        }
        Ok(())
    }
}
