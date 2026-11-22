const MAX_DENOMINATOR: i128 = 1_000_000_000_000_000_000_000_000;
const MAX_DENOMINATOR_DIGITS: u32 = 24;
const LARGEST_DIVISOR: u128 = 1_000_000_000_000_000_000_000_000_000_000_000_000;
const MAX_SIGNIFICANT_DIGITS: usize = 30;

/// A decimal number: `digits` times ten to the power `exponent`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Decimal {
    digits: i128,
    exponent: i32,
}

impl Decimal {
    /// The shortest decimal that reads back as `value`, so `0.3` is three tenths. `None` when
    /// `value` is not finite.
    pub(crate) fn of_float(value: f64) -> Option<Decimal> {
        if !value.is_finite() {
            return None;
        }
        Decimal::parse(&format!("{value:e}"))
    }

    /// The decimal that `text` spells: an optional sign, digits with an optional fraction after
    /// `.` or `,`, and an optional exponent after `e` or `E`. Only the first 30 significant digits
    /// count, the rest truncated toward zero, so no amount fails for having too many digits.
    /// `None` when `text` is not such a number.
    pub(crate) fn parse(text: &str) -> Option<Decimal> {
        let (mantissa, exponent) = match text.split_once(['e', 'E']) {
            Some((mantissa, exponent)) => (mantissa, exponent.parse::<i32>().ok()?),
            None => (text, 0),
        };
        let (negative, unsigned) = match mantissa.as_bytes().first()? {
            b'-' => (true, &mantissa[1..]),
            b'+' => (false, &mantissa[1..]),
            _ => (false, mantissa),
        };
        let (whole, fraction) = unsigned.split_once(['.', ',']).unwrap_or((unsigned, ""));
        let all_digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
        if whole.len() + fraction.len() == 0 || !all_digits(whole) || !all_digits(fraction) {
            return None;
        }
        let fraction = fraction.trim_end_matches('0');
        let significant = format!("{whole}{fraction}");
        let significant = significant.trim_start_matches('0');
        let dropped = significant.len().saturating_sub(MAX_SIGNIFICANT_DIGITS);
        let kept = &significant[..significant.len() - dropped];
        let magnitude: i128 = if kept.is_empty() {
            0
        } else {
            kept.parse().ok()?
        };
        Some(Decimal {
            digits: if negative { -magnitude } else { magnitude },
            exponent: exponent
                .checked_sub(i32::try_from(fraction.len()).ok()?)?
                .checked_add(i32::try_from(dropped).ok()?)?,
        })
    }
}

/// An exact quantity of one unit: a whole part and a fraction `numerator / denominator`, both
/// with the sign of the quantity, the fraction below one and in lowest terms. Fractions with
/// denominators up to `10^24` stay exact, so decimal amounts, thirds, and the 48699/1600 days of
/// a month carry without rounding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Amount {
    whole: i128,
    numerator: i128,
    denominator: i128,
}

fn gcd(a: i128, b: i128) -> i128 {
    let (mut a, mut b) = (a.unsigned_abs(), b.unsigned_abs());
    while b != 0 {
        (a, b) = (b, a % b);
    }
    i128::try_from(a).unwrap_or(1).max(1)
}

impl Amount {
    /// Nothing of the unit.
    pub(crate) const ZERO: Amount = Amount {
        whole: 0,
        numerator: 0,
        denominator: 1,
    };

    /// A whole number of units.
    pub(crate) fn whole(value: i128) -> Amount {
        Amount {
            whole: value,
            ..Amount::ZERO
        }
    }

    fn ratio(numerator: i128, denominator: i128) -> Option<Amount> {
        let (numerator, denominator) = if denominator < 0 {
            (numerator.checked_neg()?, denominator.checked_neg()?)
        } else {
            (numerator, denominator)
        };
        if denominator == 0 {
            return None;
        }
        let rest = numerator % denominator;
        let common = gcd(rest, denominator);
        Some(Amount {
            whole: numerator / denominator,
            numerator: rest / common,
            denominator: denominator / common,
        })
    }

    /// `count` times `factor`, where `factor` counts as the shortest decimal that reads back as
    /// it, so `0.3` is three tenths. `None` when `factor` is not finite or the product is out of
    /// range.
    pub(crate) fn product(count: i128, factor: f64) -> Option<Amount> {
        Amount::times(count, Decimal::of_float(factor)?)
    }

    /// `count` times a decimal, or `None` when the product is out of range. Digits past the
    /// 24th decimal place are dropped.
    pub(crate) fn times(count: i128, factor: Decimal) -> Option<Amount> {
        let product = count.checked_mul(factor.digits)?;
        let places = factor.exponent.unsigned_abs();
        if factor.exponent >= 0 {
            return product
                .checked_mul(10i128.checked_pow(places)?)
                .map(Amount::whole);
        }
        if places <= MAX_DENOMINATOR_DIGITS {
            return Amount::ratio(product, 10i128.pow(places));
        }
        let dropped = places - MAX_DENOMINATOR_DIGITS;
        let kept = 10i128
            .checked_pow(dropped)
            .map_or(0, |power| product / power);
        Amount::ratio(kept, MAX_DENOMINATOR)
    }

    /// `count` divided by `divisor`, where `divisor` counts as the shortest decimal that reads
    /// back as it. An infinite `divisor` gives zero. `None` when `divisor` is zero or NaN, or the
    /// quotient is out of range.
    pub(crate) fn divided(count: i128, divisor: f64) -> Option<Amount> {
        if divisor.is_infinite() {
            return Some(Amount::ZERO);
        }
        let divisor = Decimal::of_float(divisor)?;
        if divisor.digits == 0 {
            return None;
        }
        let places = divisor.exponent.unsigned_abs();
        if divisor.exponent >= 0 {
            return Some(
                10i128
                    .checked_pow(places)
                    .and_then(|power| divisor.digits.checked_mul(power))
                    .map_or(Amount::ZERO, |divisor| Amount::quotient(count, divisor)),
            );
        }
        if count == 0 {
            return Some(Amount::ZERO);
        }
        Amount::quotient(count, divisor.digits).times_power_of_ten(places)
    }

    fn times_power_of_ten(self, places: u32) -> Option<Amount> {
        let mut whole = self.whole;
        let mut numerator = self.numerator;
        for _ in 0..places {
            if numerator == 0 {
                whole = whole.checked_mul(10)?;
                continue;
            }
            let scaled = numerator * 10;
            whole = whole
                .checked_mul(10)?
                .checked_add(scaled / self.denominator)?;
            numerator = scaled % self.denominator;
        }
        Amount::whole(whole).plus(Amount::ratio(numerator, self.denominator)?)
    }

    /// `count` divided by `divisor`, which is not zero. A divisor above `10^24` keeps the
    /// quotient to 24 decimal places, and one above `10^36` gives zero.
    pub(crate) fn quotient(count: i128, divisor: i128) -> Amount {
        if divisor.unsigned_abs() <= MAX_DENOMINATOR.unsigned_abs()
            && let Some(amount) = Amount::ratio(count, divisor)
        {
            return amount;
        }
        let magnitude = divisor.unsigned_abs();
        if magnitude > LARGEST_DIVISOR {
            return Amount::ZERO;
        }
        let counted = count.unsigned_abs();
        let whole = counted / magnitude;
        let mut remainder = counted % magnitude;
        let mut fraction = 0u128;
        for _ in 0..MAX_DENOMINATOR_DIGITS {
            let scaled = remainder * 10;
            fraction = fraction * 10 + scaled / magnitude;
            remainder = scaled % magnitude;
        }
        let sign = if (count < 0) == (divisor < 0) { 1 } else { -1 };
        let fraction = sign * i128::try_from(fraction).unwrap_or(0);
        Amount::ratio(fraction, MAX_DENOMINATOR)
            .and_then(|fraction| {
                Amount::whole(sign * i128::try_from(whole).unwrap_or(i128::MAX)).plus(fraction)
            })
            .unwrap_or(Amount::ZERO)
    }

    /// The whole part, truncated toward zero.
    pub(crate) fn truncated(self) -> i128 {
        self.whole
    }

    /// The sum of two amounts of the same unit, or `None` when it overflows.
    pub(crate) fn plus(self, other: Amount) -> Option<Amount> {
        let common = self.denominator / gcd(self.denominator, other.denominator);
        let denominator = common.checked_mul(other.denominator)?;
        let numerator = self
            .numerator
            .checked_mul(denominator / self.denominator)?
            .checked_add(
                other
                    .numerator
                    .checked_mul(denominator / other.denominator)?,
            )?;
        let fraction = Amount::ratio(numerator, denominator)?;
        let mut whole = self
            .whole
            .checked_add(other.whole)?
            .checked_add(fraction.whole)?;
        let mut numerator = fraction.numerator;
        if whole > 0 && numerator < 0 {
            whole -= 1;
            numerator += fraction.denominator;
        } else if whole < 0 && numerator > 0 {
            whole += 1;
            numerator -= fraction.denominator;
        }
        Some(Amount {
            whole,
            numerator,
            denominator: fraction.denominator,
        })
    }

    fn fraction_in(self, numerator: i128, denominator: i128) -> Option<Amount> {
        Amount::ratio(
            self.numerator.checked_mul(numerator)?,
            self.denominator.checked_mul(denominator)?,
        )
    }
}

/// Carries the fractions of months, days, and seconds into the next smaller unit, with a month of
/// 30.436875 days, and truncates each unit toward zero. `None` when a unit overflows.
pub(crate) fn carry(
    months: Amount,
    days: Amount,
    seconds: Amount,
    nanoseconds: Amount,
) -> Option<(i128, i128, i128, i128)> {
    let days = days.plus(months.fraction_in(48_699, 1600)?)?;
    let seconds = seconds.plus(days.fraction_in(86_400, 1)?)?;
    let nanoseconds = nanoseconds.plus(seconds.fraction_in(1_000_000_000, 1)?)?;
    Some((
        months.truncated(),
        days.truncated(),
        seconds.truncated(),
        nanoseconds.truncated(),
    ))
}

#[cfg(test)]
#[path = "amount.test.rs"]
mod tests;
