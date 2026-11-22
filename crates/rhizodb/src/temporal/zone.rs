use jiff::Timestamp;
use jiff::tz::AmbiguousOffset;
use jiff::tz::TimeZone;

use super::calendar::NANOS_PER_SECOND;
use super::calendar::SECONDS_PER_DAY;
use super::calendar::civil_from_days;

/// The largest UTC offset, in seconds, that a time or datetime can have.
pub(crate) const MAX_OFFSET_SECONDS: i32 = 18 * 3600;

/// An IANA time zone found in the bundled database, with the name it is listed under.
#[derive(Clone, Debug)]
pub(crate) struct NamedZone {
    pub(crate) name: String,
    rules: TimeZone,
}

impl NamedZone {
    /// The zone listed under `name`, matched without regard to case.
    pub(crate) fn find(name: &str) -> Option<NamedZone> {
        let rules = TimeZone::get(name).ok()?;
        let name = rules.iana_name().unwrap_or(name).to_owned();
        Some(NamedZone { name, rules })
    }

    /// The UTC offset, in seconds, in effect at an instant.
    pub(crate) fn offset_at(&self, epoch_second: i64) -> i32 {
        let clamped = epoch_second.clamp(Timestamp::MIN.as_second(), Timestamp::MAX.as_second());
        Timestamp::from_second(clamped)
            .map(|instant| self.rules.to_offset(instant).seconds())
            .unwrap_or(0)
    }

    /// The instant, in seconds from the epoch, at which the clocks of this zone read
    /// `local_second`. A local time skipped by a transition moves forward by the length of the
    /// gap. A local time that occurs twice takes `preferred` when it is one of its offsets, and
    /// the earlier offset otherwise.
    pub(crate) fn instant_of(&self, local_second: i64, preferred: Option<i32>) -> i64 {
        let Some(civil) = civil_datetime(local_second) else {
            return local_second - i64::from(self.offset_at(local_second));
        };
        let offset = match self.rules.to_ambiguous_timestamp(civil).offset() {
            AmbiguousOffset::Unambiguous { offset } => offset.seconds(),
            AmbiguousOffset::Gap { before, .. } => before.seconds(),
            AmbiguousOffset::Fold { before, after } => match preferred {
                Some(preferred) if preferred == after.seconds() => preferred,
                _ => before.seconds(),
            },
        };
        local_second - i64::from(offset)
    }
}

fn civil_datetime(local_second: i64) -> Option<jiff::civil::DateTime> {
    let days = local_second.div_euclid(SECONDS_PER_DAY);
    let second_of_day = local_second.rem_euclid(SECONDS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    jiff::civil::DateTime::new(
        i16::try_from(year).ok()?,
        i8::try_from(month).ok()?,
        i8::try_from(day).ok()?,
        i8::try_from(second_of_day / 3600).ok()?,
        i8::try_from(second_of_day / 60 % 60).ok()?,
        i8::try_from(second_of_day % 60).ok()?,
        0,
    )
    .ok()
}

/// Whether a UTC offset in seconds is one a time or datetime can have.
pub(crate) fn is_valid_offset(offset_seconds: i32) -> bool {
    (-MAX_OFFSET_SECONDS..=MAX_OFFSET_SECONDS).contains(&offset_seconds)
}

/// The current instant of the system clock, as seconds from the epoch and nanoseconds.
pub(crate) fn system_now() -> (i64, i64) {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(elapsed) => (
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX),
            i64::from(elapsed.subsec_nanos()),
        ),
        Err(before) => {
            let before = before.duration();
            let nanos = i64::from(before.subsec_nanos());
            let seconds = -i64::try_from(before.as_secs()).unwrap_or(i64::MAX);
            if nanos == 0 {
                (seconds, 0)
            } else {
                (seconds - 1, NANOS_PER_SECOND - nanos)
            }
        }
    }
}
