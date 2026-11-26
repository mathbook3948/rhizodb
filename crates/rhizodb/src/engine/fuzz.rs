use std::sync::Arc;
use std::sync::OnceLock;

use super::block_file::BlockFile;
use super::block_file::BlockSize;
use super::block_file::FileSystem;
use super::error::EngineError;
use super::header::Header;
use super::layout::BLOCK;
use super::layout::BlockRef;
use super::layout::WAL_BLOCK;
use super::mem_fs::MemFs;
use super::page::PageHeader;
use super::store::CheckpointMode;
use super::store::Engine;
use super::store::EngineOptions;
use super::wal_format::Body;
use super::wal_format::Op;
use super::wal_format::SegmentHeader;
use super::wal_format::batch_record;
use super::wal_format::checkpoint_record;
use super::wal_format::decode_record;
use super::wal_format::seal_record;
use crate::storage::Entry;
use crate::storage::ReadTransaction;
use crate::storage::Storage;
use crate::storage::StorageError;
use crate::storage::Table;
use crate::storage::WriteTransaction;

fn block_of(bytes: &[u8], size: usize) -> Vec<u8> {
    let mut block = bytes.to_vec();
    block.resize(size, 0);
    block
}

/// Decodes `bytes` with the engine decoder its first byte picks, and fails when a decoded value
/// does not encode back to the bytes it came from. A panic anywhere is a bug the fuzzer reports.
pub(crate) fn decode_formats(bytes: &[u8]) -> Result<(), String> {
    let Some((&choice, rest)) = bytes.split_first() else {
        return Ok(());
    };
    match choice % 5 {
        0 => {
            let block = block_of(rest, BLOCK);
            if let Ok(header) = Header::decode(&block)
                && header.encode() != block
            {
                return Err(format!("header {header:?} encodes to other bytes"));
            }
        }
        1 => {
            let block = block_of(rest, WAL_BLOCK);
            if let Ok(header) = SegmentHeader::decode(&block)
                && header.encode() != block
            {
                return Err(format!("segment header {header:?} encodes to other bytes"));
            }
        }
        2 => {
            let at = rest
                .get(12..20)
                .and_then(|raw| <[u8; 8]>::try_from(raw).ok())
                .map(u64::from_le_bytes)
                .and_then(|raw| BlockRef::from_raw(raw).ok())
                .unwrap_or(BlockRef::NULL);
            let blocks = rest.len().div_ceil(BLOCK).max(1);
            let page = block_of(rest, blocks * BLOCK);
            if let Ok(header) = PageHeader::open(0, &page, at) {
                let mut again = page.clone();
                header
                    .seal(0, &mut again)
                    .map_err(|error| error.to_string())?;
                if again != page {
                    return Err(format!("page header {header:?} seals to other bytes"));
                }
            }
        }
        _ => {
            let mut record = rest.to_vec();
            if choice % 5 == 4 && record.len() >= 54 {
                seal_record(0, &mut record).map_err(|error| error.to_string())?;
            }
            if let Ok((decoded, length)) = decode_record(0, &record) {
                let mut again = match &decoded.body {
                    Body::Batch(ops) => batch_record(
                        decoded.lsn,
                        decoded.hlc,
                        decoded.txn,
                        ops.iter().map(Op::borrowed),
                    )
                    .map_err(|error| error.to_string())?,
                    Body::Checkpoint { lsn, generation } => {
                        checkpoint_record(decoded.lsn, decoded.hlc, *lsn, *generation)
                    }
                };
                again[20..28].copy_from_slice(&decoded.term.to_le_bytes());
                again[36..52].copy_from_slice(&decoded.txn.to_le_bytes());
                seal_record(0, &mut again).map_err(|error| error.to_string())?;
                if again != record[..length] {
                    return Err(format!("record {decoded:?} encodes to other bytes"));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "fuzz.test.rs"]
mod tests;

const FILE: &str = "fuzzed.rhizo";

fn fuzz_options() -> EngineOptions {
    EngineOptions {
        segment_target: 1 << 20,
        memtable_limit: u64::MAX,
        wal_limit: u64::MAX,
        checkpoint: CheckpointMode::Inline,
        cache_pages: 64,
        reclaim_slack: 64 << 10,
        stage_floor: 8,
    }
}

type Image = (Vec<u8>, Vec<(Table, Entry)>);

fn everything(engine: &Engine<MemFs>) -> Result<Vec<(Table, Entry)>, StorageError> {
    let read = engine.begin_read()?;
    let mut all = Vec::new();
    for table in Table::ALL {
        for (key, value) in read.scan_prefix(table, &[])? {
            if read.get(table, &key)?.as_deref() != Some(value.as_slice()) {
                return Err(EngineError::corrupt("a get differs from the scan").into());
            }
            all.push((table, (key, value)));
        }
    }
    Ok(all)
}

fn build_image() -> Result<Image, String> {
    let fs = Arc::new(MemFs::default());
    let engine =
        Engine::open(Arc::clone(&fs), FILE, fuzz_options()).map_err(|error| error.to_string())?;
    let mut write = engine.begin_write().map_err(|error| error.to_string())?;
    for index in 0..400u32 {
        let key = [
            index.to_be_bytes().as_slice(),
            &vec![b'k'; (index % 5) as usize * 300],
        ]
        .concat();
        let value = vec![index as u8; [3, 900, 1500, 9000][(index % 4) as usize]];
        let table = Table::ALL[(index % 3) as usize];
        write
            .put(table, &key, &value)
            .map_err(|error| error.to_string())?;
    }
    write.commit().map_err(|error| error.to_string())?;
    engine.checkpoint().map_err(|error| error.to_string())?;
    let expected = everything(&engine).map_err(|error| error.to_string())?;
    drop(engine);
    let file = fs
        .open(FILE, BlockSize::new(BLOCK).ok_or("block size")?)
        .map_err(|error| error.to_string())?;
    let blocks = file.block_count().map_err(|error| error.to_string())?;
    let mut image = vec![0; blocks as usize * BLOCK];
    file.read(0, &mut image)
        .map_err(|error| error.to_string())?;
    Ok((image, expected))
}

fn image() -> Result<&'static Image, String> {
    static IMAGE: OnceLock<Result<Image, String>> = OnceLock::new();
    IMAGE
        .get_or_init(build_image)
        .as_ref()
        .map_err(Clone::clone)
}

/// Damages a cleanly closed database file by `bytes`, read as pairs of a three-byte offset and a
/// byte to xor there, opens it, and reads every entry. Opening or reading may fail, but a panic,
/// or entries that differ from the undamaged file's without an error, is a bug.
pub(crate) fn open_damaged_file(bytes: &[u8]) -> Result<(), String> {
    let (pristine, expected) = image()?;
    let mut damaged = pristine.clone();
    for pair in bytes.as_chunks::<4>().0 {
        let offset =
            (usize::from(pair[0]) << 16 | usize::from(pair[1]) << 8 | usize::from(pair[2]))
                % damaged.len();
        damaged[offset] ^= pair[3];
    }
    let fs = Arc::new(MemFs::default());
    let size = BlockSize::new(BLOCK).ok_or("block size")?;
    let file = fs.create(FILE, size).map_err(|error| error.to_string())?;
    file.write(0, &damaged).map_err(|error| error.to_string())?;
    drop(file);
    let Ok(engine) = Engine::open(fs, FILE, fuzz_options()) else {
        return Ok(());
    };
    match everything(&engine) {
        Ok(found) if &found != expected => {
            Err("damage changed what reads return without an error".to_owned())
        }
        Ok(_) => engine.check().map(drop).or(Ok(())),
        Err(_) => Ok(()),
    }
}
