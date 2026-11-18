//! Time conversions at the feed-rs model boundary.

use feed_rs::model::Entry;
use jiff::Timestamp;

pub(crate) fn from_chrono(dt: &chrono::DateTime<chrono::Utc>) -> Option<Timestamp> {
    Timestamp::new(dt.timestamp(), dt.timestamp_subsec_nanos() as i32).ok()
}

pub(crate) fn to_chrono(ts: Timestamp) -> chrono::DateTime<chrono::Utc> {
    // jiff's timestamp range is narrower than chrono's, so this cannot fail.
    chrono::DateTime::from_timestamp(ts.as_second(), ts.subsec_nanosecond() as u32)
        .expect("jiff timestamps fit chrono's range")
}

pub(crate) fn entry_timestamp(entry: &Entry) -> Option<Timestamp> {
    entry
        .published
        .or(entry.updated)
        .and_then(|dt| from_chrono(&dt))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrono_jiff_roundtrip_preserves_seconds_and_nanos() {
        let ts = Timestamp::new(1_700_000_000, 123_456_789).unwrap();
        assert_eq!(from_chrono(&to_chrono(ts)), Some(ts));
    }

    #[test]
    fn out_of_range_chrono_dates_are_ignored() {
        let dt = chrono::DateTime::from_timestamp(300_000_000_000, 0).unwrap();
        assert_eq!(from_chrono(&dt), None);
    }
}
