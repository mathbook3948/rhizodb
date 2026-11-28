use std::fs::File;
use std::path::Path;
use std::sync::atomic::Ordering;

use rhizodb_shm::Lock;
use rhizodb_shm::Mapping;

fn open(path: &Path) -> File {
    File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap()
}

#[test]
fn a_mapping_reaches_only_its_words_and_refuses_a_short_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = open(&dir.path().join("words"));
    assert!(Mapping::new(&file, 8).is_err(), "an empty file");
    rhizodb_shm::reserve(&file, 64).unwrap();
    assert!(Mapping::new(&file, 9).is_err(), "a file one word short");
    assert!(Mapping::new(&file, 0).is_err(), "no words");
    let mapping = Mapping::new(&file, 8).unwrap();
    assert_eq!(mapping.words(), 8);
    assert!(mapping.word(7).is_some());
    assert!(mapping.word(8).is_none());
}

#[test]
fn two_mappings_of_one_file_share_their_words() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("words");
    let first = open(&path);
    rhizodb_shm::reserve(&first, 4096).unwrap();
    let a = Mapping::new(&first, 512).unwrap();
    let b = Mapping::new(&open(&path), 512).unwrap();
    a.word(511).unwrap().store(7, Ordering::Release);
    assert_eq!(b.word(511).unwrap().load(Ordering::Acquire), 7);
    assert_eq!(
        b.word(5)
            .unwrap()
            .compare_exchange(0, 3, Ordering::AcqRel, Ordering::Acquire),
        Ok(0)
    );
    assert_eq!(a.word(5).unwrap().fetch_max(9, Ordering::SeqCst), 3);
    assert_eq!(b.word(5).unwrap().load(Ordering::Acquire), 9);
}

#[test]
fn reserve_grows_a_file_and_never_shrinks_it() {
    let dir = tempfile::tempdir().unwrap();
    let file = open(&dir.path().join("grown"));
    rhizodb_shm::reserve(&file, 10_000).unwrap();
    assert_eq!(file.metadata().unwrap().len(), 10_000);
    rhizodb_shm::reserve(&file, 100).unwrap();
    assert_eq!(file.metadata().unwrap().len(), 10_000);
}

#[test]
fn a_file_has_one_identity_through_every_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("one");
    let first = open(&path);
    let link = dir.path().join("link");
    std::fs::hard_link(&path, &link).unwrap();
    let id = rhizodb_shm::file_id(&first).unwrap();
    assert_eq!(rhizodb_shm::file_id(&open(&link)).unwrap(), id);
    assert_eq!(rhizodb_shm::file_id(&open(&path)).unwrap(), id);
    assert_ne!(
        rhizodb_shm::file_id(&open(&dir.path().join("other"))).unwrap(),
        id
    );
}

#[test]
fn a_temporary_directory_is_local() {
    let dir = tempfile::tempdir().unwrap();
    assert!(rhizodb_shm::is_local(&open(&dir.path().join("here"))).unwrap());
}

#[test]
fn a_range_must_hold_a_byte_and_end_before_two_to_the_sixty_three() {
    let dir = tempfile::tempdir().unwrap();
    let file = open(&dir.path().join("locks"));
    assert!(rhizodb_shm::try_lock(&file, 0, 0, Lock::Shared).is_err());
    assert!(rhizodb_shm::try_lock(&file, i64::MAX as u64, 1, Lock::Shared).is_err());
    assert!(rhizodb_shm::try_lock(&file, i64::MAX as u64 - 1, 1, Lock::Shared).unwrap());
}

#[test]
fn locks_on_disjoint_ranges_do_not_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("locks");
    let a = open(&path);
    assert!(rhizodb_shm::try_lock(&a, 100, 1, Lock::Exclusive).unwrap());
    assert!(rhizodb_shm::try_lock(&a, 101, 1, Lock::Exclusive).unwrap());
    rhizodb_shm::unlock(&a, 100, 2).unwrap();
}

// Open-file-description locks belong to the handle, so two handles of one process conflict,
// which lets threads of one process take turns and lets the tests run in one process.
#[cfg(any(target_os = "linux", target_os = "android", windows))]
#[test]
fn locks_of_two_handles_in_one_process_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("locks");
    let (a, b) = (open(&path), open(&path));
    assert!(rhizodb_shm::try_lock(&a, 100, 1, Lock::Exclusive).unwrap());
    assert!(!rhizodb_shm::try_lock(&b, 100, 1, Lock::Shared).unwrap());
    rhizodb_shm::unlock(&a, 100, 1).unwrap();
    assert!(rhizodb_shm::try_lock(&b, 100, 1, Lock::Exclusive).unwrap());
}

// Classic `fcntl` locks belong to the process, so they never conflict within it.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
#[test]
fn classic_locks_of_one_process_never_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("locks");
    let (a, b) = (open(&path), open(&path));
    assert!(rhizodb_shm::try_lock(&a, 100, 1, Lock::Exclusive).unwrap());
    assert!(rhizodb_shm::try_lock(&b, 100, 1, Lock::Exclusive).unwrap());
    rhizodb_shm::unlock(&a, 100, 1).unwrap();
}
