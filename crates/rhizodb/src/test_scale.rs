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

/// The seed a sampled test runs with: `RHIZODB_TEST_SEED` when set, so a failure replays, and
/// otherwise one drawn from the clock. It is written to stderr, past the test harness's capture.
pub(crate) fn seed() -> u64 {
    let seed = std::env::var("RHIZODB_TEST_SEED")
        .ok()
        .and_then(|seed| seed.parse().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos() as u64)
        });
    let thread = std::thread::current();
    eprintln!(
        "{} runs with RHIZODB_TEST_SEED={seed}",
        thread.name().unwrap_or("a test")
    );
    seed
}
