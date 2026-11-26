use super::*;
use crate::engine::sim_fs::SimFs;
use crate::storage::Storage;
use crate::storage::WriteTransaction;
use crate::storage::conformance;
use crate::storage::conformance::Harness;
use crate::storage::model;
use crate::storage::sim_backend;
use crate::test_scale;
use proptest::prelude::*;

struct RedbFile {
    directory: tempfile::TempDir,
}

impl RedbFile {
    fn new() -> Self {
        Self {
            directory: tempfile::tempdir().unwrap(),
        }
    }
}

impl Harness for RedbFile {
    type Store = RedbStorage;

    fn open(&mut self) -> RedbStorage {
        RedbStorage::open(self.directory.path().join("graph.redb")).unwrap()
    }
}

struct RedbOnSimFs {
    fs: SimFs,
}

impl Harness for RedbOnSimFs {
    type Store = RedbStorage;

    fn open(&mut self) -> RedbStorage {
        sim_backend::storage(&self.fs).unwrap()
    }
}

fn on_sim_fs() -> RedbOnSimFs {
    RedbOnSimFs { fs: SimFs::new() }
}

#[test]
fn the_redb_backend_passes_the_storage_conformance_suite() {
    conformance::run(RedbFile::new);
}

#[test]
fn the_redb_backend_on_the_simulated_file_system_passes_the_storage_conformance_suite() {
    conformance::run(on_sim_fs);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(test_scale::cases(128)))]

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn the_redb_backend_matches_the_versioned_model(ops in model::ops()) {
        model::check(&mut RedbFile::new(), &ops)?;
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn the_redb_backend_on_the_simulated_file_system_matches_the_versioned_model(ops in model::ops()) {
        model::check(&mut on_sim_fs(), &ops)?;
    }
}

fn put_all(storage: &RedbStorage, table: Table, entries: &[(&[u8], &[u8])]) {
    let mut write = storage.begin_write().unwrap();
    for (key, value) in entries {
        write.put(table, key, value).unwrap();
    }
    write.commit().unwrap();
}

#[derive(Debug, Default)]
struct Faults {
    bytes: std::sync::Mutex<Vec<u8>>,
    operations: std::sync::atomic::AtomicU64,
    panic_at: std::sync::atomic::AtomicU64,
}

#[derive(Clone, Debug, Default)]
struct PanickingBackend(std::sync::Arc<Faults>);

impl PanickingBackend {
    fn panic_after(&self, operations: u64) {
        let now = self.0.operations.load(std::sync::atomic::Ordering::SeqCst);
        self.0
            .panic_at
            .store(now + operations, std::sync::atomic::Ordering::SeqCst);
    }

    fn stop_panicking(&self) {
        self.0
            .panic_at
            .store(u64::MAX, std::sync::atomic::Ordering::SeqCst);
    }

    fn operation(&self) {
        let count = self
            .0
            .operations
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if count >= self.0.panic_at.load(std::sync::atomic::Ordering::SeqCst) {
            panic!("injected panic at backend operation {count}");
        }
    }

    fn fresh() -> Self {
        let backend = Self::default();
        backend.stop_panicking();
        backend
    }
}

impl redb::StorageBackend for PanickingBackend {
    fn len(&self) -> Result<u64, redb::BackendError> {
        Ok(self.0.bytes.lock().unwrap().len() as u64)
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> Result<(), redb::BackendError> {
        self.operation();
        let bytes = self.0.bytes.lock().unwrap();
        let start = offset as usize;
        out.copy_from_slice(&bytes[start..start + out.len()]);
        Ok(())
    }

    fn set_len(&self, len: u64) -> Result<(), redb::BackendError> {
        self.operation();
        self.0.bytes.lock().unwrap().resize(len as usize, 0);
        Ok(())
    }

    fn sync_data(&self) -> Result<(), redb::BackendError> {
        self.operation();
        Ok(())
    }

    fn write(&self, offset: u64, data: &[u8]) -> Result<(), redb::BackendError> {
        self.operation();
        let mut bytes = self.0.bytes.lock().unwrap();
        let start = offset as usize;
        bytes[start..start + data.len()].copy_from_slice(data);
        Ok(())
    }
}

fn quietly<R>(work: impl FnOnce() -> R) -> R {
    static QUIET: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one_hook_at_a_time = QUIET
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
    std::panic::set_hook(hook);
    result.unwrap_or_else(|payload| std::panic::resume_unwind(payload))
}

fn is_poisoned(error: &StorageError) -> bool {
    error.is_corrupt()
        && std::error::Error::source(error).is_some_and(|source| source.is::<PoisonedError>())
}

fn is_panic(error: &StorageError) -> bool {
    error.is_corrupt()
        && std::error::Error::source(error).is_some_and(|source| source.is::<PanicError>())
}

fn assert_refuses_everything(storage: &RedbStorage, earlier: &RedbRead) {
    assert!(is_poisoned(&storage.begin_read().unwrap_err()));
    assert!(is_poisoned(&storage.begin_write().err().unwrap()));
    assert!(is_poisoned(&earlier.get(Table::Nodes, b"a").unwrap_err()));
    assert!(is_poisoned(
        &earlier.scan_prefix(Table::Nodes, b"").unwrap_err()
    ));
    let mut visited = 0;
    let visit = earlier.visit_prefix(Table::Nodes, b"", |_, _| {
        visited += 1;
        Ok::<_, StorageError>(())
    });
    assert!(is_poisoned(&visit.unwrap_err()));
    assert_eq!(visited, 0);
}

fn many(storage: &RedbStorage) {
    let keys: Vec<[u8; 8]> = (0u64..4096).map(u64::to_be_bytes).collect();
    let value = [7u8; 64];
    let entries: Vec<(&[u8], &[u8])> = keys
        .iter()
        .map(|key| (key.as_slice(), value.as_slice()))
        .collect();
    put_all(storage, Table::Relationships, &entries);
}

fn committed(backend: &PanickingBackend) -> Vec<Entry> {
    let storage = RedbStorage::with_backend(backend.clone()).unwrap();
    storage
        .begin_read()
        .unwrap()
        .scan_prefix(Table::Nodes, b"")
        .unwrap()
}

#[test]
fn a_panic_in_a_read_fails_as_corrupt_data_and_poisons_the_database() {
    let backend = PanickingBackend::fresh();
    let storage = RedbStorage::with_backend(backend.clone()).unwrap();
    put_all(&storage, Table::Nodes, &[(b"a", b"1"), (b"b", b"2")]);
    many(&storage);
    drop(storage);
    let storage = RedbStorage::with_uncached_backend(backend.clone()).unwrap();
    let earlier = storage.begin_read().unwrap();
    backend.panic_after(0);
    let error = quietly(|| earlier.get(Table::Relationships, &4095u64.to_be_bytes())).unwrap_err();
    backend.stop_panicking();
    assert!(is_panic(&error), "{error}");
    assert_refuses_everything(&storage, &earlier);
    quietly(|| {
        drop(earlier);
        drop(storage);
    });
    assert_eq!(
        committed(&backend),
        [
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"2".to_vec())
        ]
    );
}

#[test]
fn a_panic_anywhere_in_a_commit_poisons_the_database_and_reopens_to_one_of_the_two_states() {
    let before = vec![
        (b"a".to_vec(), b"1".to_vec()),
        (b"b".to_vec(), b"2".to_vec()),
    ];
    let after = vec![
        (b"a".to_vec(), b"1".to_vec()),
        (b"c".to_vec(), b"3".to_vec()),
    ];
    let mut panicked = 0;
    for operations in 0.. {
        let backend = PanickingBackend::fresh();
        let storage = RedbStorage::with_backend(backend.clone()).unwrap();
        put_all(&storage, Table::Nodes, &[(b"a", b"1"), (b"b", b"2")]);
        let earlier = storage.begin_read().unwrap();
        let mut write = storage.begin_write().unwrap();
        write.put(Table::Nodes, b"c", b"3").unwrap();
        write.delete(Table::Nodes, b"b").unwrap();
        backend.panic_after(operations);
        let outcome = quietly(|| write.commit());
        backend.stop_panicking();
        match outcome {
            Ok(()) => {
                drop(earlier);
                drop(storage);
                assert_eq!(committed(&backend), after);
                break;
            }
            Err(error) => {
                assert!(is_panic(&error), "{error}");
                panicked += 1;
                assert_refuses_everything(&storage, &earlier);
                quietly(|| {
                    drop(earlier);
                    drop(storage);
                });
                let reopened = committed(&backend);
                assert!(
                    reopened == before || reopened == after,
                    "a panic after {operations} backend operations of the commit reopened to {reopened:?}"
                );
            }
        }
    }
    assert!(panicked > 0);
}

#[test]
fn a_panic_in_a_write_poisons_the_transaction_and_drops_it_without_a_write() {
    let backend = PanickingBackend::fresh();
    let storage = RedbStorage::with_backend(backend.clone()).unwrap();
    put_all(&storage, Table::Nodes, &[(b"a", b"1")]);
    drop(storage);
    let storage = RedbStorage::with_backend(backend.clone()).unwrap();
    let earlier = storage.begin_read().unwrap();
    let mut write = storage.begin_write().unwrap();
    backend.panic_after(0);
    let error = quietly(|| write.put(Table::Nodes, b"b", b"2")).unwrap_err();
    backend.stop_panicking();
    assert!(is_panic(&error), "{error}");
    assert!(is_poisoned(
        &write.put(Table::Nodes, b"c", b"3").unwrap_err()
    ));
    assert!(is_poisoned(&write.get(Table::Nodes, b"a").unwrap_err()));
    let operations = backend
        .0
        .operations
        .load(std::sync::atomic::Ordering::SeqCst);
    quietly(|| drop(write));
    assert_refuses_everything(&storage, &earlier);
    quietly(|| {
        drop(earlier);
        drop(storage);
    });
    assert_eq!(
        backend
            .0
            .operations
            .load(std::sync::atomic::Ordering::SeqCst),
        operations,
        "a poisoned database wrote while it closed"
    );
    assert_eq!(committed(&backend), [(b"a".to_vec(), b"1".to_vec())]);
}

#[test]
fn a_poisoned_write_transaction_fails_to_commit_and_to_roll_back() {
    for commit in [true, false] {
        let backend = PanickingBackend::fresh();
        let storage = RedbStorage::with_backend(backend.clone()).unwrap();
        put_all(&storage, Table::Nodes, &[(b"a", b"1")]);
        many(&storage);
        drop(storage);
        let storage = RedbStorage::with_uncached_backend(backend.clone()).unwrap();
        let earlier = storage.begin_read().unwrap();
        let write = storage.begin_write().unwrap();
        backend.panic_after(0);
        let read = quietly(|| earlier.get(Table::Relationships, &4095u64.to_be_bytes()));
        assert!(is_panic(&read.unwrap_err()));
        backend.stop_panicking();
        let ended = quietly(|| {
            if commit {
                write.commit()
            } else {
                write.rollback()
            }
        });
        assert!(is_poisoned(&ended.unwrap_err()));
        quietly(|| {
            drop(earlier);
            drop(storage);
        });
        assert_eq!(committed(&backend), [(b"a".to_vec(), b"1".to_vec())]);
    }
}

struct PanicsWhenDropped;

impl Drop for PanicsWhenDropped {
    fn drop(&mut self) {
        panic!("a redb value panicked while it was dropped");
    }
}

#[test]
fn a_poisoned_database_survives_a_value_that_panics_while_it_is_dropped() {
    let poison = Poison::default();
    let caught = quietly(|| poison.guard(|| panic!("redb panicked")));
    assert!(caught.is_err());
    assert!(poison.is_set());
    quietly(|| poison.release(PanicsWhenDropped));
}
