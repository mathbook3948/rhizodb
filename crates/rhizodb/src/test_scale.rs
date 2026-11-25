/// The environment variable that multiplies the cases of the randomized tests, for nightly runs.
pub(crate) const SCALE_ENV: &str = "RHIZODB_TEST_SCALE";

/// `fast` cases, the count CI runs, times the scale the environment sets.
pub(crate) fn cases(fast: u32) -> u32 {
    let scale = std::env::var(SCALE_ENV)
        .ok()
        .and_then(|scale| scale.parse::<u32>().ok())
        .unwrap_or(1);
    fast.saturating_mul(scale.max(1))
}
