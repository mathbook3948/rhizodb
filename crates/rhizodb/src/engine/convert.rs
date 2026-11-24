use std::io;
use std::sync::Arc;

use super::block_file::BlockFile;
use super::block_file::BlockSize;
use super::block_file::FileSystem;
use super::block_file::SECTOR_SIZE;
use super::error::EngineError;
use super::store::Engine;
use super::store::EngineOptions;
use super::store::staging_name;
use super::wal::segment_files;
use super::wal::segment_name;
use crate::storage::ReadTransaction;
use crate::storage::Storage;
use crate::storage::StorageError;
use crate::storage::Table;
use crate::storage::WriteTransaction;
use crate::storage::redb_backend::RedbSource;
use crate::storage::redb_backend::is_held_elsewhere;

const REDB_MAGIC: [u8; 9] = [b'r', b'e', b'd', b'b', 0x1A, 0x0A, 0xA9, 0x0D, 0x0A];

const BATCH_BYTES: usize = 16 << 20;

/// The name the engine database of a conversion is built under before it replaces the redb file.
pub(crate) fn conversion_name(name: &str) -> String {
    format!(".{name}.converting")
}

fn sector_size() -> Result<BlockSize, EngineError> {
    BlockSize::new(SECTOR_SIZE).ok_or(EngineError::TooLarge)
}

fn holds_redb<B: BlockFile>(file: &B) -> Result<bool, EngineError> {
    if file.block_count()? == 0 {
        return Ok(false);
    }
    let mut first = vec![0; SECTOR_SIZE];
    file.read(0, &mut first)?;
    Ok(first.starts_with(&REDB_MAGIC))
}

fn open_redb<F: FileSystem>(fs: &F, name: &str) -> Result<Option<F::File>, EngineError> {
    match fs.open(name, sector_size()?) {
        Ok(file) if holds_redb(&file)? => Ok(Some(file)),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn ignore_missing(outcome: io::Result<()>) -> io::Result<()> {
    match outcome {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        outcome => outcome,
    }
}

fn remove_leftovers<F: FileSystem>(fs: &F, conversion: &str) -> Result<(), EngineError> {
    ignore_missing(fs.remove(conversion))?;
    ignore_missing(fs.remove(&staging_name(conversion)))?;
    for sequence in segment_files(fs, conversion)? {
        ignore_missing(fs.remove(&segment_name(conversion, sequence)))?;
    }
    Ok(())
}

fn copy<F: FileSystem + 'static>(
    source: &RedbSource,
    engine: &Engine<F>,
    batch_bytes: usize,
) -> Result<(), StorageError> {
    let read = source.begin_read()?;
    for table in Table::ALL {
        if !read.holds(table)? {
            continue;
        }
        let mut write = Some(engine.begin_write()?);
        let mut batched = 0;
        read.visit_prefix(table, &[], |key, value| -> Result<(), StorageError> {
            let open = match &mut write {
                Some(open) => open,
                None => write.insert(engine.begin_write()?),
            };
            open.put(table, key, value)?;
            batched += key.len() + value.len();
            if batched >= batch_bytes {
                batched = 0;
                if let Some(full) = write.take() {
                    full.commit()?;
                }
            }
            Ok(())
        })?;
        if let Some(last) = write {
            last.commit()?;
        }
    }
    Ok(())
}

fn already_open() -> EngineError {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        "the database file is already open",
    )
    .into()
}

fn open_source<F: FileSystem>(
    fs: &F,
    name: &str,
    file: F::File,
) -> Result<RedbSource, EngineError> {
    let opened = match fs.path(name) {
        Some(path) => {
            drop(file);
            RedbSource::open_path(&path)
        }
        None => {
            file.lock()?;
            RedbSource::open_blocks(Arc::new(file))
        }
    };
    opened.map_err(|error| {
        if is_held_elsewhere(&error) {
            already_open()
        } else {
            error.into()
        }
    })
}

/// Replaces a redb database at `name` with an engine database of the same tables, and does
/// nothing when `name` holds no redb database. The redb file stays locked through redb's own
/// locks, its pages are checked and an unclean shutdown recovered without writing it, and the
/// copy is built under another name, closed cleanly, and renamed over it, so a crash at any
/// point leaves either the redb file untouched or the whole copy.
pub(crate) fn convert_redb<F: FileSystem + 'static>(
    fs: &Arc<F>,
    name: &str,
    options: EngineOptions,
) -> Result<(), EngineError> {
    let Some(file) = open_redb(&**fs, name)? else {
        return Ok(());
    };
    let redb = open_source(&**fs, name, file)?;
    if open_redb(&**fs, name)?.is_none() {
        return Ok(());
    }
    let conversion = conversion_name(name);
    remove_leftovers(&**fs, &conversion)?;
    let engine = Engine::open(Arc::clone(fs), &conversion, options)?;
    let half_memtable = usize::try_from(options.memtable_limit / 2).unwrap_or(BATCH_BYTES);
    copy(&redb, &engine, half_memtable.clamp(1, BATCH_BYTES))?;
    engine.close()?;
    fs.rename(&conversion, name)?;
    fs.sync_names()?;
    drop(redb);
    remove_leftovers(&**fs, &conversion)
}

#[cfg(test)]
#[path = "convert.test.rs"]
mod tests;
