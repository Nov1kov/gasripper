//! Throttled progress reporting shared by the long-running scanning passes (`shuffle`, `superopt`).
//!
//! Both passes iterate a list of basic-block-local work items and can run for a while on a large
//! contract, so they report the same way: stay silent below [`MIN_ITEMS`] (small inputs finish
//! instantly), otherwise log an up-front line, a periodic line throttled by [`due`] to at most once
//! per [`INTERVAL`], and a final summary. Set `RUST_LOG=warn` to silence them.

use std::time::{Duration, Instant};

/// Minimum wall-clock gap between live progress lines.
const INTERVAL: Duration = Duration::from_secs(10);

/// Below this many work items a pass stays silent — small inputs finish instantly.
pub const MIN_ITEMS: usize = 64;

/// True at most once per [`INTERVAL`], resetting `last` when it fires — so a long scan emits periodic
/// progress without flooding the log.
pub fn due(last: &mut Instant) -> bool {
    if last.elapsed() >= INTERVAL {
        *last = Instant::now();
        true
    } else {
        false
    }
}

/// Estimated seconds remaining: `elapsed` so far with fraction `done` (0..1] of the work complete.
#[inline]
pub fn eta(elapsed: Duration, done: f64) -> f64 {
    if done <= 0.0 {
        0.0
    } else {
        elapsed.mul_f64((1.0 - done) / done).as_secs_f64()
    }
}
