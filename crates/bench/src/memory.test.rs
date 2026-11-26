use std::io::Write;

use super::*;

#[cfg(target_os = "linux")]
fn read_from_disk(path: &Path) -> u64 {
    let before = disk_read_bytes().unwrap();
    let _ = fs::read(path).unwrap();
    disk_read_bytes().unwrap() - before
}

#[cfg(target_os = "linux")]
#[test]
fn evicting_a_directory_makes_the_next_read_of_each_file_go_to_the_disk() {
    let dir = std::env::temp_dir().join(format!("bench-evict-{}", std::process::id()));
    let nested = dir.join("nested");
    fs::create_dir_all(&nested).unwrap();
    let files = [dir.join("top.db"), nested.join("inner.db")];
    let size = 1u64 << 20;
    for path in &files {
        let mut file = fs::File::create(path).unwrap();
        file.write_all(&vec![7u8; size as usize]).unwrap();
        file.sync_all().unwrap();
        let _ = fs::read(path).unwrap();
    }
    for path in &files {
        assert_eq!(read_from_disk(path), 0, "{}", path.display());
    }

    evict(&dir).unwrap();

    for path in &files {
        assert!(read_from_disk(path) >= size, "{}", path.display());
    }
    fs::remove_dir_all(&dir).unwrap();
}
