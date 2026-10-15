//! Bytes per wall-clock minute over the last hour, for the Home traffic
//! chart. The sing-box API keeps no history of its own: `SubscribeStatus`
//! and `SubscribeConnections` report what moved since their last message,
//! so BoxPilot adds those up here as they arrive. Session-scoped like the
//! streams it is fed from: cleared when a run starts or stops, never
//! persisted. Pure — the minute is always passed in.

use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

/// How many minutes the series keeps, and the chart shows.
pub const WINDOW_MINUTES: usize = 60;

/// The minute `time` falls in, counted from the Unix epoch. A clock before
/// the epoch counts as minute 0.
pub fn unix_minute(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|elapsed| (elapsed.as_secs() / 60) as i64)
        .unwrap_or(0)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MinuteSeries {
    /// (minute, bytes), oldest first; minutes strictly increasing and within
    /// [`WINDOW_MINUTES`] of the newest. Minutes nothing moved in are absent.
    buckets: VecDeque<(i64, u64)>,
}

impl MinuteSeries {
    /// Count `bytes` in `minute`. A minute older than the newest one seen
    /// (the clock stepped back) counts in the newest, so the series never
    /// reorders; minutes that fall out of the window are dropped.
    pub fn add(&mut self, minute: i64, bytes: u64) {
        if bytes == 0 {
            return;
        }
        match self.buckets.back_mut() {
            Some((newest, total)) if minute <= *newest => *total = total.saturating_add(bytes),
            _ => self.buckets.push_back((minute, bytes)),
        }
        let Some(&(newest, _)) = self.buckets.back() else {
            return;
        };
        let oldest = newest - (WINDOW_MINUTES as i64 - 1);
        while self.buckets.front().is_some_and(|&(m, _)| m < oldest) {
            self.buckets.pop_front();
        }
    }

    /// The hour up to `now`: [`WINDOW_MINUTES`] values, oldest first, the
    /// last one `now`'s minute. Minutes nothing moved in are 0.
    pub fn window(&self, now: i64) -> Vec<u64> {
        let start = now - (WINDOW_MINUTES as i64 - 1);
        let mut values = vec![0; WINDOW_MINUTES];
        for &(minute, bytes) in &self.buckets {
            if (start..=now).contains(&minute) {
                values[(minute - start) as usize] = bytes;
            }
        }
        values
    }

    pub fn clear(&mut self) {
        self.buckets.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn minutes_count_from_the_epoch() {
        assert_eq!(unix_minute(UNIX_EPOCH), 0);
        assert_eq!(unix_minute(UNIX_EPOCH + Duration::from_secs(59)), 0);
        assert_eq!(unix_minute(UNIX_EPOCH + Duration::from_secs(120)), 2);
        assert_eq!(unix_minute(UNIX_EPOCH - Duration::from_secs(5)), 0);
    }

    #[test]
    fn bytes_add_up_within_a_minute() {
        let mut series = MinuteSeries::default();
        series.add(100, 10);
        series.add(100, 5);
        series.add(101, 7);
        series.add(101, 0);
        let window = series.window(101);
        assert_eq!(window.len(), WINDOW_MINUTES);
        assert_eq!(&window[WINDOW_MINUTES - 2..], &[15, 7]);
        assert!(window[..WINDOW_MINUTES - 2].iter().all(|&b| b == 0));
    }

    #[test]
    fn the_window_ends_at_now_with_quiet_minutes_zero() {
        let mut series = MinuteSeries::default();
        series.add(100, 10);
        // Three quiet minutes later: the bucket has moved left.
        let window = series.window(103);
        assert_eq!(&window[WINDOW_MINUTES - 4..], &[10, 0, 0, 0]);
        // An hour on, it is gone from view.
        assert!(series
            .window(100 + WINDOW_MINUTES as i64)
            .iter()
            .all(|&b| b == 0));
    }

    #[test]
    fn minutes_older_than_an_hour_are_dropped() {
        let mut series = MinuteSeries::default();
        series.add(0, 1);
        series.add(1, 2);
        series.add(WINDOW_MINUTES as i64, 3);
        assert_eq!(series.buckets.len(), 2);
        assert_eq!(series.buckets.front(), Some(&(1, 2)));
    }

    #[test]
    fn a_clock_stepping_back_counts_in_the_newest_minute() {
        let mut series = MinuteSeries::default();
        series.add(100, 10);
        series.add(98, 5);
        assert_eq!(series.window(100)[WINDOW_MINUTES - 1], 15);
    }

    #[test]
    fn clear_empties_it() {
        let mut series = MinuteSeries::default();
        series.add(5, 5);
        series.clear();
        assert!(series.window(5).iter().all(|&b| b == 0));
    }
}
