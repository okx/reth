//! Rate-limited and monitoring writer wrappers for snapshot writes.
//!
//! Prevents snapshot writes from evicting the OS page cache by throttling
//! I/O throughput to a configurable MB/s limit. The design mirrors the Go
//! implementation's `rateLimitedWriter` and `monitoringWriter`.

use parking_lot::Mutex;
use std::{
    io::Write,
    sync::Arc,
    time::{Duration, Instant},
};

/// Simple token-bucket rate limiter for snapshot writes.
///
/// Limits throughput to `rate_bytes_per_sec` bytes/second. A single limiter
/// should be shared across all file writers in a snapshot operation so that
/// the aggregate write rate is bounded (matching the Go `NewGlobalRateLimiter`
/// pattern).
#[derive(Clone)]
pub struct RateLimiter {
    inner: Arc<Mutex<RateLimiterInner>>,
}

struct RateLimiterInner {
    rate_bytes_per_sec: u64,
    tokens: f64,
    last_refill: Instant,
    /// Maximum burst size in bytes. Set to 4 MB to spread large bufio
    /// flushes across many smaller I/O ops, preventing page cache eviction
    /// spikes (matches Go's burstBytes = 4 * MB).
    max_burst: f64,
}

/// 4 MB burst — matches the Go implementation.
const BURST_BYTES: u64 = 4 * 1024 * 1024;

impl RateLimiter {
    /// Create a new rate limiter.
    ///
    /// `rate_mbps` is the limit in megabytes per second. Returns `None` when
    /// `rate_mbps == 0` (unlimited).
    pub fn new(rate_mbps: u32) -> Option<Self> {
        if rate_mbps == 0 {
            return None;
        }
        let rate = rate_mbps as u64 * 1024 * 1024;
        let burst = BURST_BYTES.min(rate) as f64;
        Some(Self {
            inner: Arc::new(Mutex::new(RateLimiterInner {
                rate_bytes_per_sec: rate,
                tokens: burst, // start full up to burst
                last_refill: Instant::now(),
                max_burst: burst,
            })),
        })
    }

    /// Wait until `n` bytes are available, then consume them.
    ///
    /// For writes larger than the burst size, this method splits the request
    /// into burst-sized chunks (matching the Go `rateLimitedWriter.Write`
    /// loop) so that no single wait is excessively long.
    pub fn wait(&self, n: usize) {
        let mut remaining = n;
        while remaining > 0 {
            let chunk = remaining.min(BURST_BYTES as usize);
            self.wait_internal(chunk);
            remaining -= chunk;
        }
    }

    /// Internal: wait for exactly `n` bytes (must be <= burst size).
    fn wait_internal(&self, n: usize) {
        let mut inner = self.inner.lock();

        // Refill tokens based on elapsed time
        let now = Instant::now();
        let elapsed = now.duration_since(inner.last_refill).as_secs_f64();
        inner.tokens =
            (inner.tokens + elapsed * inner.rate_bytes_per_sec as f64).min(inner.max_burst);
        inner.last_refill = now;

        let needed = n as f64;
        if inner.tokens >= needed {
            inner.tokens -= needed;
            return;
        }

        // Calculate sleep time for the deficit. Set tokens negative to "reserve"
        // the debt so that concurrent callers see the committed reservation.
        let deficit = needed - inner.tokens;
        let wait_secs = deficit / inner.rate_bytes_per_sec as f64;
        inner.tokens -= needed; // go negative to reserve the debt
        drop(inner); // release lock before sleeping

        std::thread::sleep(Duration::from_secs_f64(wait_secs));
    }
}

/// Writer wrapper that rate-limits writes to prevent page cache eviction.
pub struct RateLimitedWriter<W: Write> {
    inner: W,
    limiter: RateLimiter,
}

impl<W: Write> RateLimitedWriter<W> {
    pub fn new(inner: W, limiter: RateLimiter) -> Self {
        Self { inner, limiter }
    }
}

impl<W: Write> Write for RateLimitedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.limiter.wait(buf.len());
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Writer wrapper that logs progress periodically.
pub struct MonitoringWriter<W: Write> {
    inner: W,
    name: String,
    bytes_written: u64,
    last_log: Instant,
    log_interval: Duration,
}

impl<W: Write> MonitoringWriter<W> {
    pub fn new(inner: W, name: &str) -> Self {
        Self {
            inner,
            name: name.to_string(),
            bytes_written: 0,
            last_log: Instant::now(),
            log_interval: Duration::from_secs(10),
        }
    }

    /// Returns the total number of bytes written so far.
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}

impl<W: Write> Write for MonitoringWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.bytes_written += n as u64;
        if self.last_log.elapsed() >= self.log_interval {
            tracing::info!(
                writer = %self.name,
                bytes_written = self.bytes_written,
                mb_written = self.bytes_written / (1024 * 1024),
                "snapshot write progress"
            );
            self.last_log = Instant::now();
        }
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Convenience: wrap a writer with an optional rate limiter.
/// If `limiter` is `None`, returns the writer unchanged (boxed).
pub fn maybe_rate_limit<W: Write + 'static>(
    writer: W,
    limiter: Option<&RateLimiter>,
) -> Box<dyn Write> {
    match limiter {
        Some(l) => Box::new(RateLimitedWriter::new(writer, l.clone())),
        None => Box::new(writer),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rate_limiter_creation() {
        // 0 means unlimited → None
        assert!(RateLimiter::new(0).is_none());
        // Non-zero → Some
        assert!(RateLimiter::new(100).is_some());
        assert!(RateLimiter::new(1).is_some());
    }

    #[test]
    fn test_rate_limiter_throttles() {
        // 1 MB/s limiter with burst = min(4MB, 1MB) = 1MB.
        let limiter = RateLimiter::new(1).unwrap(); // 1 MB/s

        // Drain the burst tokens first
        limiter.wait(1024 * 1024);

        // Now measure: write 1 MB at 1 MB/s → should take ~1 second
        let start = Instant::now();
        limiter.wait(1024 * 1024);
        let elapsed = start.elapsed();

        assert!(
            elapsed >= Duration::from_millis(800),
            "expected >= 800ms of throttling, got {:?}",
            elapsed
        );
        assert!(elapsed < Duration::from_millis(3000), "expected < 3000ms, got {:?}", elapsed);
    }

    #[test]
    fn test_rate_limited_writer() {
        let limiter = RateLimiter::new(100).unwrap(); // 100 MB/s — fast enough to not slow the test
        let mut buf = Vec::new();
        {
            let mut writer = RateLimitedWriter::new(&mut buf, limiter);
            writer.write_all(b"hello world").unwrap();
            writer.write_all(b" more data").unwrap();
            writer.flush().unwrap();
        }
        assert_eq!(buf, b"hello world more data");
    }

    #[test]
    fn test_monitoring_writer() {
        let mut buf = Vec::new();
        {
            let mut writer = MonitoringWriter::new(&mut buf, "test-writer");
            writer.write_all(b"hello").unwrap();
            assert_eq!(writer.bytes_written(), 5);
            writer.write_all(b" world").unwrap();
            assert_eq!(writer.bytes_written(), 11);
            writer.flush().unwrap();
        }
        assert_eq!(buf, b"hello world");
    }

    #[test]
    fn test_rate_limiter_shared() {
        // Two writers sharing a single limiter at 1 MB/s (burst = 1MB).
        // Verify that both writers share the same token pool by doing
        // sequential writes from two clones.
        let limiter = RateLimiter::new(1).unwrap(); // 1 MB/s
        let limiter2 = limiter.clone();

        // Drain burst tokens
        limiter.wait(1024 * 1024);

        // Sequential writes from two clones: 1MB + 1MB = 2MB at 1MB/s → ~2s
        let start = Instant::now();
        limiter.wait(1024 * 1024); // ~1s wait
        limiter2.wait(1024 * 1024); // ~1s wait
        let elapsed = start.elapsed();

        // Both writes share the same token pool, so total is ~2 seconds.
        assert!(
            elapsed >= Duration::from_millis(1500),
            "expected >= 1500ms of shared throttling, got {:?}",
            elapsed
        );
    }
}
