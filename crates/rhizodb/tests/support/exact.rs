use std::cmp::Ordering;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Int {
    negative: bool,
    limbs: Vec<u32>,
}

fn trimmed(mut limbs: Vec<u32>) -> Vec<u32> {
    while limbs.last() == Some(&0) {
        limbs.pop();
    }
    limbs
}

fn compare_magnitudes(a: &[u32], b: &[u32]) -> Ordering {
    a.len()
        .cmp(&b.len())
        .then_with(|| a.iter().rev().cmp(b.iter().rev()))
}

fn add_magnitudes(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut sum = Vec::with_capacity(a.len().max(b.len()) + 1);
    let mut carry = 0u64;
    for index in 0..a.len().max(b.len()) {
        let total =
            u64::from(*a.get(index).unwrap_or(&0)) + u64::from(*b.get(index).unwrap_or(&0)) + carry;
        sum.push(total as u32);
        carry = total >> 32;
    }
    sum.push(carry as u32);
    trimmed(sum)
}

fn subtract_magnitudes(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut difference = Vec::with_capacity(a.len());
    let mut borrow = 0i64;
    for (index, limb) in a.iter().enumerate() {
        let mut total = i64::from(*limb) - i64::from(*b.get(index).unwrap_or(&0)) - borrow;
        borrow = 0;
        if total < 0 {
            total += 1 << 32;
            borrow = 1;
        }
        difference.push(total as u32);
    }
    trimmed(difference)
}

fn multiply_magnitudes(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut product = vec![0u32; a.len() + b.len() + 1];
    for (i, x) in a.iter().enumerate() {
        let mut carry = 0u64;
        for (j, y) in b.iter().enumerate() {
            let total = u64::from(product[i + j]) + u64::from(*x) * u64::from(*y) + carry;
            product[i + j] = total as u32;
            carry = total >> 32;
        }
        product[i + b.len()] = carry as u32;
    }
    trimmed(product)
}

fn divide_magnitudes(numerator: &[u32], divisor: &[u32]) -> (Vec<u32>, Vec<u32>) {
    let mut quotient = vec![0u32; numerator.len()];
    let mut remainder: Vec<u32> = Vec::new();
    for bit in (0..numerator.len() * 32).rev() {
        let mut carry = (numerator[bit / 32] >> (bit % 32)) & 1;
        for limb in remainder.iter_mut() {
            let next = *limb >> 31;
            *limb = (*limb << 1) | carry;
            carry = next;
        }
        if carry != 0 {
            remainder.push(carry);
        }
        if compare_magnitudes(&remainder, divisor) != Ordering::Less {
            remainder = subtract_magnitudes(&remainder, divisor);
            quotient[bit / 32] |= 1 << (bit % 32);
        }
    }
    (trimmed(quotient), remainder)
}

impl Int {
    fn new(negative: bool, limbs: Vec<u32>) -> Int {
        let limbs = trimmed(limbs);
        Int {
            negative: negative && !limbs.is_empty(),
            limbs,
        }
    }

    pub fn of(value: i128) -> Int {
        let magnitude = value.unsigned_abs();
        let limbs = (0..4).map(|i| (magnitude >> (32 * i)) as u32).collect();
        Int::new(value < 0, limbs)
    }

    pub fn power_of_ten(exponent: u32) -> Int {
        (0..exponent).fold(Int::of(1), |power, _| power.times(&Int::of(10)))
    }

    pub fn to_i128(&self) -> Option<i128> {
        if self.limbs.len() > 4 {
            return None;
        }
        let magnitude = self
            .limbs
            .iter()
            .rev()
            .fold(0u128, |total, limb| (total << 32) | u128::from(*limb));
        let value = i128::try_from(magnitude).ok()?;
        Some(if self.negative { -value } else { value })
    }

    pub fn negated(&self) -> Int {
        Int::new(!self.negative, self.limbs.clone())
    }

    pub fn plus(&self, other: &Int) -> Int {
        if self.negative == other.negative {
            return Int::new(self.negative, add_magnitudes(&self.limbs, &other.limbs));
        }
        match compare_magnitudes(&self.limbs, &other.limbs) {
            Ordering::Less => Int::new(
                other.negative,
                subtract_magnitudes(&other.limbs, &self.limbs),
            ),
            _ => Int::new(
                self.negative,
                subtract_magnitudes(&self.limbs, &other.limbs),
            ),
        }
    }

    pub fn times(&self, other: &Int) -> Int {
        Int::new(
            self.negative != other.negative,
            multiply_magnitudes(&self.limbs, &other.limbs),
        )
    }

    pub fn divided_toward_zero(&self, divisor: &Int) -> (Int, Int) {
        let (quotient, remainder) = divide_magnitudes(&self.limbs, &divisor.limbs);
        let negative = self.negative != divisor.negative;
        (
            Int::new(negative, quotient),
            Int::new(self.negative, remainder),
        )
    }
}

#[derive(Clone, Debug)]
pub struct Rational {
    numerator: Int,
    denominator: Int,
}

impl Rational {
    pub fn new(numerator: Int, denominator: Int) -> Rational {
        Rational {
            numerator,
            denominator,
        }
    }

    pub fn times(&self, numerator: i128, denominator: i128) -> Rational {
        Rational::new(
            self.numerator.times(&Int::of(numerator)),
            self.denominator.times(&Int::of(denominator)),
        )
    }

    pub fn plus(&self, other: &Rational) -> Rational {
        Rational::new(
            self.numerator
                .times(&other.denominator)
                .plus(&other.numerator.times(&self.denominator)),
            self.denominator.times(&other.denominator),
        )
    }

    pub fn split(&self) -> (Int, Rational) {
        let (whole, rest) = self.numerator.divided_toward_zero(&self.denominator);
        (whole, Rational::new(rest, self.denominator.clone()))
    }
}
