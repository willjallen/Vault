use std::fmt::Write as _;

use thiserror::Error;
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, PrimitiveDateTime, UtcOffset};

/// Canonical representation for every persisted or API timestamp.
///
/// `SQLite`'s clock is millisecond-precision, so SQL writers append three zeroes
/// while Rust writers retain microsecond precision. Both paths produce the
/// same fixed-width UTC representation.
pub const CANONICAL_LENGTH: usize = 27;
pub const CANONICAL_GLOB: &str = concat!(
    "[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T",
    "[0-9][0-9]:[0-9][0-9]:[0-9][0-9].",
    "[0-9][0-9][0-9][0-9][0-9][0-9]Z"
);

const LEGACY_NAIVE_FORMATS: [&str; 4] = [
    "[year]-[month]-[day] [hour]:[minute]:[second]",
    "[year]-[month]-[day] [hour]:[minute]:[second].[subsecond]",
    "[year]-[month]-[day]T[hour]:[minute]:[second]",
    "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond]",
];

#[derive(Debug, Error)]
pub enum TimestampError {
    #[error("timestamp is empty")]
    Empty,
    #[error("timestamp is not RFC 3339 or a recognized legacy UTC timestamp")]
    Unrecognized,
    #[error("timestamp year {0} cannot be represented by the canonical format")]
    YearOutOfRange(i32),
}

/// Parses the canonical format plus every timestamp representation emitted by
/// supported historical Vault releases. Legacy timestamps without an offset
/// are UTC by definition; explicit RFC 3339 offsets are converted to UTC.
pub fn parse_utc(value: &str) -> Result<OffsetDateTime, TimestampError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(TimestampError::Empty);
    }
    if let Ok(timestamp) = OffsetDateTime::parse(value, &Rfc3339) {
        return Ok(timestamp.to_offset(UtcOffset::UTC));
    }
    for format in LEGACY_NAIVE_FORMATS {
        let Ok(description) = time::format_description::parse_borrowed::<1>(format) else {
            continue;
        };
        if let Ok(timestamp) = PrimitiveDateTime::parse(value, &description) {
            return Ok(timestamp.assume_utc());
        }
    }
    Err(TimestampError::Unrecognized)
}

/// Formats UTC with exactly six fractional digits and a literal `Z` suffix.
pub fn format_utc(timestamp: OffsetDateTime) -> Result<String, TimestampError> {
    let timestamp = timestamp.to_offset(UtcOffset::UTC);
    let year = timestamp.year();
    if !(0..=9999).contains(&year) {
        return Err(TimestampError::YearOutOfRange(year));
    }

    let mut canonical = String::with_capacity(CANONICAL_LENGTH);
    write!(
        canonical,
        "{year:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
        u8::from(timestamp.month()),
        timestamp.day(),
        timestamp.hour(),
        timestamp.minute(),
        timestamp.second(),
        timestamp.nanosecond() / 1_000,
    )
    .expect("writing a timestamp to a String cannot fail");
    Ok(canonical)
}

pub fn canonicalize(value: &str) -> Result<String, TimestampError> {
    format_utc(parse_utc(value)?)
}

#[must_use]
pub fn now_utc() -> String {
    format_utc(OffsetDateTime::now_utc())
        .expect("the current UTC year must fit the canonical timestamp format")
}

#[must_use]
pub fn is_canonical(value: &str) -> bool {
    value.len() == CANONICAL_LENGTH && canonicalize(value).is_ok_and(|canonical| canonical == value)
}
