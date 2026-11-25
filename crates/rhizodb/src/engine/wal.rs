use std::io;
use std::sync::Arc;

use super::block_file::BlockFile;
use super::block_file::BlockSize;
use super::block_file::FileSystem;
use super::error::EngineError;
use super::header::FORMAT_MINOR;
use super::header::Header;
use super::layout::WAL_BLOCK;
use super::wal_format::NotRecord;
use super::wal_format::Record;
use super::wal_format::SegmentHeader;
use super::wal_format::decode_record;
use super::wal_format::padded;
use super::wal_format::seal_record;

/// Which log the segments of one database file belong to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LogIdentity {
    pub(crate) database_id: u128,
    pub(crate) shard_id: u64,
    pub(crate) salt: u64,
}

fn wal_block() -> Result<BlockSize, EngineError> {
    BlockSize::new(WAL_BLOCK).ok_or(EngineError::TooLarge)
}

/// The name of segment `sequence` of the log of database file `base`.
pub(crate) fn segment_name(base: &str, sequence: u64) -> String {
    format!("{base}-wal-{sequence:016x}")
}

/// The sequence number of a file named as a segment of `base`'s log, or `None` for any other.
pub(crate) fn segment_sequence(base: &str, name: &str) -> Option<u64> {
    let digits = name.strip_prefix(base)?.strip_prefix("-wal-")?;
    if digits.len() != 16 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(digits, 16).ok()
}

/// The sequence numbers of every segment file of `base`'s log, ascending.
pub(crate) fn segment_files<F: FileSystem>(fs: &F, base: &str) -> io::Result<Vec<u64>> {
    let mut sequences: Vec<u64> = fs
        .names()?
        .iter()
        .filter_map(|name| segment_sequence(base, name))
        .collect();
    sequences.sort_unstable();
    Ok(sequences)
}

#[derive(Debug)]
struct Segment {
    sequence: u64,
    first_lsn: u64,
    blocks: u64,
}

/// The write-ahead log of one database: segments of whole sector blocks, each record starting a
/// block of its own, so an append never rewrites a synced sector.
/// Log segments no durable header needs, which the log no longer lists.
#[derive(Debug)]
pub(crate) struct Detached<F: FileSystem> {
    fs: Arc<F>,
    names: Vec<String>,
}

impl<F: FileSystem> Detached<F> {
    pub(crate) fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Removes the segments' files.
    pub(crate) fn remove(self) -> Result<(), EngineError> {
        for name in &self.names {
            self.fs.remove(name)?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct Wal<F: FileSystem> {
    fs: Arc<F>,
    base: String,
    identity: LogIdentity,
    segment_target: u64,
    segments_oldest_first: Vec<Segment>,
    active: F::File,
    active_header: SegmentHeader,
    next_lsn: u64,
}

impl<F: FileSystem> Wal<F> {
    /// Starts a new log whose first segment is `sequence` and whose first record will be
    /// `next_lsn`. The segment is durable, name included, when this returns.
    pub(crate) fn create(
        fs: Arc<F>,
        base: &str,
        identity: LogIdentity,
        sequence: u64,
        next_lsn: u64,
        hlc: u64,
        segment_target: u64,
    ) -> Result<Self, EngineError> {
        let (active, active_header) = new_segment(&*fs, base, identity, sequence, next_lsn, hlc)?;
        Ok(Self {
            fs,
            base: base.to_owned(),
            identity,
            segment_target,
            segments_oldest_first: vec![Segment {
                sequence,
                first_lsn: next_lsn,
                blocks: 1,
            }],
            active,
            active_header,
            next_lsn,
        })
    }

    /// Opens the log `header` names after a crash and calls `replay` with every record after
    /// the checkpoint, in LSN order. A torn tail of the last segment is cut off, an unfinished
    /// last segment is removed, and any other damage fails as corruption.
    pub(crate) fn recover(
        fs: Arc<F>,
        base: &str,
        header: &Header,
        segment_target: u64,
        mut replay: impl FnMut(Record) -> Result<(), EngineError>,
    ) -> Result<Self, EngineError> {
        let identity = LogIdentity {
            database_id: header.database_id,
            shard_id: header.shard_id,
            salt: header.wal.salt,
        };
        let mut sequences = segment_files(&*fs, base)?;
        for stale in sequences
            .iter()
            .filter(|seq| **seq < header.wal.first_segment)
        {
            fs.remove(&segment_name(base, *stale))?;
        }
        sequences.retain(|seq| *seq >= header.wal.first_segment);
        if sequences.first() != Some(&header.wal.first_segment) {
            return Err(EngineError::WalMissing);
        }
        for (expected, found) in (header.wal.first_segment..).zip(&sequences) {
            if expected != *found {
                return Err(EngineError::corrupt(format!(
                    "log segment {expected} is missing before segment {found}"
                )));
            }
        }
        let after = header.checkpoint_lsn + 1;
        let mut expected_lsn = None;
        let mut segments = Vec::new();
        let mut active = None;
        let count = sequences.len();
        for (index, sequence) in sequences.into_iter().enumerate() {
            let last = index + 1 == count;
            let name = segment_name(base, sequence);
            let file = fs.open(&name, wal_block()?)?;
            let blocks = file.block_count()?;
            let mut bytes =
                vec![0; usize::try_from(blocks).map_err(|_| EngineError::TooLarge)? * WAL_BLOCK];
            file.read(0, &mut bytes)?;
            let segment_header = match bytes.get(..WAL_BLOCK).map(SegmentHeader::decode) {
                Some(Ok(decoded)) => decoded,
                Some(Err(error @ EngineError::Unsupported(_))) => return Err(error),
                _ if last && index > 0 => {
                    let seed = SegmentHeader {
                        sequence,
                        salt: identity.salt,
                        ..SegmentHeader::default()
                    }
                    .record_seed();
                    if sealed_record_after(seed, &bytes, 0).is_some() {
                        return Err(EngineError::corrupt(format!(
                            "the header of log segment {sequence} is damaged, but synced records follow it"
                        )));
                    }
                    drop(file);
                    fs.remove(&name)?;
                    fs.sync_names()?;
                    break;
                }
                Some(Err(error)) => return Err(error),
                None => {
                    return Err(EngineError::corrupt(format!(
                        "log segment {sequence} is empty"
                    )));
                }
            };
            check_segment(&segment_header, identity, sequence)?;
            match expected_lsn {
                None if segment_header.first_lsn > after => {
                    return Err(EngineError::corrupt(format!(
                        "log segment {sequence} starts at LSN {} after the checkpoint at {}",
                        segment_header.first_lsn, header.checkpoint_lsn
                    )));
                }
                Some(expected) if segment_header.first_lsn != expected => {
                    return Err(EngineError::corrupt(format!(
                        "log segment {sequence} starts at LSN {} where {expected} was next",
                        segment_header.first_lsn
                    )));
                }
                _ => {}
            }
            let seed = segment_header.record_seed();
            let mut lsn = segment_header.first_lsn;
            let mut at = WAL_BLOCK;
            loop {
                let rest = bytes.get(at..).unwrap_or_default();
                match decode_record(seed, rest) {
                    Ok((record, length)) if record.lsn == lsn => {
                        if lsn >= after {
                            replay(record)?;
                        }
                        lsn += 1;
                        at += padded(length);
                    }
                    Ok((record, _)) => {
                        return Err(EngineError::corrupt(format!(
                            "log segment {sequence} holds LSN {} where {lsn} was next",
                            record.lsn
                        )));
                    }
                    Err(NotRecord::Refused(error)) => return Err(error),
                    Err(NotRecord::End | NotRecord::Invalid(_)) if last => {
                        if let Some(later) = sealed_record_after(seed, &bytes, at) {
                            return Err(EngineError::corrupt(format!(
                                "log segment {sequence} is damaged at LSN {lsn}, before a synced \
                                 record at byte {later}"
                            )));
                        }
                        break;
                    }
                    Err(NotRecord::End) => break,
                    Err(NotRecord::Invalid(reason)) => {
                        return Err(EngineError::corrupt(format!(
                            "log segment {sequence} is damaged before its end: {reason}"
                        )));
                    }
                }
            }
            let used = (at / WAL_BLOCK) as u64;
            if last && used < blocks {
                file.set_block_count(used)?;
                file.sync()?;
            }
            expected_lsn = Some(lsn);
            segments.push(Segment {
                sequence,
                first_lsn: segment_header.first_lsn,
                blocks: used,
            });
            active = Some((file, segment_header));
        }
        let next_lsn = expected_lsn.unwrap_or(after);
        if next_lsn < after {
            return Err(EngineError::corrupt(format!(
                "the log ends at LSN {} before the checkpoint at {}",
                next_lsn - 1,
                header.checkpoint_lsn
            )));
        }
        let Some((active, active_header)) = active else {
            return Err(EngineError::WalMissing);
        };
        Ok(Self {
            fs,
            base: base.to_owned(),
            identity,
            segment_target,
            segments_oldest_first: segments,
            active,
            active_header,
            next_lsn,
        })
    }

    #[cfg(test)]
    /// The LSN the next record must carry.
    pub(crate) fn next_lsn(&self) -> u64 {
        self.next_lsn
    }

    /// The bytes of every segment on disk.
    pub(crate) fn size(&self) -> u64 {
        self.segments_oldest_first
            .iter()
            .map(|segment| segment.blocks * WAL_BLOCK as u64)
            .sum()
    }

    /// Appends `record`, whose prefix and body are in place, seals it for the segment it lands
    /// in, and returns once it is durable. A record that would push a segment holding records
    /// past the target size starts a new segment.
    pub(crate) fn append(&mut self, mut record: Vec<u8>, hlc: u64) -> Result<(), EngineError> {
        let lsn = record
            .get(12..20)
            .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
            .map(u64::from_le_bytes);
        if lsn != Some(self.next_lsn) {
            return Err(EngineError::corrupt(format!(
                "a record for LSN {lsn:?} was appended where {} is next",
                self.next_lsn
            )));
        }
        let length = padded(record.len()) as u64;
        let active = self.active_segment()?;
        let used = active.blocks * WAL_BLOCK as u64;
        if active.blocks > 1 && used + length > self.segment_target {
            self.roll_over(hlc)?;
        }
        seal_record(self.active_header.record_seed(), &mut record)?;
        record.resize(padded(record.len()), 0);
        let at = self.active_segment()?.blocks;
        self.active.write(at, &record)?;
        self.active.sync()?;
        let blocks = (record.len() / WAL_BLOCK) as u64;
        self.active_segment_mut()?.blocks += blocks;
        self.next_lsn += 1;
        Ok(())
    }

    fn active_segment(&self) -> Result<&Segment, EngineError> {
        self.segments_oldest_first
            .last()
            .ok_or_else(|| EngineError::corrupt("the log has no segment"))
    }

    fn active_segment_mut(&mut self) -> Result<&mut Segment, EngineError> {
        self.segments_oldest_first
            .last_mut()
            .ok_or_else(|| EngineError::corrupt("the log has no segment"))
    }

    fn roll_over(&mut self, hlc: u64) -> Result<(), EngineError> {
        let sequence = self.active_segment()?.sequence + 1;
        let (file, header) = new_segment(
            &*self.fs,
            &self.base,
            self.identity,
            sequence,
            self.next_lsn,
            hlc,
        )?;
        self.active = file;
        self.active_header = header;
        self.segments_oldest_first.push(Segment {
            sequence,
            first_lsn: self.next_lsn,
            blocks: 1,
        });
        Ok(())
    }

    /// The first segment a recovery from a checkpoint at `checkpoint_lsn` needs: the one that
    /// holds, or will hold, the next LSN.
    pub(crate) fn first_needed(&self, checkpoint_lsn: u64) -> u64 {
        self.segments_oldest_first
            .iter()
            .rev()
            .find(|segment| segment.first_lsn <= checkpoint_lsn + 1)
            .or(self.segments_oldest_first.first())
            .map_or(0, |segment| segment.sequence)
    }

    /// Removes the segments before `sequence`, once a durable header no longer needs them.
    pub(crate) fn drop_before(&mut self, sequence: u64) -> Result<(), EngineError> {
        self.detach_before(sequence).remove()
    }

    /// Forgets the segments before `sequence`, once a durable header no longer needs them, and
    /// returns them for the caller to remove where removing them delays no commit.
    pub(crate) fn detach_before(&mut self, sequence: u64) -> Detached<F> {
        let mut names = Vec::new();
        while self.segments_oldest_first.len() > 1
            && self.segments_oldest_first[0].sequence < sequence
        {
            let segment = self.segments_oldest_first.remove(0);
            names.push(segment_name(&self.base, segment.sequence));
        }
        Detached {
            fs: Arc::clone(&self.fs),
            names,
        }
    }

    /// Removes every segment, after a clean close made the log unneeded.
    pub(crate) fn remove_all(self) -> Result<(), EngineError> {
        let Self {
            fs,
            base,
            segments_oldest_first: segments,
            active,
            ..
        } = self;
        drop(active);
        for segment in segments {
            fs.remove(&segment_name(&base, segment.sequence))?;
        }
        fs.sync_names()?;
        Ok(())
    }
}

fn sealed_record_after(seed: u64, bytes: &[u8], at: usize) -> Option<usize> {
    (at + WAL_BLOCK..bytes.len())
        .step_by(WAL_BLOCK)
        .find(|start| {
            !matches!(
                decode_record(seed, &bytes[*start..]),
                Err(NotRecord::End | NotRecord::Invalid(_))
            )
        })
}

fn check_segment(
    header: &SegmentHeader,
    identity: LogIdentity,
    sequence: u64,
) -> Result<(), EngineError> {
    if header.database_id != identity.database_id
        || header.shard_id != identity.shard_id
        || header.salt != identity.salt
    {
        return Err(EngineError::corrupt(format!(
            "log segment {sequence} belongs to another database or log"
        )));
    }
    if header.sequence != sequence {
        return Err(EngineError::corrupt(format!(
            "the file of log segment {sequence} holds segment {}",
            header.sequence
        )));
    }
    Ok(())
}

fn new_segment<F: FileSystem>(
    fs: &F,
    base: &str,
    identity: LogIdentity,
    sequence: u64,
    first_lsn: u64,
    hlc: u64,
) -> Result<(F::File, SegmentHeader), EngineError> {
    let header = SegmentHeader {
        minor: FORMAT_MINOR,
        database_id: identity.database_id,
        shard_id: identity.shard_id,
        sequence,
        first_lsn,
        first_term: 0,
        created_hlc: hlc,
        salt: identity.salt,
    };
    let name = segment_name(base, sequence);
    let file = fs.create(&name, wal_block()?)?;
    file.write(0, &header.encode())?;
    file.sync()?;
    fs.sync_names()?;
    Ok((file, header))
}

#[cfg(test)]
#[path = "wal.test.rs"]
mod tests;
