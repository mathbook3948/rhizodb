use std::fs::File;
use std::io;
use std::sync::atomic::AtomicU64;

use memmap2::MmapOptions;
use memmap2::MmapRaw;

/// A file mapped shared and writable into memory, which every process that maps the same file
/// sees as the same 64-bit words.
///
/// The mapping is reachable only as [`AtomicU64`] words, never as plain bytes, since another
/// process may store into it at any time. The file must not shrink below the mapped length
/// while any process maps it: a load or store past the file's end raises `SIGBUS` on Unix.
#[derive(Debug)]
pub struct Mapping {
    raw: MmapRaw,
    words: usize,
}

impl Mapping {
    /// Maps the first `words` 64-bit words of `file`, which must be opened for reading and
    /// writing and hold at least `words * 8` bytes.
    pub fn new(file: &File, words: usize) -> io::Result<Self> {
        let bytes = words
            .checked_mul(8)
            .filter(|&bytes| bytes > 0)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a mapping holds at least one word",
                )
            })?;
        if file.metadata()?.len() < bytes as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the file is shorter than the mapping",
            ));
        }
        let raw = MmapOptions::new().len(bytes).map_raw(file)?;
        if raw.as_ptr().align_offset(align_of::<AtomicU64>()) != 0 {
            return Err(io::Error::other("the mapping is not aligned to its words"));
        }
        Ok(Self { raw, words })
    }

    /// How many words the mapping holds.
    pub fn words(&self) -> usize {
        self.words
    }

    /// The word at `index`, or `None` past the mapping's end.
    pub fn word(&self, index: usize) -> Option<&AtomicU64> {
        if index >= self.words {
            return None;
        }
        let word = self
            .raw
            .as_mut_ptr()
            .cast::<AtomicU64>()
            .wrapping_add(index);
        // SAFETY: `word` lies inside the mapping, since `index < words` and the mapping holds
        // `words * 8` bytes, and it is aligned, since `new` checked the base and every word is
        // 8 bytes. The mapping stays valid for as long as `self` lives, which the returned
        // reference cannot outlive. Every access to these bytes, in this process or another
        // one, goes through an atomic word, so no non-atomic access races with it, and
        // `AtomicU64` permits shared mutation through `&`.
        Some(unsafe { &*word })
    }
}
