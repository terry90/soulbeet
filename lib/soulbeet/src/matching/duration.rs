//! How a file's length counts in its match.

/// The weight of `duration_score` in a match score, when both lengths are known.
pub const DURATION_WEIGHT: f64 = 0.3;

/// Full credit up to this many seconds of difference, none from `DURATION_NONE_S` on.
const DURATION_FULL_S: u32 = 3;
const DURATION_NONE_S: u32 = 30;

pub fn duration_score(wanted_s: u32, file_s: u32) -> f64 {
    let diff = wanted_s.abs_diff(file_s);
    if diff <= DURATION_FULL_S {
        1.0
    } else if diff >= DURATION_NONE_S {
        0.0
    } else {
        1.0 - f64::from(diff - DURATION_FULL_S) / f64::from(DURATION_NONE_S - DURATION_FULL_S)
    }
}
