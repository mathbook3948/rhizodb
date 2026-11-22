use super::*;

fn carried(months: f64, days: f64, seconds: f64, nanoseconds: f64) -> (i128, i128, i128, i128) {
    carry(
        Amount::product(1, months).unwrap(),
        Amount::product(1, days).unwrap(),
        Amount::product(1, seconds).unwrap(),
        Amount::product(1, nanoseconds).unwrap(),
    )
    .unwrap()
}

#[test]
fn a_fraction_of_a_month_carries_into_days_and_seconds_exactly() {
    assert_eq!(carried(0.75, 0.0, 0.0, 0.0), (0, 22, 71_509, 500_000_000));
    assert_eq!(carried(74.5, 7.0, 29_195.0, 0.5), (74, 22, 48_068, 0));
}

#[test]
fn negative_amounts_truncate_toward_zero() {
    assert_eq!(carried(-0.5, 0.0, 0.0, 0.0), (0, -15, -18_873, 0));
    assert_eq!(carried(1.0, -0.5, 0.0, 0.0), (1, 0, -43_200, 0));
}

#[test]
fn products_and_quotients_are_exact() {
    assert_eq!(
        Amount::product(3, 2.5),
        Some(Amount::whole(7).plus(Amount::quotient(1, 2)).unwrap())
    );
    assert_eq!(Amount::product(1 << 70, 1e300), None);
    assert_eq!(Amount::product(1, f64::NAN), None);
    assert_eq!(Amount::quotient(-7, 2).truncated(), -3);
    assert_eq!(Amount::product(1, 5e-324).map(Amount::truncated), Some(0));
}

#[test]
fn thirds_carry_without_rounding() {
    let third = |count| Amount::quotient(count, 3);
    assert_eq!(
        carry(third(0), third(1), third(0), third(0)),
        Some((0, 0, 28_800, 0))
    );
    assert_eq!(Amount::divided(1, 0.3).map(Amount::truncated), Some(3));
    assert_eq!(Amount::divided(1, 0.0), None);
}
