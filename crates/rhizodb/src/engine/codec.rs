use std::hash::Hasher;

use twox_hash::XxHash3_64;

use super::error::EngineError;

/// The xxh3 64-bit hash of `parts` joined, seeded with `seed`. Every engine checksum is one.
pub(crate) fn checksum(seed: u64, parts: &[&[u8]]) -> u64 {
    let mut hasher = XxHash3_64::with_seed(seed);
    for part in parts {
        hasher.write(part);
    }
    hasher.finish()
}

/// A bounds-checked little-endian cursor over stored bytes, which fails as corruption instead of
/// panicking when the bytes run out.
#[derive(Clone, Debug)]
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    what: &'static str,
}

impl<'a> Reader<'a> {
    /// A cursor over `bytes`, which names `what` it decodes in its errors.
    pub(crate) fn new(bytes: &'a [u8], what: &'static str) -> Self {
        Self { bytes, what }
    }

    pub(crate) fn remaining(&self) -> usize {
        self.bytes.len()
    }

    pub(crate) fn rest(&self) -> &'a [u8] {
        self.bytes
    }

    pub(crate) fn take(&mut self, length: usize) -> Result<&'a [u8], EngineError> {
        if length > self.bytes.len() {
            return Err(EngineError::corrupt(format!("{} is truncated", self.what)));
        }
        let (taken, rest) = self.bytes.split_at(length);
        self.bytes = rest;
        Ok(taken)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], EngineError> {
        let mut array = [0; N];
        array.copy_from_slice(self.take(N)?);
        Ok(array)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, EngineError> {
        Ok(self.array::<1>()?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, EngineError> {
        self.array().map(u16::from_le_bytes)
    }

    pub(crate) fn u32(&mut self) -> Result<u32, EngineError> {
        self.array().map(u32::from_le_bytes)
    }

    pub(crate) fn u64(&mut self) -> Result<u64, EngineError> {
        self.array().map(u64::from_le_bytes)
    }

    pub(crate) fn u128(&mut self) -> Result<u128, EngineError> {
        self.array().map(u128::from_le_bytes)
    }

    /// A length-prefixed byte string whose length is a `u32`.
    pub(crate) fn bytes(&mut self) -> Result<&'a [u8], EngineError> {
        let length = self.u32()?;
        let length = usize::try_from(length).map_err(|_| self.bad("a length"))?;
        self.take(length)
    }

    /// Fails unless every byte was read.
    pub(crate) fn finish(&self) -> Result<(), EngineError> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(EngineError::corrupt(format!(
                "{} has {} trailing bytes",
                self.what,
                self.bytes.len()
            )))
        }
    }

    pub(crate) fn bad(&self, field: &str) -> EngineError {
        EngineError::corrupt(format!("{} holds an invalid {field}", self.what))
    }
}

pub(crate) fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub(crate) fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub(crate) fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub(crate) fn put_u128(out: &mut Vec<u8>, value: u128) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Appends `bytes` after its `u32` length, and fails when it is longer than a `u32` counts.
pub(crate) fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), EngineError> {
    let length = u32::try_from(bytes.len()).map_err(|_| EngineError::TooLarge)?;
    put_u32(out, length);
    out.extend_from_slice(bytes);
    Ok(())
}
