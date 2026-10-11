//! The Logs page's table columns and how wide they are. Pure, no gpui — the
//! page only draws what `layout_columns` and `drag_boundary` compute, so the
//! header and the rows always agree.
//!
//! - **Columns.** Time, Level and Source (`LogColumn`) keep the width the
//!   user gave them; Message, always last, takes what they leave, never
//!   less than `MESSAGE_MIN_WIDTH`.
//! - **Short of room** for that minimum, Time first drops to `HH:MM:SS`,
//!   then Level and Source give way toward their own minimums in
//!   proportion to what they have above them. The narrowest window leaves
//!   room for the default widths, so only columns the user widened give
//!   way.
//! - **Dragging a boundary** (`drag_boundary`) moves it with the pointer:
//!   the column on its left changes width and Message gives or takes the
//!   room — neither below its minimum.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Space between two columns, shared by the header and the rows.
pub const COLUMN_GAP: f32 = 12.;
/// The rows' horizontal padding and border, either side together.
pub const ROW_INSET: f32 = 26.;
/// `HH:MM:SS.mmm` in the table's mono font: the Time column shows the
/// milliseconds from this width up (`time_shows_millis`).
pub const TIME_FULL_WIDTH: f32 = 104.;
/// The narrowest Message gets, by a drag or for want of room.
pub const MESSAGE_MIN_WIDTH: f32 = 160.;
/// No column grows wider than this by a drag or from a settings file.
pub const MAX_WIDTH: f32 = 800.;

/// A column of the Logs table the user can resize, in the table's order.
/// Message, after them, is not one: it takes the rest of the row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LogColumn {
    /// When the line arrived: `HH:MM:SS.mmm`, or `HH:MM:SS` when narrow.
    Time,
    /// The level badge.
    Level,
    /// What wrote the line (`router`, `inbound/mixed[…]`).
    Source,
}

impl LogColumn {
    pub const ALL: [LogColumn; 3] = [LogColumn::Time, LogColumn::Level, LogColumn::Source];

    /// The id in the settings file.
    pub fn key(self) -> &'static str {
        match self {
            LogColumn::Time => "time",
            LogColumn::Level => "level",
            LogColumn::Source => "source",
        }
    }

    /// The column a settings file names; `None` for an id this release
    /// doesn't know.
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|id| id.key() == key)
    }

    fn index(self) -> usize {
        self as usize
    }

    /// The width a column starts at, in px.
    pub fn default_width(self) -> f32 {
        match self {
            LogColumn::Time => TIME_FULL_WIDTH,
            LogColumn::Level => 64.,
            LogColumn::Source => 168.,
        }
    }

    /// The narrowest a column gets, by a drag or for want of room.
    pub fn min_width(self) -> f32 {
        match self {
            // `HH:MM:SS`.
            LogColumn::Time => 70.,
            // The widest badge, `ERROR` / `DEBUG`, whole.
            LogColumn::Level => 52.,
            LogColumn::Source => 48.,
        }
    }
}

/// Whether a Time column `width` wide has room for the milliseconds.
pub fn time_shows_millis(width: f32) -> bool {
    width >= TIME_FULL_WIDTH - 0.5
}

/// The widths the user gave the columns, kept in the settings
/// (`AppSettings::logs_columns`), each within `[min_width, MAX_WIDTH]`.
///
/// Stored as `{"widths": {"source": 220}}`, holding only what the user
/// changed. Ids this release doesn't know are skipped on load.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "StoredWidths", into = "StoredWidths")]
pub struct LogColumnWidths {
    widths: BTreeMap<LogColumn, f32>,
}

impl LogColumnWidths {
    /// The column's width as the user left it (its default if untouched).
    pub fn width(&self, id: LogColumn) -> f32 {
        self.widths
            .get(&id)
            .copied()
            .unwrap_or_else(|| id.default_width())
    }

    /// Back to the default width.
    pub fn reset_width(&mut self, id: LogColumn) {
        self.widths.remove(&id);
    }

    fn set_width(&mut self, id: LogColumn, width: f32) {
        let width = clamp_width(id, width);
        if width == id.default_width() {
            self.widths.remove(&id);
        } else {
            self.widths.insert(id, width);
        }
    }
}

fn clamp_width(id: LogColumn, width: f32) -> f32 {
    // Whole pixels: the settings file stays readable.
    width.round().clamp(id.min_width(), MAX_WIDTH)
}

/// `LogColumnWidths` as written to the settings file: ids as strings, so
/// one this release doesn't know is skipped instead of failing the file.
#[derive(Serialize, Deserialize, Default)]
struct StoredWidths {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    widths: BTreeMap<String, f64>,
}

impl From<StoredWidths> for LogColumnWidths {
    fn from(stored: StoredWidths) -> Self {
        let mut widths = Self::default();
        for (key, width) in stored.widths {
            if let Some(id) = LogColumn::from_key(&key) {
                let width = width as f32;
                if width.is_finite() {
                    widths.set_width(id, width);
                }
            }
        }
        widths
    }
}

impl From<LogColumnWidths> for StoredWidths {
    fn from(widths: LogColumnWidths) -> Self {
        Self {
            widths: widths
                .widths
                .iter()
                .map(|(id, width)| (id.key().to_string(), f64::from(*width)))
                .collect(),
        }
    }
}

/// The columns' widths in a list of a given width, shared by the header and
/// the rows.
#[derive(Clone, Debug, PartialEq)]
pub struct LogLayout {
    /// Time, Level and Source, in `LogColumn::ALL`'s order.
    widths: [f32; 3],
    /// What is left for Message: at least `MESSAGE_MIN_WIDTH`.
    pub message: f32,
}

impl LogLayout {
    pub fn width(&self, id: LogColumn) -> f32 {
        self.widths[id.index()]
    }
}

/// Lay the columns out in a list `list_width` wide (the rows' padding
/// included): each at its width, Message on what is left. Short of room
/// for Message's minimum, Time drops to `HH:MM:SS` first, then Level and
/// Source give way toward their minimums in proportion to what they have
/// above them.
pub fn layout_columns(settings: &LogColumnWidths, list_width: f32) -> LogLayout {
    let room = (list_width - ROW_INSET - COLUMN_GAP * LogColumn::ALL.len() as f32).max(0.);
    let mut widths = LogColumn::ALL.map(|id| settings.width(id));
    let mut deficit = widths.iter().sum::<f32>() + MESSAGE_MIN_WIDTH - room;
    if deficit > 0. {
        // Time gives up its milliseconds all at once: a column between the
        // two forms would only show the shorter one with space to spare.
        let time = &mut widths[LogColumn::Time.index()];
        let compact = LogColumn::Time.min_width().min(*time);
        deficit -= *time - compact;
        *time = compact;
    }
    if deficit > 0. {
        let giving = [LogColumn::Level, LogColumn::Source];
        let slack: f32 = giving
            .iter()
            .map(|id| widths[id.index()] - id.min_width())
            .sum();
        if slack > 0. {
            let take = deficit.min(slack) / slack;
            for id in giving {
                let width = &mut widths[id.index()];
                *width -= (*width - id.min_width()) * take;
            }
        }
    }
    LogLayout {
        message: (room - widths.iter().sum::<f32>()).max(MESSAGE_MIN_WIDTH),
        widths,
    }
}

/// The settings after dragging `column`'s right edge (in `layout`, as it
/// was when the drag began, from `settings`) by `delta` px.
///
/// The column changes by `delta` — but not below its minimum, past
/// `MAX_WIDTH`, or by more than Message can give before its minimum — and
/// Message makes up the difference. Level and Source take the widths they
/// were laid out at, so a column that had given way for want of room
/// doesn't take it back and push the boundary off the pointer.
pub fn drag_boundary(
    settings: &LogColumnWidths,
    layout: &LogLayout,
    column: LogColumn,
    delta: f32,
) -> LogColumnWidths {
    let width = layout.width(column);
    let grow = delta
        .max(column.min_width() - width)
        .min((layout.message - MESSAGE_MIN_WIDTH).max(0.))
        .min(MAX_WIDTH - width);
    let mut settings = settings.clone();
    for id in [LogColumn::Level, LogColumn::Source] {
        if id != column && layout.width(id) < settings.width(id) {
            settings.set_width(id, layout.width(id));
        }
    }
    settings.set_width(column, width + grow);
    settings
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The list's width in the narrowest window (880px) beside the expanded
    /// sidebar, and in the 1000px window BoxPilot opens at.
    const NARROWEST: f32 = 606.;
    const OPENING: f32 = 870.;

    fn widths(layout: &LogLayout) -> [f32; 3] {
        LogColumn::ALL.map(|id| layout.width(id))
    }

    fn fills(layout: &LogLayout, list_width: f32) -> bool {
        let row = ROW_INSET + COLUMN_GAP * 3. + widths(layout).iter().sum::<f32>() + layout.message;
        (row - list_width).abs() < 0.01
    }

    #[test]
    fn defaults_fit_the_narrowest_window() {
        let settings = LogColumnWidths::default();
        for id in LogColumn::ALL {
            assert_eq!(settings.width(id), id.default_width());
            assert!(id.min_width() <= id.default_width(), "{id:?}");
            assert_eq!(LogColumn::from_key(id.key()), Some(id));
        }
        let layout = layout_columns(&settings, NARROWEST);
        assert_eq!(widths(&layout), [104., 64., 168.]);
        assert!(layout.message > MESSAGE_MIN_WIDTH);
        assert!(fills(&layout, NARROWEST));
        assert!(time_shows_millis(layout.width(LogColumn::Time)));
    }

    #[test]
    fn message_takes_the_rest() {
        let layout = layout_columns(&LogColumnWidths::default(), OPENING);
        assert_eq!(widths(&layout), [104., 64., 168.]);
        assert!(fills(&layout, OPENING));
    }

    #[test]
    fn short_of_room_time_drops_its_millis_then_the_others_give_way() {
        let mut settings = LogColumnWidths::default();
        settings.set_width(LogColumn::Source, 360.);
        // 26 + 36 + 104 + 64 + 360 + 160 = 750: room for all of it.
        let layout = layout_columns(&settings, 750.);
        assert_eq!(widths(&layout), [104., 64., 360.]);
        assert_eq!(layout.message, MESSAGE_MIN_WIDTH);
        // 20 short: Time alone gives it, all 34 of its milliseconds.
        let layout = layout_columns(&settings, 730.);
        assert_eq!(widths(&layout), [70., 64., 360.]);
        assert_eq!(layout.message, 174.);
        assert!(!time_shows_millis(layout.width(LogColumn::Time)));
        // 100 short: Time's 34, then Level and Source share the other 66
        // by what they have above their minimums (12 and 312).
        let layout = layout_columns(&settings, 650.);
        let [time, level, source] = widths(&layout);
        assert_eq!(time, 70.);
        assert!((level - (64. - 66. * 12. / 324.)).abs() < 0.01, "{level}");
        assert!(
            (source - (360. - 66. * 312. / 324.)).abs() < 0.01,
            "{source}"
        );
        assert_eq!(layout.message, MESSAGE_MIN_WIDTH);
        assert!(fills(&layout, 650.));
        // Past every minimum the row is wider than the list.
        let layout = layout_columns(&settings, 300.);
        assert_eq!(widths(&layout), LogColumn::ALL.map(LogColumn::min_width));
        assert_eq!(layout.message, MESSAGE_MIN_WIDTH);
    }

    #[test]
    fn a_drag_moves_the_boundary_with_the_pointer() {
        let settings = LogColumnWidths::default();
        let layout = layout_columns(&settings, OPENING);
        let message = layout.message;
        let dragged = drag_boundary(&settings, &layout, LogColumn::Source, 40.);
        assert_eq!(dragged.width(LogColumn::Source), 208.);
        let after = layout_columns(&dragged, OPENING);
        assert_eq!(widths(&after), [104., 64., 208.]);
        assert_eq!(after.message, message - 40.);
        // Narrower, the other way.
        let dragged = drag_boundary(&settings, &layout, LogColumn::Time, -10.);
        assert_eq!(
            layout_columns(&dragged, OPENING).width(LogColumn::Time),
            94.
        );
    }

    #[test]
    fn a_drag_stops_at_the_minimums_and_the_maximum() {
        let settings = LogColumnWidths::default();
        let layout = layout_columns(&settings, OPENING);
        // Not below the column's own minimum.
        let dragged = drag_boundary(&settings, &layout, LogColumn::Level, -500.);
        assert_eq!(
            dragged.width(LogColumn::Level),
            LogColumn::Level.min_width()
        );
        // Not past Message's minimum.
        let dragged = drag_boundary(&settings, &layout, LogColumn::Source, 5000.);
        let after = layout_columns(&dragged, OPENING);
        assert_eq!(after.message, MESSAGE_MIN_WIDTH);
        let source = 168. + (layout.message - MESSAGE_MIN_WIDTH).round();
        assert_eq!(widths(&after), [104., 64., source]);
        // Not past MAX_WIDTH, however wide the list.
        let wide = layout_columns(&settings, 4000.);
        let dragged = drag_boundary(&settings, &wide, LogColumn::Source, 5000.);
        assert_eq!(dragged.width(LogColumn::Source), MAX_WIDTH);
    }

    #[test]
    fn a_drag_in_a_squeezed_row_keeps_the_others_where_they_are() {
        let mut settings = LogColumnWidths::default();
        settings.set_width(LogColumn::Source, 360.);
        let layout = layout_columns(&settings, 650.);
        let before = widths(&layout);
        // Level narrower by 4: Source keeps the width it was squeezed to
        // rather than taking some of that back, so only Message grows.
        let dragged = drag_boundary(&settings, &layout, LogColumn::Level, -4.);
        let after = layout_columns(&dragged, 650.);
        assert!((after.width(LogColumn::Source) - before[2].round()).abs() < 0.01);
        assert!((after.width(LogColumn::Level) - (before[1] - 4.).round()).abs() < 0.01);
        assert_eq!(after.width(LogColumn::Time), 70.);
        assert!(fills(&after, 650.));
    }

    #[test]
    fn widths_round_trip_and_bad_ones_are_skipped() {
        let settings: LogColumnWidths = serde_json::from_str(
            r#"{"widths": {"source": 220.4, "message": 90, "level": 5,
                           "time": 104, "nope": 1}, "visible": []}"#,
        )
        .unwrap();
        assert_eq!(settings.width(LogColumn::Source), 220.);
        assert_eq!(
            settings.width(LogColumn::Level),
            LogColumn::Level.min_width()
        );
        // The default width is not stored as a change.
        assert!(!settings.widths.contains_key(&LogColumn::Time));
        let json = serde_json::to_value(&settings).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"widths": {"source": 220.0, "level": 52.0}})
        );
        let back: LogColumnWidths = serde_json::from_value(json).unwrap();
        assert_eq!(back, settings);
        // Missing altogether, or untouched: the defaults, written as `{}`.
        let settings: LogColumnWidths = serde_json::from_str("{}").unwrap();
        assert_eq!(settings, LogColumnWidths::default());
        assert_eq!(
            serde_json::to_value(LogColumnWidths::default()).unwrap(),
            serde_json::json!({})
        );
        let mut settings = settings;
        settings.set_width(LogColumn::Time, 90.);
        settings.reset_width(LogColumn::Time);
        assert_eq!(settings, LogColumnWidths::default());
    }
}
