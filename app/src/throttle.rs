// One speed limit shared by every download. Each connection asks for permission before
// using the bytes it just read; when the limit is reached, it waits a moment. Zero means
// no limit.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

static LIMIT_BPS: AtomicU64 = AtomicU64::new(0);

/// Bytes allowed right now (can go below zero: that is time owed) and when it was last filled.
static BUCKET: Mutex<Option<(f64, Instant)>> = Mutex::new(None);

/// Set the limit in bytes per second; 0 turns it off.
pub fn set(bps: u64) {
    LIMIT_BPS.store(bps, Ordering::SeqCst);
    *BUCKET.lock().unwrap() = None;
    crate::torrent::apply_speed_limit(bps);
}

/// The limit in bytes per second (0 = none).
pub fn get() -> u64 {
    LIMIT_BPS.load(Ordering::Relaxed)
}

/// How long to wait after using `n` bytes so all downloads together stay under the limit.
fn owed(n: u64, now: Instant) -> Duration {
    let rate = get();
    if rate == 0 {
        return Duration::ZERO;
    }
    owed_from(&mut BUCKET.lock().unwrap(), rate, n, now)
}

fn owed_from(b: &mut Option<(f64, Instant)>, rate: u64, n: u64, now: Instant) -> Duration {
    let rate = rate as f64;
    let (mut tokens, last) = b.unwrap_or((rate * 0.25, now));
    // Refill for the time that passed, keeping at most a quarter second of burst.
    tokens = (tokens + now.saturating_duration_since(last).as_secs_f64() * rate).min(rate * 0.25);
    tokens -= n as f64;
    *b = Some((tokens, now));
    if tokens >= 0.0 {
        Duration::ZERO
    } else {
        Duration::from_secs_f64((-tokens / rate).min(5.0))
    }
}

/// Account for `n` bytes just read, waiting if the limit asks for it.
pub fn take(n: usize) {
    let wait = owed(n as u64, Instant::now());
    if !wait.is_zero() {
        thread::sleep(wait);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limit_holds_across_many_reads() {
        // Simulated clock: 1 MB/s, reading 64 KB at a time for what would be 4 MB.
        let mut bucket = None;
        let start = Instant::now();
        let mut now = start;
        for _ in 0..64 {
            now += owed_from(&mut bucket, 1024 * 1024, 64 * 1024, now);
        }
        let secs = now.duration_since(start).as_secs_f64();
        // 4 MB at 1 MB/s takes about 4 s, minus the small burst allowed at the start.
        assert!((3.5..=4.1).contains(&secs), "took {secs}");
        if get() == 0 {
            assert_eq!(owed(10_000_000, now), Duration::ZERO, "no limit, no waiting");
        }
    }
}
