const SCALE_HIGH: i128 = 16 * 1_000_000_000_000;
const SCALE_LOW: i128 = 1_000_000_000_000;
const SCALE: i128 = SCALE_HIGH * SCALE_LOW;
const SCALE_DIGITS: u32 = 24;

/// An exact quantity of one unit: a whole part and a fraction in units of `1 / SCALE`, both with
/// the sign of the quantity. `SCALE` is a multiple of 1600 and of `10^24`, so decimal fractions and
/// the 48699/1600 days of a month carry without rounding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Amount {
    whole: i128,
    fraction: i128,
}

impl Amount {
    /// A whole number of units.
    pub(crate) fn whole(value: i128) -> Amount {
        Amount {
            whole: value,
            fraction: 0,
        }
    }

    /// `count` times `factor`, where `factor` counts as the shortest decimal that reads back as
    /// it, so `0.3` is three tenths. `None` when `factor` is not finite or the product is out of
    /// range.
    pub(crate) fn product(count: i128, factor: f64) -> Option<Amount> {
        if !factor.is_finite() {
            return None;
        }
        let text = format!("{factor:e}");
        let (mantissa, exponent) = text.split_once('e')?;
        let exponent: i32 = exponent.parse().ok()?;
        let (whole_digits, fraction_digits) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        let digits: i128 = format!("{whole_digits}{fraction_digits}").parse().ok()?;
        let exponent = exponent - i32::try_from(fraction_digits.len()).ok()?;
        let product = count.checked_mul(digits)?;
        if exponent >= 0 {
            let power = 10i128.checked_pow(exponent.unsigned_abs())?;
            return product.checked_mul(power).map(Amount::whole);
        }
        let places = exponent.unsigned_abs();
        if places > 38 {
            return Some(Amount::default());
        }
        let power = 10i128.pow(places);
        let whole = product / power;
        let remainder = product % power;
        let fraction = if places <= SCALE_DIGITS {
            remainder * (SCALE / power)
        } else {
            remainder / 10i128.pow(places - SCALE_DIGITS) * 16
        };
        Some(Amount { whole, fraction })
    }

    /// `count` divided by `divisor`, which is not zero.
    pub(crate) fn quotient(count: i128, divisor: i128) -> Amount {
        let whole = count / divisor;
        let remainder = count % divisor;
        let high = remainder * SCALE_HIGH;
        let fraction = high / divisor * SCALE_LOW + high % divisor * SCALE_LOW / divisor;
        Amount { whole, fraction }
    }

    /// The whole part, truncated toward zero.
    pub(crate) fn truncated(self) -> i128 {
        self.whole
    }

    /// The sum of two amounts of the same unit, or `None` when it overflows.
    pub(crate) fn plus(self, other: Amount) -> Option<Amount> {
        let mut whole = self.whole.checked_add(other.whole)?;
        let mut fraction = self.fraction + other.fraction;
        let carry = fraction / SCALE;
        whole = whole.checked_add(carry)?;
        fraction -= carry * SCALE;
        if whole > 0 && fraction < 0 {
            whole -= 1;
            fraction += SCALE;
        } else if whole < 0 && fraction > 0 {
            whole += 1;
            fraction -= SCALE;
        }
        Some(Amount { whole, fraction })
    }

    fn fraction_in(self, numerator: i128, denominator: i128) -> Amount {
        let scaled = self.fraction * numerator / denominator;
        Amount {
            whole: scaled / SCALE,
            fraction: scaled % SCALE,
        }
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
    let days = days.plus(months.fraction_in(48_699, 1600))?;
    let seconds = seconds.plus(days.fraction_in(86_400, 1))?;
    let nanoseconds = nanoseconds.plus(seconds.fraction_in(1_000_000_000, 1))?;
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
