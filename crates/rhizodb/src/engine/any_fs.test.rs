use super::*;

#[test]
fn a_file_of_the_operating_system_reaches_its_identity_and_locks_through_any_fs() {
    let directory = tempfile::tempdir().unwrap();
    let fs = AnyFs::Os(OsFs::new(directory.path()));
    let size = BlockSize::new(4096).unwrap();
    drop(fs.create("one", size).unwrap());
    drop(fs.create("other", size).unwrap());
    std::fs::hard_link(directory.path().join("one"), directory.path().join("link")).unwrap();
    let one = fs.open("one", size).unwrap();
    let identity = one.identity().unwrap();
    assert_ne!(identity, (0, 0));
    assert_eq!(fs.open("link", size).unwrap().identity().unwrap(), identity);
    assert_ne!(
        fs.open("other", size).unwrap().identity().unwrap(),
        identity
    );
    assert!(!one.held_elsewhere().unwrap());
}
