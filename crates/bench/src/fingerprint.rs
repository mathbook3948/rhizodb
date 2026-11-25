const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const PRIME: u64 = 0x0000_0100_0000_01b3;
const FLOAT_SCALE: f64 = 1_000_000.0;
const MIDPOINT_BIAS: f64 = 1e-3;

/// The number of rows a query returned and an order-independent checksum of their values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fingerprint {
    pub rows: u64,
    pub checksum: u64,
}

impl Fingerprint {
    /// Adds one row. The order rows arrive in does not change the result.
    pub fn absorb(&mut self, row: RowHasher) {
        self.rows += 1;
        self.checksum = self.checksum.wrapping_add(row.finish());
    }

    /// The fingerprint of the rows of both.
    pub fn merge(self, other: Fingerprint) -> Fingerprint {
        Fingerprint {
            rows: self.rows + other.rows,
            checksum: self.checksum.wrapping_add(other.checksum),
        }
    }

    /// Merges the result of the query at position `op`. The position is mixed into its checksum,
    /// so the sum keeps which query gave which result: two queries that swap results differ.
    pub fn merge_at(self, op: u64, other: Fingerprint) -> Fingerprint {
        let mut seeded = RowHasher::default();
        seeded.int(op as i64);
        seeded.int(other.checksum as i64);
        Fingerprint {
            rows: self.rows + other.rows,
            checksum: self.checksum.wrapping_add(seeded.finish()),
        }
    }
}

/// Hashes the values of one row into a number both engines compute the same way.
///
/// An integer, a float, a string, a boolean, a null, and a list each hash under their own tag.
/// A float is rounded to six decimals first, with the rounding boundary moved off the exact
/// midpoint, so two engines that sum in another order agree even for a value such as `x.0000005`.
#[derive(Clone, Copy, Debug)]
pub struct RowHasher {
    state: u64,
}

impl Default for RowHasher {
    fn default() -> Self {
        RowHasher { state: OFFSET }
    }
}

impl RowHasher {
    fn byte(&mut self, byte: u8) {
        self.state = (self.state ^ u64::from(byte)).wrapping_mul(PRIME);
    }

    fn bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.byte(byte);
        }
    }

    pub fn null(&mut self) {
        self.byte(b'n');
    }

    pub fn boolean(&mut self, value: bool) {
        self.byte(b'b');
        self.byte(u8::from(value));
    }

    pub fn int(&mut self, value: i64) {
        self.byte(b'i');
        self.bytes(&value.to_le_bytes());
    }

    pub fn float(&mut self, value: f64) {
        self.byte(b'f');
        let rounded = (value * FLOAT_SCALE - MIDPOINT_BIAS).round();
        self.bytes(&(rounded as i64).to_le_bytes());
    }

    pub fn text(&mut self, value: &str) {
        self.byte(b's');
        self.bytes(&(value.len() as u64).to_le_bytes());
        self.bytes(value.as_bytes());
    }

    pub fn open_list(&mut self) {
        self.byte(b'[');
    }

    pub fn close_list(&mut self) {
        self.byte(b']');
    }

    pub(crate) fn finish(self) -> u64 {
        let mut x = self.state;
        x ^= x >> 30;
        x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
        x ^ (x >> 31)
    }
}

#[cfg(test)]
#[path = "fingerprint.test.rs"]
mod tests;
