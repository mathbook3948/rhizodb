use std::collections::BTreeMap;
use std::ops::Bound;

use crate::value::DecodeError;
use crate::value::Reader;
use crate::value::put_varint;

const BLOCK_SHIFT: u32 = 7;

/// The block that holds the postings of the element with ID `element` for one term.
pub(crate) fn block_of(element: u64) -> u64 {
    element >> BLOCK_SHIFT
}

/// One element's posting of a term: how often the term occurs, the element's term count, and
/// the positions of the term in ascending order.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Posting {
    pub(crate) element: u64,
    pub(crate) frequency: u32,
    pub(crate) length: u32,
    pub(crate) positions: Vec<u32>,
}

/// The postings of one term for the elements of one block, in ascending element order, as
/// stored: for each element its distance from the one before, its frequency and term count,
/// and the gaps between its positions, all as varints.
#[derive(Clone, Debug, Default)]
pub(crate) struct Block {
    bytes: Vec<u8>,
    last: Option<u64>,
}

impl Block {
    /// The block stored as `bytes` for block `block`, checked in full.
    pub(crate) fn load(block: u64, bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut last = None;
        for entry in Entries::new(block, bytes) {
            last = Some(entry?.element);
        }
        if last.is_none() {
            return Err(DecodeError::UnexpectedEnd);
        }
        Ok(Self {
            bytes: bytes.to_vec(),
            last,
        })
    }

    /// The stored bytes, or `None` when the block holds no posting.
    pub(crate) fn bytes(&self) -> Option<&[u8]> {
        self.last.map(|_| self.bytes.as_slice())
    }

    pub(crate) fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Records the posting of `element`, which lies in block `block`, replacing any it held.
    pub(crate) fn insert(
        &mut self,
        block: u64,
        element: u64,
        positions: &[u32],
        length: u32,
    ) -> Result<(), DecodeError> {
        match self.last {
            Some(last) if last >= element => {
                let mut kept = self.without(block, element)?;
                let at = kept
                    .iter()
                    .position(|(held, _)| *held > element)
                    .unwrap_or(kept.len());
                let mut entry = Vec::new();
                put_counts(&mut entry, positions, length);
                kept.insert(at, (element, entry));
                self.rebuild(block, kept);
            }
            last => {
                put_step(&mut self.bytes, block, last, element);
                put_counts(&mut self.bytes, positions, length);
                self.last = Some(element);
            }
        }
        Ok(())
    }

    /// Removes the posting of `element`, if the block holds one.
    pub(crate) fn remove(&mut self, block: u64, element: u64) -> Result<(), DecodeError> {
        let kept = self.without(block, element)?;
        self.rebuild(block, kept);
        Ok(())
    }

    fn without(&self, block: u64, element: u64) -> Result<Vec<(u64, Vec<u8>)>, DecodeError> {
        let mut kept = Vec::new();
        let mut reader = Reader::new(&self.bytes);
        let mut previous = None;
        while reader.remaining() > 0 {
            let held = read_step(&mut reader, block, previous)?;
            let start = self.bytes.len() - reader.remaining();
            skip_counts(&mut reader)?;
            if held != element {
                let end = self.bytes.len() - reader.remaining();
                kept.push((held, self.bytes[start..end].to_vec()));
            }
            previous = Some(held);
        }
        Ok(kept)
    }

    fn rebuild(&mut self, block: u64, entries: Vec<(u64, Vec<u8>)>) {
        self.bytes.clear();
        self.last = None;
        for (element, counts) in entries {
            put_step(&mut self.bytes, block, self.last, element);
            self.bytes.extend_from_slice(&counts);
            self.last = Some(element);
        }
    }
}

/// The posting blocks a write transaction changed, by the start of their keys, which names the
/// index and the term, and then by block, with the sum of their lengths.
#[derive(Debug, Default)]
pub(crate) struct Held {
    terms: BTreeMap<Vec<u8>, BTreeMap<u64, Block>>,
    bytes: usize,
}

impl Held {
    /// The held block whose key, of `term` and `block`, is `key`.
    pub(crate) fn get(&self, key: &[u8], block: u64) -> Option<&Block> {
        self.terms.get(term_of(key))?.get(&block)
    }

    /// The held block whose key, of `term` and `block`, is `key`.
    pub(crate) fn get_mut(&mut self, key: &[u8], block: u64) -> Option<&mut Block> {
        self.terms.get_mut(term_of(key))?.get_mut(&block)
    }

    /// Holds `loaded` as the block whose key is `key`.
    pub(crate) fn insert(&mut self, key: &[u8], block: u64, loaded: Block) {
        self.bytes += loaded.len();
        match self.terms.get_mut(term_of(key)) {
            Some(blocks) => {
                blocks.insert(block, loaded);
            }
            None => {
                self.terms
                    .insert(term_of(key).to_vec(), BTreeMap::from([(block, loaded)]));
            }
        }
    }

    /// Records that a held block's length went from `before` to `after`.
    pub(crate) fn resized(&mut self, before: usize, after: usize) {
        self.bytes = self.bytes - before + after;
    }

    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    /// Drops every held block whose key starts with `prefix`.
    pub(crate) fn forget(&mut self, prefix: &[u8]) {
        self.terms.retain(|term, _| !term.starts_with(prefix));
        self.bytes = self
            .terms
            .values()
            .flat_map(BTreeMap::values)
            .map(Block::len)
            .sum();
    }

    /// Every held block whose key starts with `prefix`, with its key, in key order.
    pub(crate) fn under<'a>(
        &'a self,
        prefix: &'a [u8],
    ) -> impl Iterator<Item = (Vec<u8>, &'a Block)> {
        self.terms
            .range::<[u8], _>((Bound::Included(prefix), Bound::Unbounded))
            .take_while(move |(term, _)| term.starts_with(prefix))
            .flat_map(|(term, blocks)| {
                blocks
                    .iter()
                    .map(move |(block, held)| (key_of(term, *block), held))
            })
    }

    /// Every held block with its key, in key order, leaving none held.
    pub(crate) fn take(&mut self) -> Vec<(Vec<u8>, Block)> {
        self.bytes = 0;
        std::mem::take(&mut self.terms)
            .into_iter()
            .flat_map(|(term, blocks)| {
                blocks
                    .into_iter()
                    .map(move |(block, held)| (key_of(&term, block), held))
            })
            .collect()
    }
}

fn term_of(key: &[u8]) -> &[u8] {
    &key[..key.len().saturating_sub(8)]
}

fn key_of(term: &[u8], block: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(term.len() + 8);
    key.extend_from_slice(term);
    key.extend_from_slice(&block.to_be_bytes());
    key
}

/// The postings stored as `bytes` for block `block`, each checked as it is read.
pub(crate) struct Entries<'a> {
    reader: Reader<'a>,
    block: u64,
    previous: Option<u64>,
    failed: bool,
}

impl<'a> Entries<'a> {
    pub(crate) fn new(block: u64, bytes: &'a [u8]) -> Self {
        Self {
            reader: Reader::new(bytes),
            block,
            previous: None,
            failed: false,
        }
    }

    /// The next element with its frequency and term count, skipping its positions.
    pub(crate) fn next_counts(&mut self) -> Option<Result<(u64, u32, u32), DecodeError>> {
        if self.failed || self.reader.remaining() == 0 {
            return None;
        }
        let read = (|| {
            let element = read_step(&mut self.reader, self.block, self.previous)?;
            let (frequency, length) = read_counts(&mut self.reader)?;
            for _ in 0..frequency {
                self.reader.varint()?;
            }
            self.previous = Some(element);
            Ok((element, frequency, length))
        })();
        self.failed = read.is_err();
        Some(read)
    }
}

impl Iterator for Entries<'_> {
    type Item = Result<Posting, DecodeError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.reader.remaining() == 0 {
            return None;
        }
        let read = (|| {
            let element = read_step(&mut self.reader, self.block, self.previous)?;
            let (frequency, length) = read_counts(&mut self.reader)?;
            let mut positions = Vec::new();
            let mut position = 0u32;
            for _ in 0..frequency {
                let step = u32::try_from(self.reader.varint()?)
                    .map_err(|_| DecodeError::LengthOverflow)?;
                position = position
                    .checked_add(step)
                    .ok_or(DecodeError::LengthOverflow)?;
                positions.push(position);
            }
            self.previous = Some(element);
            Ok(Posting {
                element,
                frequency,
                length,
                positions,
            })
        })();
        self.failed = read.is_err();
        Some(read)
    }
}

fn put_step(out: &mut Vec<u8>, block: u64, previous: Option<u64>, element: u64) {
    let step = match previous {
        None => element - (block << BLOCK_SHIFT),
        Some(previous) => element - previous - 1,
    };
    put_varint(out, usize::try_from(step).unwrap_or(usize::MAX));
}

fn read_step(
    reader: &mut Reader<'_>,
    block: u64,
    previous: Option<u64>,
) -> Result<u64, DecodeError> {
    let step = reader.varint_u64()?;
    let element = match previous {
        None => (block << BLOCK_SHIFT).checked_add(step),
        Some(previous) => previous.checked_add(step).and_then(|at| at.checked_add(1)),
    }
    .filter(|element| block_of(*element) == block)
    .ok_or(DecodeError::LengthOverflow)?;
    Ok(element)
}

fn put_counts(out: &mut Vec<u8>, positions: &[u32], length: u32) {
    put_varint(out, positions.len());
    put_varint(out, usize::try_from(length).unwrap_or(usize::MAX));
    let mut previous = 0u32;
    for position in positions {
        put_varint(
            out,
            usize::try_from(position.saturating_sub(previous)).unwrap_or(usize::MAX),
        );
        previous = *position;
    }
}

fn read_counts(reader: &mut Reader<'_>) -> Result<(u32, u32), DecodeError> {
    let frequency = u32::try_from(reader.varint()?).map_err(|_| DecodeError::LengthOverflow)?;
    let length = u32::try_from(reader.varint()?).map_err(|_| DecodeError::LengthOverflow)?;
    if frequency == 0 {
        return Err(DecodeError::LengthOverflow);
    }
    Ok((frequency, length))
}

fn skip_counts(reader: &mut Reader<'_>) -> Result<(), DecodeError> {
    let (frequency, _) = read_counts(reader)?;
    for _ in 0..frequency {
        reader.varint()?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "postings.test.rs"]
mod tests;
