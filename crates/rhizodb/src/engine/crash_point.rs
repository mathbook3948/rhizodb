//! Named points of the multi-process write path where a build made with `--cfg crash_points`
//! aborts its process, so a test kills a writer at an exact step of a commit, a hand-over of the
//! log's tail, or a checkpoint. Other builds compile every point to nothing.

/// Aborts the process when `RHIZODB_CRASH_POINT` names `name` and this is the hit it counts
/// to, written as `<name>:<hit>`, in a build made with `--cfg crash_points`.
#[cfg(crash_points)]
pub(crate) fn reached(name: &str) {
    use std::sync::OnceLock;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    static TARGET: OnceLock<Option<(String, u64)>> = OnceLock::new();
    static HITS: AtomicU64 = AtomicU64::new(0);
    let target = TARGET.get_or_init(|| {
        let value = std::env::var("RHIZODB_CRASH_POINT").ok()?;
        let (point, hit) = value.rsplit_once(':')?;
        Some((point.to_owned(), hit.parse().ok()?))
    });
    if let Some((point, hit)) = target
        && point == name
        && HITS.fetch_add(1, Ordering::SeqCst) + 1 == *hit
    {
        std::process::abort();
    }
}

/// Does nothing outside a build made with `--cfg crash_points`.
#[cfg(not(crash_points))]
#[inline(always)]
pub(crate) fn reached(_name: &str) {}

/// Shrinks the log's segments, the memtable, and the checkpoint stages when
/// `RHIZODB_CRASH_TUNING` is set, in a build made with `--cfg crash_points`, so a short test
/// reaches every point of a rollover and a staged checkpoint.
#[cfg(crash_points)]
pub(crate) fn tuned(options: super::store::EngineOptions) -> super::store::EngineOptions {
    if std::env::var_os("RHIZODB_CRASH_TUNING").is_none() {
        return options;
    }
    super::store::EngineOptions {
        segment_target: 64 << 10,
        memtable_limit: 256 << 10,
        wal_limit: 1 << 20,
        stage_floor: 8,
        ..options
    }
}

/// Leaves `options` as they are outside a build made with `--cfg crash_points`.
#[cfg(not(crash_points))]
#[inline(always)]
pub(crate) fn tuned(options: super::store::EngineOptions) -> super::store::EngineOptions {
    options
}
