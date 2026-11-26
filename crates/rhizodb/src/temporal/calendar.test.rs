use super::*;
use proptest::prelude::*;

#[test]
fn the_epoch_is_day_zero_and_a_thursday() {
    assert_eq!(days_from_civil(1970, 1, 1), 0);
    assert_eq!(civil_from_days(0), (1970, 1, 1));
    assert_eq!(day_of_week(0), 4);
}

#[test]
fn known_dates_have_their_day_numbers() {
    assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    assert_eq!(days_from_civil(1969, 12, 31), -1);
    assert_eq!(civil_from_days(-719_528), (0, 1, 1));
}

#[test]
fn iso_weeks_follow_the_fourth_of_january() {
    assert_eq!(iso_week(days_from_civil(1984, 1, 1)), (1983, 52));
    assert_eq!(iso_week(days_from_civil(1984, 10, 11)), (1984, 41));
    assert_eq!(iso_week(days_from_civil(1816, 12, 31)), (1817, 1));
    assert_eq!(week_year_start(1817), days_from_civil(1816, 12, 30));
    assert_eq!(weeks_in_year(1818), 53);
    assert_eq!(weeks_in_year(1819), 52);
}

#[test]
fn leap_years_follow_the_gregorian_rule() {
    assert!(is_leap_year(2000));
    assert!(!is_leap_year(1900));
    assert!(is_leap_year(1984));
    assert_eq!(days_in_month(1984, 2), 29);
    assert_eq!(days_in_quarter(1984, 1), 91);
}

proptest! {
    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn day_numbers_round_trip(days in MIN_DAYS..=MAX_DAYS) {
        let (year, month, day) = civil_from_days(days);
        prop_assert!((1..=12).contains(&month));
        prop_assert!(day >= 1 && day <= days_in_month(year, month));
        prop_assert_eq!(days_from_civil(year, month, day), days);
    }
}
