//! Learning: rule credit, birth, rewrite and pruning; model pass/fail stats.

pub mod models;
pub mod rules;

/// Evidence cap: old results fade as new ones arrive.
pub const WINDOW: f64 = 20.0;

/// Rounds half up to two decimals, as run.js's `Math.round(x * 100) / 100` does for x >= 0.
pub fn r2(x: f64) -> f64 {
    ((x * 100.0 + 0.5) as i64) as f64 / 100.0
}

/// Scales a pass/fail pair down to `WINDOW` observations, keeping its ratio.
pub fn cap_pair(x: f64, y: f64) -> (f64, f64) {
    if x + y > WINDOW {
        (r2(x * WINDOW / (x + y)), r2(y * WINDOW / (x + y)))
    } else {
        (r2(x), r2(y))
    }
}
