/// A small seeded xorshift generator, so a seed always yields the same graph and queries.
#[derive(Clone, Debug)]
pub struct Random {
    state: u64,
}

impl Random {
    /// Starts a sequence from `seed`. The state is kept odd, since xorshift stays at zero forever.
    pub fn new(seed: u64) -> Random {
        Random {
            state: (seed ^ 0x9E37_79B9_7F4A_7C15) | 1,
        }
    }

    /// The next raw value.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// A value in `0..bound`, or 0 when `bound` is 0.
    pub fn below(&mut self, bound: usize) -> usize {
        match u64::try_from(bound) {
            Ok(0) | Err(_) => 0,
            Ok(bound) => usize::try_from(self.next_u64() % bound).unwrap_or(0),
        }
    }

    /// An integer in `low..=high`.
    pub fn between(&mut self, low: i64, high: i64) -> i64 {
        let span = usize::try_from(high.saturating_sub(low)).unwrap_or(0);
        let offset = i64::try_from(self.below(span.saturating_add(1))).unwrap_or(0);
        low.saturating_add(offset)
    }

    /// True with probability `numerator / denominator`.
    pub fn chance(&mut self, numerator: usize, denominator: usize) -> bool {
        self.below(denominator) < numerator
    }

    /// One element of a non-empty slice, or `None` for an empty one.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        items.get(self.below(items.len()))
    }
}

#[cfg(test)]
#[path = "random.test.rs"]
mod tests;
