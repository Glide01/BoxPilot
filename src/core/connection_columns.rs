//! The Connections page's table columns: which there are, which the user
//! shows, how wide each is, and how they share the list's width. Pure, no
//! gpui — the page only draws what `layout_columns` and `drag_boundary`
//! compute, so the header and the rows always agree.
//!
//! - **Columns** (`ColumnId`) come in one fixed order. Six show by
//!   default, the table as it was before columns could be picked (Time,
//!   Host, Chain, Speed, Traffic, Duration); the others (Network,
//!   Destination, Process, Source, Inbound, Rule) are there to turn on.
//! - **Basic columns** are Host and Chain: what the connection is and where
//!   it went. At least one of them always shows (`ColumnSettings::toggle`
//!   refuses to hide the last, and loading puts Host back when a settings
//!   file has neither).
//! - **Widths.** Host and Chain are flexible: they share what the other
//!   columns leave, in proportion to their weights, never below their
//!   minimums. The others keep the width the user gave them. When even
//!   the minimums don't fit, Time first drops to `HH:MM:SS`, then the other
//!   columns give way toward their own minimums, and only then is the table
//!   wider than the list (it scrolls sideways).
//! - **Dragging a boundary** (`drag_boundary`) moves it with the pointer:
//!   the column on the side away from Host / Chain changes width and the
//!   nearest flexible column on the other side gives or takes the room —
//!   never below its minimum. Between Host and Chain the boundary trades
//!   width between the two.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Space between two columns, shared by the header and the rows.
pub const COLUMN_GAP: f32 = 8.;
/// The rows' horizontal padding and border, either side together.
pub const ROW_INSET: f32 = 26.;
/// The close button's slot at the end of each row (not a column: it is
/// always there).
pub const CLOSE_WIDTH: f32 = 28.;
/// `HH:MM:SS.mmm` in the small mono font: the Time column shows the
/// milliseconds from this width up (`time_shows_millis`).
pub const TIME_FULL_WIDTH: f32 = 104.;
/// No fixed column grows wider than this by a drag or from a settings
/// file.
pub const MAX_WIDTH: f32 = 800.;
/// Host's and Chain's weights are their widths in some window, which may
/// be a very wide one.
const MAX_WEIGHT: f32 = 10_000.;

/// One column of the Connections table, in the order they are laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ColumnId {
    /// When it opened: `HH:MM:SS.mmm`, or `HH:MM:SS` when narrow.
    Time,
    /// TCP / UDP, and the sniffed protocol.
    Network,
    /// What it reached (`host_label`). Basic and flexible.
    Host,
    /// The destination address as sing-box reports it.
    Destination,
    /// The process that opened it.
    Process,
    /// The source address.
    Source,
    /// The inbound that accepted it.
    Inbound,
    /// The route rule that matched.
    Rule,
    /// Group → … → node. Basic and flexible.
    Chain,
    /// The current rate, up over down.
    Speed,
    /// Bytes so far, up over down.
    Traffic,
    /// How long it has lived.
    Duration,
}

impl ColumnId {
    /// Every column, in the table's (and the Columns menu's) order.
    pub const ALL: [ColumnId; 12] = [
        ColumnId::Time,
        ColumnId::Network,
        ColumnId::Host,
        ColumnId::Destination,
        ColumnId::Process,
        ColumnId::Source,
        ColumnId::Inbound,
        ColumnId::Rule,
        ColumnId::Chain,
        ColumnId::Speed,
        ColumnId::Traffic,
        ColumnId::Duration,
    ];

    /// The id in the settings file.
    pub fn key(self) -> &'static str {
        match self {
            ColumnId::Time => "time",
            ColumnId::Network => "network",
            ColumnId::Host => "host",
            ColumnId::Destination => "destination",
            ColumnId::Process => "process",
            ColumnId::Source => "source",
            ColumnId::Inbound => "inbound",
            ColumnId::Rule => "rule",
            ColumnId::Chain => "chain",
            ColumnId::Speed => "speed",
            ColumnId::Traffic => "traffic",
            ColumnId::Duration => "duration",
        }
    }

    /// The column a settings file names; `None` for an id this release
    /// doesn't know (from a newer or an older one).
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|id| id.key() == key)
    }

    /// Host and Chain: at least one of them always shows.
    pub fn is_basic(self) -> bool {
        matches!(self, ColumnId::Host | ColumnId::Chain)
    }

    /// Host and Chain share the list's leftover width; the rest keep
    /// theirs.
    pub fn is_flexible(self) -> bool {
        matches!(self, ColumnId::Host | ColumnId::Chain)
    }

    /// Shown until the user says otherwise: the table as it always was.
    pub fn default_visible(self) -> bool {
        matches!(
            self,
            ColumnId::Time
                | ColumnId::Host
                | ColumnId::Chain
                | ColumnId::Speed
                | ColumnId::Traffic
                | ColumnId::Duration
        )
    }

    /// The width a column starts at, in px. For Host and Chain it is their
    /// weight in sharing the leftover room (five to four).
    pub fn default_width(self) -> f32 {
        match self {
            ColumnId::Time => TIME_FULL_WIDTH,
            // The badge, then `quic` beside it.
            ColumnId::Network => 76.,
            ColumnId::Host => 250.,
            ColumnId::Destination => 150.,
            ColumnId::Process => 120.,
            ColumnId::Source => 140.,
            ColumnId::Inbound => 90.,
            ColumnId::Rule => 150.,
            ColumnId::Chain => 200.,
            // The widest figure, `↑ 1023.9 KB/s` / `↑ 1023.9 MB`, in the
            // small mono font.
            ColumnId::Speed => 96.,
            ColumnId::Traffic => 84.,
            ColumnId::Duration => 56.,
        }
    }

    /// The narrowest a column gets, by a drag or for want of room.
    pub fn min_width(self) -> f32 {
        match self {
            // `HH:MM:SS`.
            ColumnId::Time => 70.,
            // The badge alone.
            ColumnId::Network => 40.,
            // The network badge and a few letters of the host.
            ColumnId::Host => 112.,
            ColumnId::Destination => 80.,
            ColumnId::Process => 60.,
            ColumnId::Source => 80.,
            ColumnId::Inbound => 50.,
            ColumnId::Rule => 60.,
            ColumnId::Chain => 80.,
            // The figures never get cut short: these only grow.
            ColumnId::Speed => 96.,
            ColumnId::Traffic => 84.,
            ColumnId::Duration => 56.,
        }
    }
}

/// Whether a Time column `width` wide has room for the milliseconds.
pub fn time_shows_millis(width: f32) -> bool {
    width >= TIME_FULL_WIDTH - 0.5
}

/// The user's choice of columns and widths, kept in the settings
/// (`AppSettings::connections_columns`). Always holds at least one basic
/// column, and widths within `[min_width, MAX_WIDTH]`.
///
/// Stored as `{"visible": ["time", "host", …], "widths": {"rule": 180}}`:
/// `widths` holds only what the user changed (Host's and Chain's are their
/// weights). Ids this release doesn't know are skipped on load.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "StoredColumns", into = "StoredColumns")]
pub struct ColumnSettings {
    visible: BTreeSet<ColumnId>,
    widths: BTreeMap<ColumnId, f32>,
}

impl Default for ColumnSettings {
    fn default() -> Self {
        Self {
            visible: ColumnId::ALL
                .into_iter()
                .filter(|id| id.default_visible())
                .collect(),
            widths: BTreeMap::new(),
        }
    }
}

impl ColumnSettings {
    /// A settings value showing exactly `visible` (normalized: Host comes
    /// back if neither basic column is there).
    pub fn with_visible(visible: impl IntoIterator<Item = ColumnId>) -> Self {
        let mut settings = Self {
            visible: visible.into_iter().collect(),
            widths: BTreeMap::new(),
        };
        settings.normalize();
        settings
    }

    pub fn is_visible(&self, id: ColumnId) -> bool {
        self.visible.contains(&id)
    }

    /// The shown columns, in the table's order.
    pub fn visible(&self) -> impl Iterator<Item = ColumnId> + '_ {
        ColumnId::ALL
            .into_iter()
            .filter(|id| self.visible.contains(id))
    }

    /// False only for the last basic column still showing.
    pub fn can_hide(&self, id: ColumnId) -> bool {
        !id.is_basic()
            || !self.is_visible(id)
            || self
                .visible
                .iter()
                .any(|other| *other != id && other.is_basic())
    }

    /// Show a hidden column, or hide a shown one — unless it is the last
    /// basic column (`can_hide`). Whether anything changed.
    pub fn toggle(&mut self, id: ColumnId) -> bool {
        if self.visible.contains(&id) {
            if !self.can_hide(id) {
                return false;
            }
            self.visible.remove(&id);
        } else {
            self.visible.insert(id);
        }
        true
    }

    /// The column's width as the user left it (its default if untouched).
    /// For Host and Chain: their weight.
    pub fn width(&self, id: ColumnId) -> f32 {
        self.widths
            .get(&id)
            .copied()
            .unwrap_or_else(|| id.default_width())
    }

    /// Back to the default width — for Host or Chain, both of their
    /// weights, since one is only ever set against the other.
    pub fn reset_width(&mut self, id: ColumnId) {
        if id.is_flexible() {
            self.widths.retain(|id, _| !id.is_flexible());
        } else {
            self.widths.remove(&id);
        }
    }

    fn set_width(&mut self, id: ColumnId, width: f32) {
        let width = clamp_width(id, width);
        if width == id.default_width() {
            self.widths.remove(&id);
        } else {
            self.widths.insert(id, width);
        }
    }

    /// Puts Host back when no basic column shows, and keeps widths in
    /// range.
    fn normalize(&mut self) {
        if !self.visible.iter().any(|id| id.is_basic()) {
            self.visible.insert(ColumnId::Host);
        }
        self.widths = std::mem::take(&mut self.widths)
            .into_iter()
            .filter(|(_, width)| width.is_finite())
            .map(|(id, width)| (id, clamp_width(id, width)))
            .filter(|(id, width)| *width != id.default_width())
            .collect();
    }
}

fn clamp_width(id: ColumnId, width: f32) -> f32 {
    let max = if id.is_flexible() {
        MAX_WEIGHT
    } else {
        MAX_WIDTH
    };
    // Whole pixels: the settings file stays readable.
    width.round().clamp(id.min_width(), max)
}

/// `ColumnSettings` as written to the settings file: ids as strings, so one
/// this release doesn't know is skipped instead of failing the whole file.
#[derive(Serialize, Deserialize, Default)]
struct StoredColumns {
    /// Absent: the default columns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    visible: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    widths: BTreeMap<String, f64>,
}

impl From<StoredColumns> for ColumnSettings {
    fn from(stored: StoredColumns) -> Self {
        let visible = match stored.visible {
            Some(keys) => keys
                .iter()
                .filter_map(|key| ColumnId::from_key(key))
                .collect(),
            None => ColumnSettings::default().visible,
        };
        let widths = stored
            .widths
            .iter()
            .filter_map(|(key, width)| Some((ColumnId::from_key(key)?, *width as f32)))
            .collect();
        let mut settings = Self { visible, widths };
        settings.normalize();
        settings
    }
}

impl From<ColumnSettings> for StoredColumns {
    fn from(settings: ColumnSettings) -> Self {
        Self {
            visible: Some(settings.visible().map(|id| id.key().to_string()).collect()),
            widths: settings
                .widths
                .iter()
                .map(|(id, width)| (id.key().to_string(), f64::from(*width)))
                .collect(),
        }
    }
}

/// One shown column and its width in a laid-out table.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlacedColumn {
    pub id: ColumnId,
    pub width: f32,
}

/// The shown columns at a given list width, shared by the header and the
/// rows.
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnLayout {
    pub columns: Vec<PlacedColumn>,
    /// A row's whole width: its padding, the columns, the gaps and the
    /// close slot. Wider than the list only when the columns' minimums
    /// don't fit (the table then scrolls sideways).
    pub width: f32,
    /// The narrowest the row can be with these fixed columns: Host and
    /// Chain at their minimums.
    pub min_width: f32,
}

impl ColumnLayout {
    pub fn width_of(&self, id: ColumnId) -> Option<f32> {
        self.columns
            .iter()
            .find(|column| column.id == id)
            .map(|column| column.width)
    }

    /// Whether the table is wider than `list_width` and must scroll.
    pub fn overflows(&self, list_width: f32) -> bool {
        self.width > list_width + 0.5
    }
}

/// Lay the shown columns out in a list `list_width` wide (the rows'
/// padding included).
///
/// The fixed columns take their widths; Host and Chain share the rest by
/// weight, never below their minimums. Short of room for those minimums,
/// Time drops to `HH:MM:SS` first, then the other fixed columns give way
/// toward their minimums in proportion to what they have above them; past
/// that the row is wider than the list.
pub fn layout_columns(settings: &ColumnSettings, list_width: f32) -> ColumnLayout {
    let ids: Vec<ColumnId> = settings.visible().collect();
    let overhead = ROW_INSET + CLOSE_WIDTH + COLUMN_GAP * ids.len() as f32;
    let room = (list_width - overhead).max(0.);
    let mut widths: Vec<f32> = ids
        .iter()
        .map(|id| {
            if id.is_flexible() {
                0.
            } else {
                settings.width(*id)
            }
        })
        .collect();
    let flex_min: f32 = ids
        .iter()
        .filter(|id| id.is_flexible())
        .map(|id| id.min_width())
        .sum();

    let mut deficit = widths.iter().sum::<f32>() + flex_min - room;
    if deficit > 0. {
        // Time gives up its milliseconds all at once: a column between the
        // two forms would only show the shorter one with space to spare.
        if let Some(ix) = ids.iter().position(|id| *id == ColumnId::Time) {
            let compact = ColumnId::Time.min_width().min(widths[ix]);
            deficit -= widths[ix] - compact;
            widths[ix] = compact;
        }
    }
    if deficit > 0. {
        let slack: f32 = ids
            .iter()
            .zip(&widths)
            .filter(|(id, _)| !id.is_flexible() && **id != ColumnId::Time)
            .map(|(id, width)| width - id.min_width())
            .sum();
        if slack > 0. {
            let take = deficit.min(slack) / slack;
            for (id, width) in ids.iter().zip(widths.iter_mut()) {
                if !id.is_flexible() && *id != ColumnId::Time {
                    *width -= (*width - id.min_width()) * take;
                }
            }
        }
    }

    let fixed: f32 = widths.iter().sum();
    let flex_room = (room - fixed).max(flex_min);
    let flex: Vec<usize> = (0..ids.len()).filter(|ix| ids[*ix].is_flexible()).collect();
    let weights: Vec<f32> = flex.iter().map(|ix| settings.width(ids[*ix])).collect();
    let mins: Vec<f32> = flex.iter().map(|ix| ids[*ix].min_width()).collect();
    for (ix, width) in flex.iter().zip(share(flex_room, &weights, &mins)) {
        widths[*ix] = width;
    }

    ColumnLayout {
        width: overhead + widths.iter().sum::<f32>(),
        min_width: overhead + fixed + flex_min,
        columns: ids
            .into_iter()
            .zip(widths)
            .map(|(id, width)| PlacedColumn { id, width })
            .collect(),
    }
}

/// `room` split in proportion to `weights`, none below its minimum: those
/// that would be get their minimum, and the rest share what is left.
fn share(room: f32, weights: &[f32], mins: &[f32]) -> Vec<f32> {
    let mut fixed: Vec<Option<f32>> = vec![None; weights.len()];
    loop {
        let left = room - fixed.iter().flatten().sum::<f32>();
        let total: f32 = weights
            .iter()
            .zip(&fixed)
            .filter(|(_, fixed)| fixed.is_none())
            .map(|(weight, _)| weight)
            .sum();
        let mut clamped = false;
        for ix in 0..weights.len() {
            if fixed[ix].is_none() && left * weights[ix] / total < mins[ix] {
                fixed[ix] = Some(mins[ix]);
                clamped = true;
            }
        }
        if !clamped {
            return (0..weights.len())
                .map(|ix| fixed[ix].unwrap_or_else(|| left * weights[ix] / total))
                .collect();
        }
    }
}

/// What dragging the boundary after column `boundary` of `layout` resizes,
/// and which flexible columns give or take the room, nearest first.
///
/// While a flexible column lies to the boundary's right, the column on its
/// left resizes and the flexible ones on the right make up for it; past
/// the last flexible column, the column on the boundary's right resizes
/// (from its left edge) and the flexible ones on the left make up for it.
/// Either way the boundary follows the pointer. `None` for the last
/// column's right edge, which has no handle.
pub fn boundary_target(layout: &ColumnLayout, boundary: usize) -> Option<(usize, Vec<usize>)> {
    let columns = &layout.columns;
    if boundary + 1 >= columns.len() {
        return None;
    }
    let flex_right: Vec<usize> = (boundary + 1..columns.len())
        .filter(|ix| columns[*ix].id.is_flexible())
        .collect();
    if !flex_right.is_empty() {
        Some((boundary, flex_right))
    } else {
        let flex_left = (0..=boundary)
            .rev()
            .filter(|ix| columns[*ix].id.is_flexible())
            .collect();
        Some((boundary + 1, flex_left))
    }
}

/// The settings after dragging the boundary after column `boundary` of
/// `layout` (as it was when the drag began, from `settings`) by `delta` px.
///
/// The resized column (`boundary_target`) changes by `delta` — but not
/// below its minimum, and not by more than the flexible columns on the
/// other side can give before theirs — and those flexible columns, nearest
/// first, make up the difference, so the rest of the row stays put. Host's
/// and Chain's weights become their new widths, so the next layout at the
/// same list width draws exactly this.
pub fn drag_boundary(
    settings: &ColumnSettings,
    layout: &ColumnLayout,
    boundary: usize,
    delta: f32,
) -> ColumnSettings {
    let Some((target, absorbers)) = boundary_target(layout, boundary) else {
        return settings.clone();
    };
    let mut widths: Vec<f32> = layout.columns.iter().map(|column| column.width).collect();
    let target_id = layout.columns[target].id;
    // Right of the boundary, the column grows as the pointer goes left.
    let delta = if target == boundary { delta } else { -delta };
    let slack: f32 = absorbers
        .iter()
        .map(|ix| (widths[*ix] - layout.columns[*ix].id.min_width()).max(0.))
        .sum();
    let grow = delta
        .max(target_id.min_width() - widths[target])
        .min(slack)
        .min(MAX_WIDTH - widths[target]);
    widths[target] += grow;
    // What the target took (or gave back), from the nearest first.
    let mut owed = grow;
    for ix in &absorbers {
        if owed > 0. {
            let give = owed.min((widths[*ix] - layout.columns[*ix].id.min_width()).max(0.));
            widths[*ix] -= give;
            owed -= give;
        } else {
            widths[*ix] -= owed;
            break;
        }
    }

    let mut settings = settings.clone();
    if !target_id.is_flexible() {
        settings.set_width(target_id, widths[target]);
    }
    for (column, width) in layout.columns.iter().zip(&widths) {
        if column.id.is_flexible() {
            // A weight: not rounded, so the boundary lands where it was
            // dropped.
            settings
                .widths
                .insert(column.id, width.clamp(column.id.min_width(), MAX_WEIGHT));
        }
    }
    settings
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The list's width in an 880px window beside the expanded and the
    /// collapsed sidebar, in the 1000px window BoxPilot opens at, and in a
    /// 1280px one.
    const EXPANDED_880: f32 = 606.;
    const COLLAPSED_880: f32 = 750.;
    const OPENING_1000: f32 = 870.;
    const WIDE_1280: f32 = 1150.;

    fn ids(layout: &ColumnLayout) -> Vec<ColumnId> {
        layout.columns.iter().map(|column| column.id).collect()
    }

    fn assert_fills(layout: &ColumnLayout, list_width: f32) {
        assert!(
            (layout.width - list_width).abs() < 0.01,
            "{} vs {list_width}: {layout:?}",
            layout.width
        );
    }

    #[test]
    fn defaults_are_the_table_as_it_was() {
        let settings = ColumnSettings::default();
        assert_eq!(
            settings.visible().collect::<Vec<_>>(),
            [
                ColumnId::Time,
                ColumnId::Host,
                ColumnId::Chain,
                ColumnId::Speed,
                ColumnId::Traffic,
                ColumnId::Duration
            ]
        );
        for id in ColumnId::ALL {
            assert_eq!(settings.width(id), id.default_width());
            assert!(id.min_width() <= id.default_width(), "{id:?}");
            assert_eq!(ColumnId::from_key(id.key()), Some(id));
        }
        assert_eq!(ColumnId::from_key("outbound"), None);
    }

    #[test]
    fn the_last_basic_column_stays() {
        let mut settings = ColumnSettings::default();
        assert!(settings.can_hide(ColumnId::Host));
        assert!(settings.toggle(ColumnId::Host));
        assert!(!settings.is_visible(ColumnId::Host));
        // Chain is the last basic column now.
        assert!(!settings.can_hide(ColumnId::Chain));
        assert!(!settings.toggle(ColumnId::Chain));
        assert!(settings.is_visible(ColumnId::Chain));
        // Others come and go freely.
        assert!(settings.toggle(ColumnId::Time));
        assert!(settings.toggle(ColumnId::Rule));
        assert!(settings.is_visible(ColumnId::Rule));
        // Host back: either basic column may go again.
        assert!(settings.toggle(ColumnId::Host));
        assert!(settings.can_hide(ColumnId::Chain));
        assert!(settings.can_hide(ColumnId::Host));
        // A hidden column can always be "hidden".
        assert!(settings.can_hide(ColumnId::Network));
    }

    #[test]
    fn a_file_without_a_basic_column_gets_host_back() {
        let settings: ColumnSettings =
            serde_json::from_str(r#"{"visible": ["time", "speed"]}"#).unwrap();
        assert_eq!(
            settings.visible().collect::<Vec<_>>(),
            [ColumnId::Time, ColumnId::Host, ColumnId::Speed]
        );
        let settings: ColumnSettings = serde_json::from_str(r#"{"visible": []}"#).unwrap();
        assert_eq!(settings.visible().collect::<Vec<_>>(), [ColumnId::Host]);
        assert_eq!(
            ColumnSettings::with_visible([ColumnId::Rule]),
            ColumnSettings::with_visible([ColumnId::Host, ColumnId::Rule])
        );
    }

    #[test]
    fn unknown_ids_and_bad_widths_are_skipped() {
        let settings: ColumnSettings = serde_json::from_str(
            r#"{
                "visible": ["chain", "outbound", "rule", "rule", "Host"],
                "widths": {"rule": 180.4, "outbound": 90, "speed": 5,
                           "process": 1e9, "time": 104},
                "sort": "whatever"
            }"#,
        )
        .unwrap();
        assert_eq!(
            settings.visible().collect::<Vec<_>>(),
            [ColumnId::Rule, ColumnId::Chain]
        );
        assert_eq!(settings.width(ColumnId::Rule), 180.);
        assert_eq!(settings.width(ColumnId::Speed), ColumnId::Speed.min_width());
        assert_eq!(settings.width(ColumnId::Process), MAX_WIDTH);
        // The default width is not stored as a change.
        assert!(!settings.widths.contains_key(&ColumnId::Time));
        // Missing altogether: the defaults.
        let settings: ColumnSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings, ColumnSettings::default());
    }

    #[test]
    fn settings_round_trip() {
        let mut settings = ColumnSettings::default();
        settings.toggle(ColumnId::Rule);
        settings.toggle(ColumnId::Time);
        settings.set_width(ColumnId::Rule, 200.);
        let json = serde_json::to_value(&settings).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "visible": ["host", "rule", "chain", "speed", "traffic", "duration"],
                "widths": {"rule": 200.0}
            })
        );
        let back: ColumnSettings = serde_json::from_value(json).unwrap();
        assert_eq!(back, settings);
        // Untouched widths are not written.
        let json = serde_json::to_value(ColumnSettings::default()).unwrap();
        assert!(json.get("widths").is_none(), "{json}");
    }

    #[test]
    fn host_and_chain_fill_the_list_five_to_four() {
        for list in [OPENING_1000, WIDE_1280, 2000.] {
            let layout = layout_columns(&ColumnSettings::default(), list);
            assert_fills(&layout, list);
            assert_eq!(layout.width_of(ColumnId::Time), Some(TIME_FULL_WIDTH));
            assert_eq!(layout.width_of(ColumnId::Speed), Some(96.));
            let host = layout.width_of(ColumnId::Host).unwrap();
            let chain = layout.width_of(ColumnId::Chain).unwrap();
            assert!((host / chain - 1.25).abs() < 0.001, "{layout:?}");
        }
    }

    #[test]
    fn a_narrow_list_drops_the_milliseconds_first() {
        // Beside the expanded sidebar in the narrowest window: Time keeps
        // its seconds and Host and Chain keep their minimums, all in view.
        let layout = layout_columns(&ColumnSettings::default(), EXPANDED_880);
        assert_fills(&layout, EXPANDED_880);
        assert_eq!(
            ids(&layout),
            ColumnSettings::default().visible().collect::<Vec<_>>()
        );
        let time = layout.width_of(ColumnId::Time).unwrap();
        assert_eq!(time, ColumnId::Time.min_width());
        assert!(!time_shows_millis(time));
        assert!(layout.width_of(ColumnId::Host).unwrap() >= ColumnId::Host.min_width());
        assert!(layout.width_of(ColumnId::Chain).unwrap() >= ColumnId::Chain.min_width());
        // With the sidebar collapsed there is room for all of it.
        let layout = layout_columns(&ColumnSettings::default(), COLLAPSED_880);
        assert!(time_shows_millis(layout.width_of(ColumnId::Time).unwrap()));
        assert_fills(&layout, COLLAPSED_880);
    }

    #[test]
    fn too_many_columns_give_way_then_scroll() {
        let all = ColumnSettings::with_visible(ColumnId::ALL);
        let layout = layout_columns(&all, EXPANDED_880);
        for column in &layout.columns {
            assert!(
                column.width >= column.id.min_width() - 0.01,
                "{column:?} below its minimum"
            );
        }
        // Everything at its minimum, and the row wider than the list.
        assert!(layout.overflows(EXPANDED_880));
        assert!((layout.width - layout.min_width).abs() < 0.01);
        let at_mins: f32 = ColumnId::ALL.iter().map(|id| id.min_width()).sum();
        let overhead = ROW_INSET + CLOSE_WIDTH + COLUMN_GAP * 12.;
        assert!(
            (layout.width - (at_mins + overhead)).abs() < 0.01,
            "{layout:?}"
        );

        // A little short: the fixed columns give way, nothing scrolls.
        let some = ColumnSettings::with_visible([
            ColumnId::Time,
            ColumnId::Host,
            ColumnId::Destination,
            ColumnId::Rule,
            ColumnId::Chain,
            ColumnId::Speed,
        ]);
        let layout = layout_columns(&some, EXPANDED_880);
        assert_fills(&layout, EXPANDED_880);
        let rule = layout.width_of(ColumnId::Rule).unwrap();
        assert!(rule < ColumnId::Rule.default_width() && rule >= ColumnId::Rule.min_width());
    }

    #[test]
    fn a_wide_user_column_takes_room_from_host_and_chain() {
        let mut settings = ColumnSettings::default();
        settings.set_width(ColumnId::Duration, 300.);
        let before = layout_columns(&ColumnSettings::default(), WIDE_1280);
        let after = layout_columns(&settings, WIDE_1280);
        assert_fills(&after, WIDE_1280);
        assert_eq!(after.width_of(ColumnId::Duration), Some(300.));
        assert!(after.width_of(ColumnId::Host) < before.width_of(ColumnId::Host));
    }

    /// Where the boundary after column `ix` is, from the row's start.
    fn edge(layout: &ColumnLayout, ix: usize) -> f32 {
        ROW_INSET / 2.
            + layout.columns[..=ix]
                .iter()
                .map(|column| column.width)
                .sum::<f32>()
            + COLUMN_GAP * ix as f32
    }

    /// Drag the boundary after `ix` by `delta` and lay out again at
    /// `list`: the boundary must have followed the pointer, and the row
    /// still fill the list.
    fn drag_and_check(
        settings: &ColumnSettings,
        list: f32,
        ix: usize,
        delta: f32,
    ) -> ColumnSettings {
        let layout = layout_columns(settings, list);
        let dragged = drag_boundary(settings, &layout, ix, delta);
        let after = layout_columns(&dragged, list);
        assert_fills(&after, list);
        assert!(
            (edge(&after, ix) - (edge(&layout, ix) + delta)).abs() < 0.6,
            "boundary {ix} by {delta}: {layout:?} → {after:?}"
        );
        dragged
    }

    #[test]
    fn dragging_between_host_and_chain_trades_width() {
        let settings = ColumnSettings::default();
        let layout = layout_columns(&settings, WIDE_1280);
        let host = layout.width_of(ColumnId::Host).unwrap();
        let chain = layout.width_of(ColumnId::Chain).unwrap();
        // Host is column 1, its right edge the boundary to Chain.
        let dragged = drag_and_check(&settings, WIDE_1280, 1, 60.);
        let after = layout_columns(&dragged, WIDE_1280);
        assert!((after.width_of(ColumnId::Host).unwrap() - (host + 60.)).abs() < 0.01);
        assert!((after.width_of(ColumnId::Chain).unwrap() - (chain - 60.)).abs() < 0.01);
        assert_eq!(after.width_of(ColumnId::Speed), Some(96.));
        // The share holds in another window width.
        let wide = layout_columns(&dragged, 2000.);
        let ratio =
            wide.width_of(ColumnId::Host).unwrap() / wide.width_of(ColumnId::Chain).unwrap();
        assert!((ratio - (host + 60.) / (chain - 60.)).abs() < 0.001);
        // Not past Chain's minimum.
        let squeezed = drag_boundary(&settings, &layout, 1, 5000.);
        let after = layout_columns(&squeezed, WIDE_1280);
        assert!(
            (after.width_of(ColumnId::Chain).unwrap() - ColumnId::Chain.min_width()).abs() < 0.01
        );
        assert_fills(&after, WIDE_1280);
    }

    #[test]
    fn dragging_left_of_host_resizes_the_column_on_the_left() {
        let settings = ColumnSettings::default();
        // Time | Host.
        let dragged = drag_and_check(&settings, WIDE_1280, 0, 30.);
        assert_eq!(dragged.width(ColumnId::Time), TIME_FULL_WIDTH + 30.);
        // Not below Time's minimum.
        let layout = layout_columns(&settings, WIDE_1280);
        let narrowed = drag_boundary(&settings, &layout, 0, -500.);
        assert_eq!(narrowed.width(ColumnId::Time), ColumnId::Time.min_width());
    }

    #[test]
    fn dragging_right_of_chain_resizes_the_column_on_the_right() {
        let settings = ColumnSettings::default();
        // Chain | Speed, dragged left: Speed grows from its left edge.
        let dragged = drag_and_check(&settings, WIDE_1280, 2, -40.);
        assert_eq!(dragged.width(ColumnId::Speed), 136.);
        // Speed | Traffic, dragged left: Traffic grows, Speed moves.
        let dragged = drag_and_check(&settings, WIDE_1280, 3, -10.);
        assert_eq!(dragged.width(ColumnId::Traffic), 94.);
        assert_eq!(dragged.width(ColumnId::Speed), 96.);
        // Not below its minimum, which for the figures is where they
        // start.
        let layout = layout_columns(&settings, WIDE_1280);
        let narrowed = drag_boundary(&settings, &layout, 3, 30.);
        assert_eq!(narrowed.width(ColumnId::Traffic), 84.);
        // The last column's right edge has no handle.
        let layout = layout_columns(&settings, WIDE_1280);
        assert_eq!(boundary_target(&layout, 5), None);
        assert_eq!(drag_boundary(&settings, &layout, 5, 50.), settings);
    }

    #[test]
    fn host_and_chain_give_room_nearest_first_and_no_further() {
        let settings = ColumnSettings::default();
        let layout = layout_columns(&settings, WIDE_1280);
        // Speed grows by far more than Chain has: Host gives the rest, down
        // to both minimums, and no further.
        let dragged = drag_boundary(&settings, &layout, 2, -2000.);
        let after = layout_columns(&dragged, WIDE_1280);
        assert_fills(&after, WIDE_1280);
        assert!(
            (after.width_of(ColumnId::Chain).unwrap() - ColumnId::Chain.min_width()).abs() < 0.01
        );
        assert!(
            (after.width_of(ColumnId::Host).unwrap() - ColumnId::Host.min_width()).abs() < 0.01
        );
        // A little: only Chain gives.
        let dragged = drag_boundary(&settings, &layout, 2, -20.);
        let after = layout_columns(&dragged, WIDE_1280);
        assert!(
            (after.width_of(ColumnId::Host).unwrap() - layout.width_of(ColumnId::Host).unwrap())
                .abs()
                < 0.01
        );
    }

    #[test]
    fn dragging_with_one_basic_column_and_extras() {
        let settings = ColumnSettings::with_visible([
            ColumnId::Network,
            ColumnId::Destination,
            ColumnId::Rule,
            ColumnId::Chain,
            ColumnId::Duration,
        ]);
        let list = OPENING_1000;
        // Network | Destination, Destination | Rule: left of Chain.
        drag_and_check(&settings, list, 0, 20.);
        let dragged = drag_and_check(&settings, list, 1, -30.);
        assert_eq!(dragged.width(ColumnId::Destination), 120.);
        // Chain | Duration: Duration resizes.
        let dragged = drag_and_check(&settings, list, 3, -24.);
        assert_eq!(dragged.width(ColumnId::Duration), 80.);
        // Rule | Chain: Rule resizes, Chain gives.
        let dragged = drag_and_check(&settings, list, 2, 50.);
        assert_eq!(dragged.width(ColumnId::Rule), 200.);
    }

    #[test]
    fn reset_width_restores_the_defaults() {
        let settings = ColumnSettings::default();
        let layout = layout_columns(&settings, WIDE_1280);
        let mut dragged = drag_boundary(&settings, &layout, 1, 80.);
        dragged = drag_boundary(&dragged, &layout_columns(&dragged, WIDE_1280), 0, 20.);
        dragged.reset_width(ColumnId::Chain);
        assert_eq!(
            dragged.width(ColumnId::Host),
            ColumnId::Host.default_width()
        );
        assert_eq!(
            dragged.width(ColumnId::Chain),
            ColumnId::Chain.default_width()
        );
        assert_eq!(dragged.width(ColumnId::Time), TIME_FULL_WIDTH + 20.);
        dragged.reset_width(ColumnId::Time);
        assert_eq!(dragged, settings);
    }
}
