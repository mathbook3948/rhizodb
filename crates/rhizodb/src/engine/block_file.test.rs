use std::io::ErrorKind;

use super::*;
use crate::engine::mem_fs::MemFs;
use crate::engine::os_fs::OsFs;
use crate::engine::sim_fs::SimFs;

const BLOCK: usize = 4096;

type Block = [u8; BLOCK];

fn size() -> BlockSize {
    BlockSize::new(BLOCK).unwrap()
}

fn page(fill: u8) -> Block {
    [fill; BLOCK]
}

fn read_all(file: &impl BlockFile) -> Vec<Block> {
    let mut pages = vec![page(0); file.block_count().unwrap() as usize];
    file.read(0, pages.as_flattened_mut()).unwrap();
    pages
}

fn holds_blocks_by_number(fs: &impl FileSystem) {
    let file = fs.create("pages", size()).unwrap();
    assert_eq!(file.block_count().unwrap(), 0);
    file.write(0, [page(1), page(2)].as_flattened()).unwrap();
    file.write(4, [page(5)].as_flattened()).unwrap();
    assert_eq!(file.block_count().unwrap(), 5);
    assert_eq!(
        read_all(&file),
        [page(1), page(2), page(0), page(0), page(5)]
    );
    file.write(1, [page(7)].as_flattened()).unwrap();
    let mut middle = [page(9); 2];
    file.read(1, middle.as_flattened_mut()).unwrap();
    assert_eq!(middle, [page(7), page(0)]);

    let mut past = [page(0); 2];
    assert_eq!(
        file.read(4, past.as_flattened_mut()).unwrap_err().kind(),
        ErrorKind::UnexpectedEof
    );

    file.set_block_count(2).unwrap();
    assert_eq!(read_all(&file), [page(1), page(7)]);
    file.set_block_count(3).unwrap();
    assert_eq!(read_all(&file), [page(1), page(7), page(0)]);
    file.sync().unwrap();

    let again = fs.open("pages", size()).unwrap();
    assert_eq!(read_all(&again), [page(1), page(7), page(0)]);
}

fn names_files(fs: &impl FileSystem) {
    fs.create("b", size())
        .unwrap()
        .write(0, [page(2)].as_flattened())
        .unwrap();
    fs.create("a", size())
        .unwrap()
        .write(0, [page(1)].as_flattened())
        .unwrap();
    assert_eq!(fs.names().unwrap(), ["a", "b"]);
    assert_eq!(
        fs.create("a", size()).unwrap_err().kind(),
        ErrorKind::AlreadyExists
    );
    assert_eq!(
        fs.open("missing", size()).unwrap_err().kind(),
        ErrorKind::NotFound
    );

    fs.rename("a", "b").unwrap();
    assert_eq!(fs.names().unwrap(), ["b"]);
    assert_eq!(read_all(&fs.open("b", size()).unwrap()), [page(1)]);
    fs.rename("b", "c").unwrap();
    fs.remove("c").unwrap();
    assert!(fs.names().unwrap().is_empty());
    assert!(fs.remove("c").is_err());
    assert!(fs.rename("c", "d").is_err());
    fs.sync_names().unwrap();
}

#[test]
fn every_file_system_holds_blocks_by_number() {
    let directory = tempfile::tempdir().unwrap();
    holds_blocks_by_number(&OsFs::new(directory.path()));
    holds_blocks_by_number(&MemFs::default());
    holds_blocks_by_number(&SimFs::new());
}

#[test]
fn every_file_system_creates_renames_and_removes_files() {
    let directory = tempfile::tempdir().unwrap();
    names_files(&OsFs::new(directory.path()));
    names_files(&MemFs::default());
    names_files(&SimFs::new());
}

#[test]
fn an_os_block_count_leaves_out_a_partial_last_page() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("partial"), vec![3; BLOCK + 10]).unwrap();
    let file = OsFs::new(directory.path()).open("partial", size()).unwrap();
    assert_eq!(file.block_count().unwrap(), 1);
    assert_eq!(read_all(&file), [page(3)]);
}

#[test]
fn a_block_number_whose_byte_offset_overflows_is_refused() {
    let file = MemFs::default().create("huge", size()).unwrap();
    assert!(file.write(u64::MAX, [page(1)].as_flattened()).is_err());
    assert!(size().offset(u64::MAX).is_err());
    let directory = tempfile::tempdir().unwrap();
    let file = OsFs::new(directory.path()).create("huge", size()).unwrap();
    assert!(file.write(u64::MAX, [page(1)].as_flattened()).is_err());
}

#[test]
fn a_memory_file_keeps_its_blocks_after_its_name_is_removed() {
    let fs = MemFs::default();
    let file = fs.create("gone", size()).unwrap();
    fs.remove("gone").unwrap();
    file.write(0, [page(4)].as_flattened()).unwrap();
    assert_eq!(read_all(&file), [page(4)]);
}

fn refuses_partial_blocks(fs: &impl FileSystem) {
    let file = fs.create("partial", size()).unwrap();
    for length in [1, BLOCK - 1, BLOCK + SECTOR_SIZE] {
        let error = file.write(0, &vec![1; length]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }
    file.write(0, &[1; 2 * BLOCK]).unwrap();
    assert_eq!(
        file.read(0, &mut [0; BLOCK / 2]).unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
}

fn holds_blocks_of_any_size(fs: &impl FileSystem) {
    for bytes in [SECTOR_SIZE, 1 << 16, 1 << 20] {
        let size = BlockSize::new(bytes).unwrap();
        let name = format!("blocks-{bytes}");
        let file = fs.create(&name, size).unwrap();
        let mut written = vec![0; 2 * bytes];
        written[..bytes].fill(5);
        written[bytes..].fill(6);
        file.write(1, &written).unwrap();
        file.sync().unwrap();
        assert_eq!(file.block_count().unwrap(), 3);
        let mut read = vec![9; 3 * bytes];
        fs.open(&name, size).unwrap().read(0, &mut read).unwrap();
        assert!(read[..bytes].iter().all(|byte| *byte == 0));
        assert_eq!(read[bytes..], written);
        file.set_block_count(1).unwrap();
        assert_eq!(file.block_count().unwrap(), 1);
    }
}

#[test]
fn every_file_system_refuses_a_buffer_of_partial_blocks() {
    let directory = tempfile::tempdir().unwrap();
    refuses_partial_blocks(&OsFs::new(directory.path()));
    refuses_partial_blocks(&MemFs::default());
    refuses_partial_blocks(&SimFs::new());
}

#[test]
fn every_file_system_holds_blocks_of_any_size() {
    let directory = tempfile::tempdir().unwrap();
    holds_blocks_of_any_size(&OsFs::new(directory.path()));
    holds_blocks_of_any_size(&MemFs::default());
    holds_blocks_of_any_size(&SimFs::new());
}

#[test]
fn a_block_size_is_a_power_of_two_from_a_sector_to_a_gibibyte() {
    for bytes in [SECTOR_SIZE, 4096, 1 << 20, 1 << 30] {
        assert_eq!(BlockSize::new(bytes).unwrap().bytes(), bytes);
    }
    for bytes in [0, 1, 256, 4095, 4097, 3 << 12, 1 << 31] {
        assert_eq!(BlockSize::new(bytes), None);
    }
}
