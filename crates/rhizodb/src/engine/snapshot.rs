use std::io;
use std::io::Read;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use super::block_file::BlockFile;
use super::block_file::FileSystem;
use super::changes::EpochId;
use super::codec::Reader;
use super::codec::checksum;
use super::codec::put_u16;
use super::codec::put_u32;
use super::codec::put_u64;
use super::error::EngineError;
use super::header::Header;
use super::header::Replication;
use super::layout::BLOCK;
use super::layout::physical_block;
use super::store::Engine;
use super::store::EngineOptions;
use super::store::Pinned;
use super::store::block_size;
use super::store::load;
use super::store::new_incarnation;
use super::store::open_or_create_staging;
use super::store::random_u64;
use super::store::read_header;
use super::store::replica;
use super::store::staging_name;
use crate::error::ErrorKind;
use crate::storage::StorageError;

const MAGIC: [u8; 8] = *b"RHZSNAP\0";
const VERSION: u16 = 1;
const PREFIX: usize = 24;
const FIXED: usize = 48;
const SUM: usize = 8;
const FRAME_SEED: u64 = 0x5a4e_4150_5348_4f54;
const WRITE_BLOCKS: usize = 256;

/// The payload bytes of every chunk but the last.
pub(crate) const CHUNK: usize = 1 << 20;

const HISTORY_LIMIT: usize = 1 << 20;

fn frame_seed(nonce: u64, index: u64) -> u64 {
    nonce ^ FRAME_SEED ^ index.wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

fn frames(logical: u64) -> u64 {
    logical.div_ceil(CHUNK as u64)
}

fn to_usize(value: u64) -> Result<usize, EngineError> {
    usize::try_from(value).map_err(|_| EngineError::TooLarge)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Range {
    first: u64,
    count: u64,
}

fn live_ranges(
    pinned_extents: &super::extent::ExtentTable,
) -> Result<(Vec<Range>, u64), EngineError> {
    let mut ranges: Vec<Range> = Vec::new();
    let mut file_blocks = 2;
    for entry in pinned_extents.entries.values() {
        file_blocks = file_blocks.max(physical_block(entry.slot, entry.written)?);
        for (start, count) in entry.used.ranges() {
            ranges.push(Range {
                first: physical_block(entry.slot, start)?,
                count: u64::from(count),
            });
        }
    }
    ranges.sort_unstable_by_key(|range| range.first);
    let mut merged: Vec<Range> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            Some(last) if last.first + last.count == range.first => last.count += range.count,
            _ => merged.push(range),
        }
    }
    Ok((merged, file_blocks))
}

/// A pinned checkpoint written out as a stream of chunks, each followed by a checksum of its
/// own, read at any offset so a cut transfer resumes where it stopped.
#[derive(Debug)]
pub(crate) struct Export<'a, F: FileSystem + 'static> {
    pinned: Pinned<'a, F>,
    nonce: u64,
    manifest: Vec<u8>,
    ranges: Vec<Range>,
    starts: Vec<u64>,
    logical: u64,
    frame: Mutex<Option<(u64, Vec<u8>)>>,
}

impl<'a, F: FileSystem + 'static> Export<'a, F> {
    /// Writes out the checkpoint `pinned` holds, with `history` carried along for the receiver.
    pub(crate) fn new(
        pinned: Pinned<'a, F>,
        nonce: u64,
        history: &[u8],
    ) -> Result<Self, EngineError> {
        let (ranges, file_blocks) = live_ranges(&pinned.extents)?;
        let mut starts = Vec::with_capacity(ranges.len());
        let mut blocks = 0;
        for range in &ranges {
            starts.push(blocks);
            blocks += range.count;
        }
        let mut manifest = Vec::new();
        manifest.extend_from_slice(&MAGIC);
        put_u64(&mut manifest, nonce);
        put_u64(&mut manifest, 0);
        put_u16(&mut manifest, VERSION);
        put_u16(&mut manifest, 0);
        put_u32(&mut manifest, 0);
        put_u64(&mut manifest, pinned.header.checkpoint_lsn);
        put_u64(&mut manifest, file_blocks);
        put_u32(
            &mut manifest,
            u32::try_from(history.len())
                .ok()
                .filter(|length| *length as usize <= HISTORY_LIMIT)
                .ok_or(EngineError::TooLarge)?,
        );
        manifest.extend_from_slice(history);
        put_u64(&mut manifest, ranges.len() as u64);
        for range in &ranges {
            put_u64(&mut manifest, range.first);
            put_u64(&mut manifest, range.count);
        }
        manifest.extend_from_slice(&pinned.header.encode());
        manifest.resize(manifest.len().next_multiple_of(BLOCK), 0);
        let logical = manifest.len() as u64 + blocks * BLOCK as u64;
        manifest[16..24].copy_from_slice(&logical.to_le_bytes());
        Ok(Self {
            pinned,
            nonce,
            manifest,
            ranges,
            starts,
            logical,
            frame: Mutex::new(None),
        })
    }

    /// The LSN of the newest commit the checkpoint holds.
    pub(crate) fn position(&self) -> u64 {
        self.pinned.header.checkpoint_lsn
    }

    /// The chain hash of the newest commit the checkpoint holds.
    pub(crate) fn chain(&self) -> u64 {
        self.pinned.header.replication.checkpoint_chain
    }

    /// The term and writer of the newest commit the checkpoint holds.
    pub(crate) fn epoch(&self) -> &EpochId {
        &self.pinned.epoch
    }

    /// The database whose history the checkpoint holds, or zero when it holds no change.
    pub(crate) fn database(&self) -> u128 {
        lineage(&self.pinned.header)
    }

    /// The bytes of the whole stream.
    pub(crate) fn len(&self) -> u64 {
        self.logical + frames(self.logical) * SUM as u64
    }

    /// Copies the stream's bytes from `offset` on into `buffer`, and returns how many it copied,
    /// which is zero only at or past the end.
    pub(crate) fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<usize, EngineError> {
        let total = self.len();
        let frame_len = (CHUNK + SUM) as u64;
        let mut done = 0;
        let _held = self.pinned.hold()?;
        let mut cache = self.frame.lock().unwrap_or_else(PoisonError::into_inner);
        while done < buffer.len() && offset + (done as u64) < total {
            let at = offset + done as u64;
            let index = at / frame_len;
            let within = to_usize(at % frame_len)?;
            if cache.as_ref().is_none_or(|(cached, _)| *cached != index) {
                *cache = Some((index, self.frame_bytes(index)?));
            }
            let Some((_, frame)) = cache.as_ref() else {
                break;
            };
            let rest = frame.get(within..).unwrap_or_default();
            let count = rest.len().min(buffer.len() - done);
            buffer[done..done + count].copy_from_slice(&rest[..count]);
            done += count;
        }
        Ok(done)
    }

    fn frame_bytes(&self, index: u64) -> Result<Vec<u8>, EngineError> {
        let start = index * CHUNK as u64;
        let end = (start + CHUNK as u64).min(self.logical);
        let mut payload = vec![0; to_usize(end - start)?];
        let manifest = self.manifest.len() as u64;
        if start < manifest {
            let from = to_usize(start)?;
            let until = to_usize(end.min(manifest))?;
            payload[..until - from].copy_from_slice(&self.manifest[from..until]);
        }
        let mut block = start.max(manifest).saturating_sub(manifest) / BLOCK as u64;
        let last = end.saturating_sub(manifest) / BLOCK as u64;
        let mut at = to_usize(start.max(manifest) - start)?;
        while block < last {
            let range = self.starts.partition_point(|first| *first <= block) - 1;
            let Range { first, count } = self.ranges[range];
            let skip = block - self.starts[range];
            let take = (count - skip).min(last - block);
            let length = to_usize(take)? * BLOCK;
            self.pinned
                .read(first + skip, &mut payload[at..at + length])?;
            at += length;
            block += take;
        }
        let sum = checksum(frame_seed(self.nonce, index), &[&payload]);
        payload.extend_from_slice(&sum.to_le_bytes());
        Ok(payload)
    }
}

/// Splits a whole stream into its logical bytes, the payload of every chunk without its
/// checksum, checking none of the checksums.
#[cfg(any(test, fuzzing))]
pub(crate) fn unseal(stream: &[u8]) -> Vec<u8> {
    stream
        .chunks(CHUNK + SUM)
        .flat_map(|frame| &frame[..frame.len().saturating_sub(SUM)])
        .copied()
        .collect()
}

/// Splits `logical` into chunks and seals each with the checksum the nonce in its prefix seeds,
/// as an export does.
#[cfg(any(test, fuzzing))]
pub(crate) fn seal(logical: &[u8]) -> Vec<u8> {
    let nonce = logical
        .get(8..16)
        .and_then(|bytes| bytes.try_into().ok())
        .map_or(0, u64::from_le_bytes);
    let mut stream = Vec::with_capacity(logical.len() + logical.len() / CHUNK * SUM + SUM);
    for (index, payload) in (0u64..).zip(logical.chunks(CHUNK)) {
        stream.extend_from_slice(payload);
        let sum = checksum(frame_seed(nonce, index), &[payload]);
        stream.extend_from_slice(&sum.to_le_bytes());
    }
    stream
}

impl<F: FileSystem + 'static> Engine<F> {
    /// Pins the last durable checkpoint and writes it out as a snapshot that carries `history`.
    pub(crate) fn export(&self, history: &[u8]) -> Result<Export<'_, F>, EngineError> {
        Export::new(self.pin()?, random_u64(), history)
    }
}

fn lineage(header: &Header) -> u128 {
    match (header.checkpoint_lsn, header.replication.origin) {
        (0, _) => 0,
        (_, 0) => header.database_id,
        (_, origin) => origin,
    }
}

fn input_error(error: io::Error) -> EngineError {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        return EngineError::corrupt("the snapshot ends before its last chunk");
    }
    EngineError::Storage(StorageError::of_kind(ErrorKind::Io, error))
}

struct Frames<R> {
    input: R,
    nonce: u64,
    logical: u64,
    index: u64,
    payload: Vec<u8>,
    at: usize,
}

impl<R: Read> Frames<R> {
    fn open(mut input: R) -> Result<Self, EngineError> {
        let mut prefix = [0; PREFIX];
        input.read_exact(&mut prefix).map_err(input_error)?;
        if prefix[..8] != MAGIC {
            return Err(EngineError::corrupt("the stream is not a snapshot"));
        }
        let mut reader = Reader::new(&prefix[8..], "the snapshot prefix");
        let nonce = reader.u64()?;
        let logical = reader.u64()?;
        let mut frames = Self {
            input,
            nonce,
            logical,
            index: 0,
            payload: prefix.to_vec(),
            at: 0,
        };
        frames.fill(PREFIX)?;
        frames.at = PREFIX;
        Ok(frames)
    }

    fn fill(&mut self, held: usize) -> Result<(), EngineError> {
        let start = self.index * CHUNK as u64;
        let length = to_usize((self.logical - start).min(CHUNK as u64))?;
        if length < held {
            return Err(EngineError::corrupt("a snapshot chunk is too short"));
        }
        self.payload.resize(length + SUM, 0);
        self.input
            .read_exact(&mut self.payload[held..])
            .map_err(input_error)?;
        let sum = self.payload.split_off(length);
        let expected = checksum(frame_seed(self.nonce, self.index), &[&self.payload]);
        if sum != expected.to_le_bytes() {
            return Err(EngineError::corrupt(format!(
                "snapshot chunk {} fails its checksum",
                self.index
            )));
        }
        self.at = 0;
        Ok(())
    }

    fn read_exact(&mut self, mut buffer: &mut [u8]) -> Result<(), EngineError> {
        while !buffer.is_empty() {
            if self.at == self.payload.len() {
                self.index += 1;
                if self.index >= frames(self.logical) {
                    return Err(EngineError::corrupt(
                        "the snapshot's contents run past its length",
                    ));
                }
                self.payload.clear();
                self.fill(0)?;
            }
            let count = (self.payload.len() - self.at).min(buffer.len());
            buffer[..count].copy_from_slice(&self.payload[self.at..self.at + count]);
            self.at += count;
            buffer = &mut buffer[count..];
        }
        Ok(())
    }

    fn take(&mut self, length: usize) -> Result<Vec<u8>, EngineError> {
        let mut bytes = Vec::with_capacity(length.min(CHUNK));
        while bytes.len() < length {
            let start = bytes.len();
            bytes.resize(start + (length - start).min(CHUNK), 0);
            self.read_exact(&mut bytes[start..])?;
        }
        Ok(bytes)
    }

    fn finished(&self) -> bool {
        self.at == self.payload.len() && self.index + 1 == frames(self.logical)
    }
}

struct Manifest {
    file_blocks: u64,
    ranges: Vec<Range>,
    header: Header,
}

fn read_manifest<R: Read>(frames: &mut Frames<R>) -> Result<Manifest, EngineError> {
    let fixed = frames.take(FIXED - PREFIX)?;
    let mut reader = Reader::new(&fixed, "the snapshot manifest");
    let version = reader.u16()?;
    if version != VERSION {
        return Err(EngineError::Unsupported(format!(
            "snapshot format version {version}"
        )));
    }
    reader.u16()?;
    reader.u32()?;
    let position = reader.u64()?;
    let file_blocks = reader.u64()?;
    let history_length = u32::from_le_bytes(
        frames
            .take(4)?
            .try_into()
            .map_err(|_| EngineError::corrupt("the snapshot manifest is truncated"))?,
    );
    if history_length as usize > HISTORY_LIMIT {
        return Err(EngineError::corrupt(
            "the snapshot carries a history longer than any export writes",
        ));
    }
    let history = frames.take(history_length as usize)?;
    let count = u64::from_le_bytes(
        frames
            .take(8)?
            .try_into()
            .map_err(|_| EngineError::corrupt("the snapshot manifest is truncated"))?,
    );
    if count > frames.logical / BLOCK as u64 || count > file_blocks {
        return Err(EngineError::corrupt(
            "the snapshot names more ranges than it holds blocks",
        ));
    }
    let listed = frames.take(
        to_usize(count)?
            .checked_mul(16)
            .ok_or(EngineError::TooLarge)?,
    )?;
    let mut reader = Reader::new(&listed, "the snapshot ranges");
    let mut ranges = Vec::with_capacity(to_usize(count)?);
    let mut end = 2;
    let mut blocks = 0u64;
    for _ in 0..count {
        let range = Range {
            first: reader.u64()?,
            count: reader.u64()?,
        };
        let last = range.first.checked_add(range.count);
        if range.count == 0 || range.first < end || last.is_none_or(|last| last > file_blocks) {
            return Err(EngineError::corrupt(
                "the snapshot's block ranges overlap or leave the file",
            ));
        }
        end = range.first + range.count;
        blocks += range.count;
        ranges.push(range);
    }
    let header = Header::decode(&frames.take(BLOCK)?)?;
    let read = FIXED + 4 + history.len() + 8 + listed.len() + BLOCK;
    frames.take(read.next_multiple_of(BLOCK) - read)?;
    let manifest = read.next_multiple_of(BLOCK) as u64;
    if Some(frames.logical)
        != blocks
            .checked_mul(BLOCK as u64)
            .map(|bytes| bytes + manifest)
    {
        return Err(EngineError::corrupt(
            "the snapshot's length differs from its blocks",
        ));
    }
    if !header.clean || header.staged_lsn != 0 || header.checkpoint_lsn != position {
        return Err(EngineError::corrupt(
            "the snapshot's header is not a clean checkpoint at its position",
        ));
    }
    Ok(Manifest {
        file_blocks,
        ranges,
        header,
    })
}

fn write_file<R: Read>(file: &impl BlockFile, input: R) -> Result<Manifest, EngineError> {
    let mut frames = Frames::open(input)?;
    let manifest = read_manifest(&mut frames)?;
    file.set_block_count(0)?;
    let mut buffer = vec![0; WRITE_BLOCKS * BLOCK];
    for range in &manifest.ranges {
        let mut done = 0;
        while done < range.count {
            let take = (range.count - done).min(WRITE_BLOCKS as u64);
            let bytes = &mut buffer[..to_usize(take)? * BLOCK];
            frames.read_exact(bytes)?;
            file.write(range.first + done, bytes)?;
            done += take;
        }
    }
    if !frames.finished() {
        return Err(EngineError::corrupt(
            "the snapshot's contents end before its length",
        ));
    }
    file.set_block_count(manifest.file_blocks)?;
    let other = 1 - manifest.header.slot();
    file.write(other, &vec![0; BLOCK])?;
    file.write(manifest.header.slot(), &manifest.header.encode())?;
    file.sync()?;
    Ok(manifest)
}

/// Writes the snapshot `input` streams into a staging file beside `name`, syncs it, checks
/// every page it holds, renames it over `name`, and opens it. Until the rename, `name` and its
/// log stay as they were, so a crash or a failure leaves the old database or none. Windows
/// renames no file that a handle holds open, so there both files are closed before the rename
/// and the database is opened again after it.
pub(crate) fn install<F: FileSystem + 'static>(
    fs: Arc<F>,
    name: &str,
    input: impl Read,
    options: EngineOptions,
    force: bool,
) -> Result<Engine<F>, EngineError> {
    let existing = match fs.open(name, block_size()?) {
        Ok(file) => {
            file.lock()?;
            Some(file)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let local = existing
        .as_ref()
        .and_then(|file| read_header(file).ok())
        .map(|(header, _)| header);
    let local_history = fs
        .open(name, block_size()?)
        .ok()
        .and_then(|file| load(file, &options).ok())
        .and_then(|loaded| loaded.newest_term().ok())
        .unwrap_or(0);
    let local_term = local
        .map_or(0, |header| header.replication.seen_term)
        .max(local_history);
    let local_lineage = local.as_ref().map_or(0, lineage);
    let staging = staging_name(name);
    let file = open_or_create_staging(&*fs, &staging)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::WouldBlock,
            "another open is creating the database file",
        )
    })?;
    file.lock()?;
    let written = write_file(&file, input).and_then(|_| {
        let mut loaded = load(file, &options)?;
        loaded.check()?;
        let snapshot_term = loaded
            .header()
            .replication
            .seen_term
            .max(loaded.newest_term()?);
        if local_term > snapshot_term && !force {
            return Err(replica::fenced(snapshot_term, local_term));
        }
        let snapshot_lineage = lineage(loaded.header());
        if local_lineage != 0 && local_lineage != snapshot_lineage && !force {
            return Err(replica::different(local_lineage, snapshot_lineage));
        }
        let header = *loaded.header();
        loaded.rewrite_header(Header {
            replication: Replication {
                incarnation: new_incarnation(),
                replica: true,
                seen_term: local_term.max(snapshot_term),
                ..header.replication
            },
            ..header
        })?;
        Ok(loaded)
    });
    let loaded = match written {
        Ok(done) => done,
        Err(error) => {
            let _ = fs.remove(&staging);
            return Err(error);
        }
    };
    if cfg!(windows) {
        drop(loaded);
        drop(existing);
        fs.rename(&staging, name)?;
        fs.sync_names()?;
        return Engine::open(fs, name, options);
    }
    fs.rename(&staging, name)?;
    fs.sync_names()?;
    drop(existing);
    Engine::open_loaded(fs, name, loaded, options)
}

#[cfg(test)]
#[path = "snapshot.test.rs"]
mod tests;
