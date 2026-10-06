//! Holding map generation to a share of the CPU, on every core.
//!
//! Heat follows power, and power follows how much work is done per second, so
//! a cooler build is a slower one whatever the method. The choice is how to
//! slow it. Fewer threads leaves the busy cores at full boost; this keeps
//! every core in use and makes them all rest in a fixed rhythm instead: in
//! each 100 ms window the workers run for `limit`% and then wait. The window
//! is short against the chip's thermal mass, so the temperature settles near
//! what the average power would give rather than following the bursts.
//!
//! Workers call [`gate`] at the top of each step of a parallel loop -- a row
//! of a raster, one tile -- so they stop within about a millisecond of the
//! gate closing. At 100% there is no timer and `gate` is a single load.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Once;
use std::time::Duration;

/// Percent of each window the workers may run. 100 means no limit.
static LIMIT: AtomicU8 = AtomicU8::new(100);
static CLOSED: AtomicBool = AtomicBool::new(false);
static TIMER: Once = Once::new();

const WINDOW_MS: u64 = 100;
/// Until someone picks a level in the Maps window. Balanced, not full speed:
/// measured on a 9800X3D it costs a build about 18% more time and keeps the
/// CPU under 90°C throughout, where full speed peaks near 95°C. Not everyone's
/// cooling is up to the latter.
pub const DEFAULT: u8 = 70;

/// Offered in the Maps window: label, percent, and what it is for.
pub const LEVELS: &[(&str, u8, &str)] = &[
    ("Full speed", 100, "every core flat out; fastest, and hottest"),
    ("Balanced", 70, "every core, resting 30% of the time; about 1.2x as long, and no hotter than about 86°C on a 9800X3D"),
    ("Cool", 45, "every core, resting more than half the time; about 1.6x as long, and no hotter than about 80°C on a 9800X3D"),
];

pub fn set_limit(percent: u8) {
    let p = percent.clamp(10, 100);
    LIMIT.store(p, Ordering::Relaxed);
    if p < 100 {
        TIMER.call_once(|| {
            std::thread::Builder::new()
                .name("throttle".into())
                .spawn(run_timer)
                .expect("start throttle timer");
        });
    } else {
        CLOSED.store(false, Ordering::Relaxed);
    }
}

pub fn limit() -> u8 {
    LIMIT.load(Ordering::Relaxed)
}

/// Wait here while the gate is closed.
#[inline]
pub fn gate() {
    while CLOSED.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn run_timer() {
    loop {
        let p = LIMIT.load(Ordering::Relaxed) as u64;
        if p >= 100 {
            CLOSED.store(false, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(WINDOW_MS));
            continue;
        }
        CLOSED.store(false, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(WINDOW_MS * p / 100));
        CLOSED.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(WINDOW_MS * (100 - p) / 100));
    }
}

/// How many zones to build side by side under the current limit. Loading a
/// zone is single-threaded and cannot be gated, so the lanes shrink with it.
pub fn scale_lanes(lanes: usize) -> usize {
    ((lanes * limit() as usize + 99) / 100).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn holds_work_to_its_share() {
        // Spin on gated steps for a second at 50%: about half the wall time
        // should be spent waiting. Generous bounds: CI machines are noisy.
        set_limit(50);
        let t = Instant::now();
        let mut ran = Duration::ZERO;
        while t.elapsed() < Duration::from_secs(1) {
            gate();
            let s = Instant::now();
            while s.elapsed() < Duration::from_micros(500) {}
            ran += s.elapsed();
        }
        set_limit(100);
        let share = ran.as_secs_f64() / t.elapsed().as_secs_f64();
        assert!((0.35..0.65).contains(&share), "ran {share:.2} of the time");
        assert_eq!(scale_lanes(6), 6);
    }
}
