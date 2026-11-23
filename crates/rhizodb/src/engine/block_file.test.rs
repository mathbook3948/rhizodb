use std::io::ErrorKind;

use super::*;
use crate::engine::mem_fs::MemFs;
use crate::engine::os_fs::OsFs;
use crate::engine::sim_fs::SimFs;

fn page(fill: u8) -> Page {
    [fill; PAGE_SIZE]
}

fn read_all(file: &impl PageFile) -> Vec<Page> {
    let mut pages = vec![page(0); file.page_count().unwrap() as usize];
    file.read(0, &mut pages).unwrap();
    pages
}

fn holds_pages_by_number(fs: &impl FileSystem) {
    let file = fs.create("pages").unwrap();
    assert_eq!(file.page_count().unwrap(), 0);
    file.write(0, &[page(1), page(2)]).unwrap();
    file.write(4, &[page(5)]).unwrap();
    assert_eq!(file.page_count().unwrap(), 5);
    assert_eq!(
        read_all(&file),
        [page(1), page(2), page(0), page(0), page(5)]
    );
    file.write(1, &[page(7)]).unwrap();
    let mut middle = [page(9); 2];
    file.read(1, &mut middle).unwrap();
    assert_eq!(middle, [page(7), page(0)]);

    let mut past = [page(0); 2];
    assert_eq!(
        file.read(4, &mut past).unwrap_err().kind(),
        ErrorKind::UnexpectedEof
    );

    file.set_page_count(2).unwrap();
    assert_eq!(read_all(&file), [page(1), page(7)]);
    file.set_page_count(3).unwrap();
    assert_eq!(read_all(&file), [page(1), page(7), page(0)]);
    file.sync().unwrap();

    let again = fs.open("pages").unwrap();
    assert_eq!(read_all(&again), [page(1), page(7), page(0)]);
}

fn names_files(fs: &impl FileSystem) {
    fs.create("b").unwrap().write(0, &[page(2)]).unwrap();
    fs.create("a").unwrap().write(0, &[page(1)]).unwrap();
    assert_eq!(fs.names().unwrap(), ["a", "b"]);
    assert_eq!(fs.create("a").unwrap_err().kind(), ErrorKind::AlreadyExists);
    assert_eq!(fs.open("missing").unwrap_err().kind(), ErrorKind::NotFound);

    fs.rename("a", "b").unwrap();
    assert_eq!(fs.names().unwrap(), ["b"]);
    assert_eq!(read_all(&fs.open("b").unwrap()), [page(1)]);
    fs.rename("b", "c").unwrap();
    fs.remove("c").unwrap();
    assert!(fs.names().unwrap().is_empty());
    assert!(fs.remove("c").is_err());
    assert!(fs.rename("c", "d").is_err());
    fs.sync_names().unwrap();
}

#[test]
fn every_file_system_holds_pages_by_number() {
    let directory = tempfile::tempdir().unwrap();
    holds_pages_by_number(&OsFs::new(directory.path()));
    holds_pages_by_number(&MemFs::default());
    holds_pages_by_number(&SimFs::new());
}

#[test]
fn every_file_system_creates_renames_and_removes_files() {
    let directory = tempfile::tempdir().unwrap();
    names_files(&OsFs::new(directory.path()));
    names_files(&MemFs::default());
    names_files(&SimFs::new());
}

#[test]
fn an_os_page_count_leaves_out_a_partial_last_page() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("partial"), vec![3; PAGE_SIZE + 10]).unwrap();
    let file = OsFs::new(directory.path()).open("partial").unwrap();
    assert_eq!(file.page_count().unwrap(), 1);
    assert_eq!(read_all(&file), [page(3)]);
}

#[test]
fn a_page_number_whose_byte_offset_overflows_is_refused() {
    let file = MemFs::default().create("huge").unwrap();
    assert!(file.write(u64::MAX, &[page(1)]).is_err());
    assert!(byte_offset(u64::MAX).is_err());
    let directory = tempfile::tempdir().unwrap();
    let file = OsFs::new(directory.path()).create("huge").unwrap();
    assert!(file.write(u64::MAX, &[page(1)]).is_err());
}

#[test]
fn a_memory_file_keeps_its_pages_after_its_name_is_removed() {
    let fs = MemFs::default();
    let file = fs.create("gone").unwrap();
    fs.remove("gone").unwrap();
    file.write(0, &[page(4)]).unwrap();
    assert_eq!(read_all(&file), [page(4)]);
}
