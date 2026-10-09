//! The Logs page's lines: sing-box's two log sources merged into one bounded
//! view, without showing a line twice. Pure — no gpui, no threads; time is
//! passed in, so every rule here is unit-tested.
//!
//! The two sources see different parts of the same log:
//!
//! - **API** (`SubscribeLog`, see `singbox_api::stream_logs`): every level,
//!   whatever `log.level` says, with the level as data. But only from the
//!   moment the `api` service started (the first startup lines are missing),
//!   and only while sing-box is up.
//! - **Pipes** (sing-box's stdout/stderr): only lines at `log.level` or more
//!   severe, but from the first byte to the last: config errors, startup
//!   failures, deprecation warnings (stderr only), panics, and whatever
//!   sing-box writes while exiting.
//!
//! sing-box writes each logged line to both. The merge, per run of sing-box
//! (`begin_run` on the Stopped→Running edge):
//!
//! 1. **Pre-API.** Pipe lines are shown as they arrive.
//! 2. **Reset.** A batch with `reset` set (the first snapshot, the snapshot
//!    after a re-subscribe, or the one `ClearLogs` triggers) replaces this
//!    run's API lines with the snapshot. Pipe lines of this run that the
//!    snapshot also carries are dropped, so what is left of the pre-API
//!    segment is exactly the startup the API never saw.
//! 3. **API live.** A pipe line is held back for `PIPE_GRACE`. If the API
//!    delivers the same line — before or after it — the pipe copy is dropped.
//!    Otherwise the line is shown when the grace runs out: it was
//!    stderr-only output.
//! 4. **Stopped** (`end_api`, on the Running→Stopped edge). Held lines are
//!    shown at once; later pipe lines (stderr after exit) are shown as they
//!    arrive, still minus any the API already delivered.
//!
//! "The same line" is the same level plus the same text after the level
//! word. The console prints a wall-clock timestamp in front of the level
//! when `log.timestamp` is set; the API never does.
//!
//! The view is bounded to `max_lines`. When full, the oldest lines hidden by
//! the level threshold go first, so a flood of debug lines the user isn't
//! looking at can't push out the warnings they are.

use crate::core::singbox_api::{strip_ansi, LogBatch, LogLevel};
use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long a pipe line waits for its API twin while the API stream is live
/// (and how long an API line waits for its pipe twin). Covers both drain
/// intervals and the re-subscribe gap after an idle timeout, with margin.
pub const PIPE_GRACE: Duration = Duration::from_secs(3);

/// The level word sits within the first few words of a console line:
/// `INFO[0012] …`, or `+0800 2026-01-02 15:04:05 INFO …` with timestamps.
const MAX_PREFIX_WORDS: usize = 4;

/// Where a shown line came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogSource {
    /// sing-box's stdout/stderr.
    Pipe,
    /// The API's `SubscribeLog` stream.
    Api,
}

/// One line of the view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    pub level: LogLevel,
    /// As sing-box formatted it, ANSI colours removed.
    pub text: String,
    /// Byte range of the level word in `text` (`INFO` in `INFO[0012] …`),
    /// where the level badge goes. `None` for output that isn't a log line
    /// (a Go panic, say).
    pub level_span: Option<Range<usize>>,
    pub source: LogSource,
    /// When the line reached BoxPilot, unix ms (wall clock). A held-back
    /// pipe line keeps the time it was read, not when it was shown.
    pub at_ms: i64,
    /// Strictly increasing in view order.
    id: u64,
    /// The sing-box run this line belongs to.
    run: u64,
    /// Where the text after the level word starts: the dedup key.
    body_start: usize,
}

/// A parsed line, not yet in the view.
#[derive(Clone, Debug)]
struct Line {
    level: LogLevel,
    text: String,
    level_span: Option<Range<usize>>,
    body_start: usize,
}

type Key = (LogLevel, String);

impl Line {
    /// A pipe line. Anything without a level word is non-log output on
    /// stderr (panics, runtime errors), so it counts as `Error` and shows
    /// under the default threshold.
    fn from_pipe(raw: &str) -> Self {
        let text = strip_ansi(raw).into_owned();
        match find_level(&text) {
            Some((level, span, body_start)) => Self {
                level,
                text,
                level_span: Some(span),
                body_start,
            },
            None => Self {
                level: LogLevel::Error,
                text,
                level_span: None,
                body_start: 0,
            },
        }
    }

    /// An API line: the level comes with it; the text still says it too.
    fn from_api(level: LogLevel, message: &str) -> Self {
        let text = strip_ansi(message).into_owned();
        let (level_span, body_start) = match find_level(&text) {
            Some((_, span, body_start)) => (Some(span), body_start),
            None => (None, 0),
        };
        Self {
            level,
            text,
            level_span,
            body_start,
        }
    }

    fn key(&self) -> Key {
        (self.level, self.text[self.body_start..].to_string())
    }
}

impl LogEntry {
    fn key(&self) -> Key {
        (self.level, self.text[self.body_start..].to_string())
    }

    /// Strictly increasing in view order: a stable handle on the line.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The line split for the Logs table: see [`LineParts`].
    pub fn parts(&self) -> LineParts<'_> {
        LineParts::parse(&self.text[self.body_start..])
    }
}

/// Most a source may be: longer, the words before a colon are prose, not a
/// component's name.
const MAX_SOURCE_LEN: usize = 48;

/// A log line's text after the level word, split into columns. sing-box
/// writes `[3417626869 12ms] inbound/mixed[mixed-in]: inbound connection
/// from …`: the connection tag, the component that logged, and the
/// message. Each part but the message is optional; a line that isn't
/// shaped like that (a Go panic) is all message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineParts<'a> {
    /// `[3417626869 12ms]`: which connection, and how long it has run.
    pub tag: Option<&'a str>,
    /// `inbound/mixed[mixed-in]`, `router`, `dns`.
    pub source: Option<&'a str>,
    pub message: &'a str,
}

impl<'a> LineParts<'a> {
    pub fn parse(body: &'a str) -> Self {
        let mut rest = body.trim();
        let mut tag = None;
        if rest.starts_with('[') {
            if let Some(end) = rest.find("] ") {
                tag = Some(&rest[..=end]);
                rest = rest[end + 2..].trim_start();
            }
        }
        let source = rest.find(": ").and_then(|end| {
            let name = &rest[..end];
            // A component's tag, in brackets after it, may hold spaces (an
            // outbound named "Japan 02"); the rest of the name may not.
            let bare = match name.find('[') {
                Some(open) if name.ends_with(']') => &name[..open],
                _ => name,
            };
            (!name.is_empty()
                && name.len() <= MAX_SOURCE_LEN
                && !bare.contains(char::is_whitespace))
            .then_some(name)
        });
        if let Some(name) = source {
            rest = rest[name.len() + 2..].trim_start();
        }
        Self {
            tag,
            source,
            message: rest,
        }
    }
}

/// The Logs page's search: whitespace-separated terms, each of which must
/// appear in the line (case-insensitively) — or, written `-term`, must not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogQuery {
    include: Vec<String>,
    exclude: Vec<String>,
}

impl LogQuery {
    pub fn parse(query: &str) -> Self {
        let mut parsed = Self::default();
        for term in query.split_whitespace() {
            match term.strip_prefix('-') {
                Some(excluded) if !excluded.is_empty() => {
                    parsed.exclude.push(excluded.to_lowercase())
                }
                // A lone "-" is a term like any other.
                _ => parsed.include.push(term.to_lowercase()),
            }
        }
        parsed
    }

    pub fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }

    pub fn matches(&self, entry: &LogEntry) -> bool {
        if self.is_empty() {
            return true;
        }
        let text = entry.text.to_lowercase();
        self.include.iter().all(|term| text.contains(term.as_str()))
            && !self.exclude.iter().any(|term| text.contains(term.as_str()))
    }
}

/// The level word of a console line: its level, its byte range, and where
/// the text after it starts. A word is a level word when it is one of
/// sing-box's upper-case level names, optionally followed by `[…]`.
fn find_level(text: &str) -> Option<(LogLevel, Range<usize>, usize)> {
    let mut pos = 0;
    for _ in 0..MAX_PREFIX_WORDS {
        let start = pos + (text[pos..].len() - text[pos..].trim_start().len());
        if start >= text.len() {
            return None;
        }
        let end = text[start..]
            .find(char::is_whitespace)
            .map_or(text.len(), |i| start + i);
        let word = &text[start..end];
        let name = &word[..word.find('[').unwrap_or(word.len())];
        if let Some(level) = level_from_name(name) {
            let body_start = end + (text[end..].len() - text[end..].trim_start().len());
            return Some((level, start..start + name.len(), body_start));
        }
        pos = end;
    }
    None
}

fn level_from_name(name: &str) -> Option<LogLevel> {
    Some(match name {
        "PANIC" => LogLevel::Panic,
        "FATAL" => LogLevel::Fatal,
        "ERROR" => LogLevel::Error,
        "WARN" => LogLevel::Warn,
        "INFO" => LogLevel::Info,
        "DEBUG" => LogLevel::Debug,
        "TRACE" => LogLevel::Trace,
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// No run, or the run ended: pipe lines show at once.
    Stopped,
    /// Running, API stream not yet connected: pipe lines show at once and
    /// stay matchable.
    PreApi,
    /// API stream live: pipe lines are held back.
    Api,
}

/// A pipe line waiting for its API twin.
#[derive(Debug)]
struct PendingPipe {
    key: Key,
    at: Instant,
    state: PendingState,
}

#[derive(Debug)]
enum PendingState {
    /// Already in the view (pre-API); drop it from there on a match.
    Shown(u64),
    /// Not shown yet (API live); show it when the grace runs out.
    Held(Line),
}

/// The merged, bounded log view. See the module docs for the rules.
#[derive(Debug)]
pub struct LogMerge {
    entries: VecDeque<LogEntry>,
    max_lines: usize,
    /// Lines less severe than this are evicted first when full.
    keep_level: LogLevel,
    next_id: u64,
    run: u64,
    phase: Phase,
    /// Pipe lines without an API twin yet, oldest first.
    pending: VecDeque<PendingPipe>,
    /// API lines without a pipe twin yet: arrival times, oldest first.
    recent_api: HashMap<Key, VecDeque<Instant>>,
    /// An instant and the wall clock (unix ms) then: the merge runs on
    /// `Instant`s, the lines are stamped with wall-clock time from them.
    epoch: (Instant, i64),
}

impl LogMerge {
    pub fn new(max_lines: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            max_lines,
            keep_level: LogLevel::Trace,
            next_id: 0,
            run: 0,
            phase: Phase::Stopped,
            pending: VecDeque::new(),
            recent_api: HashMap::new(),
            epoch: (Instant::now(), unix_millis(SystemTime::now())),
        }
    }

    /// `at` as wall-clock unix ms, rounded down either side of the epoch
    /// alike, so instants keep their spacing.
    fn wall_ms(&self, at: Instant) -> i64 {
        let (base, base_ms) = self.epoch;
        let nanos = match at.checked_duration_since(base) {
            Some(after) => after.as_nanos() as i128,
            None => -(base.duration_since(at).as_nanos() as i128),
        };
        base_ms + nanos.div_euclid(1_000_000) as i64
    }

    /// Oldest first.
    pub fn entries(&self) -> &VecDeque<LogEntry> {
        &self.entries
    }

    /// Set the level threshold the view is shown at, so eviction can spare
    /// the visible lines. Never evicts by itself.
    pub fn set_keep_level(&mut self, level: LogLevel) {
        self.keep_level = level;
    }

    /// sing-box started (Stopped→Running edge): a new run begins, before
    /// any of its output can arrive.
    pub fn begin_run(&mut self) {
        self.run += 1;
        self.phase = Phase::PreApi;
        self.pending.clear();
        self.recent_api.clear();
    }

    /// The API stream for this run is over (Running→Stopped edge): show what
    /// is held back, and show later pipe lines as they arrive. Returns
    /// whether the view changed.
    pub fn end_api(&mut self) -> bool {
        self.phase = Phase::Stopped;
        let mut changed = false;
        for pending in std::mem::take(&mut self.pending) {
            if let PendingState::Held(line) = pending.state {
                self.show(line, LogSource::Pipe, pending.at);
                changed = true;
            }
        }
        self.trim();
        changed
    }

    /// Lines read from sing-box's stdout/stderr, oldest first, raw (ANSI
    /// colours included). Returns whether the view changed.
    pub fn push_pipe<S: AsRef<str>>(&mut self, lines: &[S], now: Instant) -> bool {
        let mut changed = false;
        for raw in lines {
            let raw = raw.as_ref();
            if raw.trim().is_empty() {
                continue;
            }
            let line = Line::from_pipe(raw);
            let key = line.key();
            if self.take_recent_api(&key, now) {
                continue; // The API already showed it.
            }
            match self.phase {
                Phase::Api => self.pending.push_back(PendingPipe {
                    key,
                    at: now,
                    state: PendingState::Held(line),
                }),
                Phase::PreApi => {
                    let id = self.show(line, LogSource::Pipe, now);
                    self.pending.push_back(PendingPipe {
                        key,
                        at: now,
                        state: PendingState::Shown(id),
                    });
                    changed = true;
                }
                Phase::Stopped => {
                    self.show(line, LogSource::Pipe, now);
                    changed = true;
                }
            }
        }
        // Bound the backlog too: past `max_lines`, the oldest stop waiting.
        while self.pending.len() > self.max_lines {
            if let Some(PendingPipe {
                state: PendingState::Held(line),
                at,
                ..
            }) = self.pending.pop_front()
            {
                self.show(line, LogSource::Pipe, at);
                changed = true;
            }
        }
        self.trim();
        changed
    }

    /// One `SubscribeLog` message. Returns whether the view changed.
    pub fn push_api(&mut self, batch: LogBatch, now: Instant) -> bool {
        if self.phase == Phase::Stopped {
            // A batch can only follow `begin_run`; a stray one is stale.
            return false;
        }
        self.phase = Phase::Api;
        let lines: Vec<Line> = batch
            .lines
            .iter()
            .map(|l| Line::from_api(l.level, &l.message))
            .collect();
        if batch.reset {
            self.apply_reset(lines, now);
            self.trim();
            return true;
        }
        let changed = !lines.is_empty();
        for line in lines {
            let key = line.key();
            if !self.take_pending_pipe(&key, now) {
                self.recent_api.entry(key).or_default().push_back(now);
            }
            self.show(line, LogSource::Api, now);
        }
        self.trim();
        changed
    }

    /// Whether a pipe line is held back, waiting for its API twin or for
    /// the grace to run out — i.e. whether `tick` can still change the view
    /// without new input. The drain only runs a clock while this is true.
    pub fn has_pending(&self) -> bool {
        self.pending
            .iter()
            .any(|p| matches!(p.state, PendingState::Held(_)))
    }

    /// Advance the clock: show held pipe lines whose grace ran out, forget
    /// expired matches. Returns whether the view changed.
    pub fn tick(&mut self, now: Instant) -> bool {
        let mut changed = false;
        while self
            .pending
            .front()
            .is_some_and(|p| now.saturating_duration_since(p.at) >= PIPE_GRACE)
        {
            if let Some(PendingPipe {
                state: PendingState::Held(line),
                at,
                ..
            }) = self.pending.pop_front()
            {
                self.show(line, LogSource::Pipe, at);
                changed = true;
            }
        }
        self.recent_api.retain(|_, times| {
            while times
                .front()
                .is_some_and(|t| now.saturating_duration_since(*t) >= PIPE_GRACE)
            {
                times.pop_front();
            }
            !times.is_empty()
        });
        self.trim();
        changed
    }

    /// Empty the view (the Clear button). Lines held back are dropped with
    /// it; what the API has delivered stays known, so a late pipe twin of a
    /// cleared line doesn't reappear. Returns whether anything was dropped.
    pub fn clear(&mut self) -> bool {
        let changed = !self.entries.is_empty() || !self.pending.is_empty();
        self.entries.clear();
        self.pending.clear();
        changed
    }

    /// Replace this run's API lines with `snapshot`, dropping this run's pipe
    /// lines the snapshot also carries (shown or held).
    fn apply_reset(&mut self, snapshot: Vec<Line>, now: Instant) {
        let run = self.run;
        let mut unmatched: HashMap<Key, usize> = HashMap::new();
        for line in &snapshot {
            *unmatched.entry(line.key()).or_default() += 1;
        }
        let mut take = |key: &Key| match unmatched.get_mut(key) {
            Some(n) if *n > 0 => {
                *n -= 1;
                true
            }
            _ => false,
        };
        self.entries.retain(|e| {
            if e.run != run {
                return true;
            }
            match e.source {
                LogSource::Api => false,
                LogSource::Pipe => !take(&e.key()),
            }
        });
        let entries = &self.entries;
        self.pending.retain(|p| match &p.state {
            PendingState::Held(_) => !take(&p.key),
            // Its line may have just gone as a match above.
            PendingState::Shown(id) => entries.binary_search_by_key(id, |e| e.id).is_ok(),
        });
        // Snapshot lines whose pipe twin may still be on its way.
        self.recent_api.clear();
        for (key, n) in unmatched {
            if n > 0 {
                self.recent_api
                    .insert(key, std::iter::repeat_n(now, n).collect());
            }
        }
        for line in snapshot {
            self.show(line, LogSource::Api, now);
        }
    }

    /// Consume an API line's pipe twin, if one is waiting (within grace).
    fn take_pending_pipe(&mut self, key: &Key, now: Instant) -> bool {
        let Some(index) = self
            .pending
            .iter()
            .position(|p| &p.key == key && now.saturating_duration_since(p.at) < PIPE_GRACE)
        else {
            return false;
        };
        if let Some(PendingPipe {
            state: PendingState::Shown(id),
            ..
        }) = self.pending.remove(index)
        {
            if let Ok(pos) = self.entries.binary_search_by_key(&id, |e| e.id) {
                self.entries.remove(pos);
            }
        }
        true
    }

    /// Consume a pipe line's API twin, if one arrived within grace.
    fn take_recent_api(&mut self, key: &Key, now: Instant) -> bool {
        let Some(times) = self.recent_api.get_mut(key) else {
            return false;
        };
        while times
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= PIPE_GRACE)
        {
            times.pop_front();
        }
        let found = times.pop_front().is_some();
        if times.is_empty() {
            self.recent_api.remove(key);
        }
        found
    }

    fn show(&mut self, line: Line, source: LogSource, at: Instant) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let at_ms = self.wall_ms(at);
        self.entries.push_back(LogEntry {
            level: line.level,
            text: line.text,
            level_span: line.level_span,
            source,
            at_ms,
            id,
            run: self.run,
            body_start: line.body_start,
        });
        id
    }

    /// Enforce `max_lines`: evict the oldest lines hidden at `keep_level`
    /// first, then the oldest of the rest.
    fn trim(&mut self) {
        let mut excess = self.entries.len().saturating_sub(self.max_lines);
        if excess == 0 {
            return;
        }
        let keep_level = self.keep_level;
        self.entries.retain(|e| {
            if excess > 0 && e.level > keep_level {
                excess -= 1;
                false
            } else {
                true
            }
        });
        for _ in 0..excess {
            self.entries.pop_front();
        }
    }
}

fn unix_millis(at: SystemTime) -> i64 {
    at.duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as i64)
}

/// The lines visible at `threshold` (that level or more severe).
pub fn visible(
    entries: &VecDeque<LogEntry>,
    threshold: LogLevel,
) -> impl Iterator<Item = &LogEntry> {
    entries.iter().filter(move |e| e.level <= threshold)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::singbox_api::LogLine;

    const ESC: &str = "\x1b";

    fn info(body: &str) -> String {
        format!("{ESC}[36mINFO{ESC}[0m[0000] {body}")
    }

    fn api_line(level: LogLevel, body: &str) -> LogLine {
        let word = level.as_str().to_uppercase();
        LogLine {
            level,
            message: format!("{ESC}[36m{word}{ESC}[0m[0000] {body}"),
        }
    }

    fn reset(lines: Vec<LogLine>) -> LogBatch {
        LogBatch { reset: true, lines }
    }

    fn append(lines: Vec<LogLine>) -> LogBatch {
        LogBatch {
            reset: false,
            lines,
        }
    }

    fn texts(merge: &LogMerge) -> Vec<String> {
        merge.entries().iter().map(|e| e.text.clone()).collect()
    }

    fn sources(merge: &LogMerge) -> Vec<LogSource> {
        merge.entries().iter().map(|e| e.source).collect()
    }

    #[test]
    fn finds_the_level_word_in_both_console_formats() {
        let (level, span, body) = find_level("INFO[0012] [42 5ms] router: ok").unwrap();
        assert_eq!(level, LogLevel::Info);
        assert_eq!(span, 0..4);
        assert_eq!(
            &"INFO[0012] [42 5ms] router: ok"[body..],
            "[42 5ms] router: ok"
        );

        let stamped = "+0800 2026-10-02 11:23:04 WARN dns: slow";
        let (level, span, body) = find_level(stamped).unwrap();
        assert_eq!(level, LogLevel::Warn);
        assert_eq!(&stamped[span], "WARN");
        assert_eq!(&stamped[body..], "dns: slow");

        assert!(find_level("panic: runtime error").is_none());
        assert!(find_level("goroutine 1 [running]:").is_none());
        assert!(find_level("").is_none());
        assert!(find_level("a b c d INFO too far").is_none());
    }

    #[test]
    fn pipe_lines_get_level_and_lose_ansi() {
        let line = Line::from_pipe(&info("sing-box started"));
        assert_eq!(line.level, LogLevel::Info);
        assert_eq!(line.text, "INFO[0000] sing-box started");
        assert_eq!(line.level_span, Some(0..4));

        let panic = Line::from_pipe("panic: boom");
        assert_eq!(
            panic.level,
            LogLevel::Error,
            "non-log stderr counts as an error"
        );
        assert_eq!(panic.level_span, None);
    }

    /// The console with `log.timestamp` and the API's `LEVEL[secs]` form are
    /// the same line.
    #[test]
    fn dedup_key_ignores_the_timestamp_format() {
        let console = Line::from_pipe("+0000 2026-10-02 11:23:04 INFO sing-box started (0.00s)");
        let api = Line::from_api(LogLevel::Info, &info("sing-box started (0.00s)"));
        assert_eq!(console.key(), api.key());
    }

    #[test]
    fn pipe_lines_show_at_once_without_a_run() {
        let now = Instant::now();
        let mut merge = LogMerge::new(100);
        assert!(merge.push_pipe(&["FATAL[0000] decode config: unknown field"], now));
        assert_eq!(merge.entries()[0].level, LogLevel::Fatal);
    }

    /// The first snapshot replaces the pre-API segment's copies; the startup
    /// lines the API never saw stay, in front.
    #[test]
    fn first_reset_keeps_only_the_startup_the_api_missed() {
        let now = Instant::now();
        let mut merge = LogMerge::new(100);
        merge.begin_run();
        merge.push_pipe(
            &[
                info("inbound/mixed[proxy]: tcp server started"),
                info("service/api: tcp server started"),
                info("sing-box started"),
            ],
            now,
        );
        assert_eq!(merge.entries().len(), 3);

        merge.push_api(
            reset(vec![
                api_line(LogLevel::Info, "service/api: tcp server started"),
                api_line(LogLevel::Debug, "router: rule-set loaded"),
                api_line(LogLevel::Info, "sing-box started"),
            ]),
            now,
        );
        assert_eq!(
            texts(&merge),
            vec![
                "INFO[0000] inbound/mixed[proxy]: tcp server started",
                "INFO[0000] service/api: tcp server started",
                "DEBUG[0000] router: rule-set loaded",
                "INFO[0000] sing-box started",
            ]
        );
        assert_eq!(
            sources(&merge),
            vec![
                LogSource::Pipe,
                LogSource::Api,
                LogSource::Api,
                LogSource::Api
            ]
        );
        assert_eq!(merge.entries()[2].level, LogLevel::Debug);
    }

    /// A line logged right after the snapshot can reach the view through the
    /// pipe first; its API copy then replaces it.
    #[test]
    fn pipe_line_shown_before_the_api_is_replaced_by_its_twin() {
        let now = Instant::now();
        let mut merge = LogMerge::new(100);
        merge.begin_run();
        merge.push_pipe(&[info("early"), info("just after snapshot")], now);
        merge.push_api(reset(vec![]), now);
        assert_eq!(merge.entries().len(), 2, "unmatched pre-API lines stay");

        merge.push_api(
            append(vec![api_line(LogLevel::Info, "just after snapshot")]),
            now,
        );
        assert_eq!(
            texts(&merge),
            vec!["INFO[0000] early", "INFO[0000] just after snapshot"]
        );
        assert_eq!(sources(&merge), vec![LogSource::Pipe, LogSource::Api]);
    }

    #[test]
    fn live_pipe_copies_are_dropped_whichever_arrives_first() {
        let t0 = Instant::now();
        let mut merge = LogMerge::new(100);
        merge.begin_run();
        merge.push_api(reset(vec![]), t0);

        // Pipe first: held, then its API twin arrives.
        assert!(!merge.push_pipe(&[info("a")], t0));
        assert!(
            merge.entries().is_empty(),
            "held back while the API is live"
        );
        merge.push_api(append(vec![api_line(LogLevel::Info, "a")]), t0);
        // API first: the pipe copy finds it.
        merge.push_api(append(vec![api_line(LogLevel::Info, "b")]), t0);
        assert!(!merge.push_pipe(&[info("b")], t0));

        assert!(!merge.tick(t0 + PIPE_GRACE), "nothing left to show");
        assert_eq!(texts(&merge), vec!["INFO[0000] a", "INFO[0000] b"]);
        assert!(merge.entries().iter().all(|e| e.source == LogSource::Api));
    }

    /// Deprecation warnings and panics reach stderr only: they show once the
    /// grace runs out.
    #[test]
    fn stderr_only_lines_show_after_the_grace() {
        let t0 = Instant::now();
        let mut merge = LogMerge::new(100);
        merge.begin_run();
        merge.push_api(reset(vec![]), t0);
        merge.push_pipe(&["WARN[0000] deprecated: legacy field"], t0);

        assert!(!merge.tick(t0 + PIPE_GRACE / 2));
        assert!(merge.entries().is_empty());
        assert!(merge.tick(t0 + PIPE_GRACE));
        assert_eq!(texts(&merge), vec!["WARN[0000] deprecated: legacy field"]);
        assert_eq!(merge.entries()[0].source, LogSource::Pipe);
    }

    #[test]
    fn has_pending_only_while_a_pipe_line_is_held() {
        let t0 = Instant::now();
        let mut merge = LogMerge::new(100);
        assert!(!merge.has_pending());

        // Stopped / pre-API: pipe lines show at once, nothing is held.
        merge.push_pipe(&[info("before")], t0);
        assert!(!merge.has_pending());
        merge.begin_run();
        merge.push_pipe(&[info("pre-api")], t0);
        assert!(!merge.has_pending(), "shown pre-API lines are not held");

        // API live: a pipe line waits for its twin…
        merge.push_api(reset(vec![]), t0);
        merge.push_pipe(&[info("a")], t0);
        assert!(merge.has_pending());
        merge.push_api(append(vec![api_line(LogLevel::Info, "a")]), t0);
        assert!(!merge.has_pending(), "its twin took it");

        // …or for the grace to run out.
        merge.push_pipe(&[info("stderr only")], t0);
        assert!(merge.has_pending());
        assert!(merge.tick(t0 + PIPE_GRACE));
        assert!(!merge.has_pending());

        // The end of the API stream lets held lines through.
        merge.push_pipe(&[info("late")], t0 + PIPE_GRACE);
        assert!(merge.has_pending());
        assert!(merge.end_api());
        assert!(!merge.has_pending());
    }

    /// An API twin arriving after the grace no longer matches: the line has
    /// been shown already, and the API's copy shows too (better twice than
    /// lost).
    #[test]
    fn matching_stops_after_the_grace() {
        let t0 = Instant::now();
        let mut merge = LogMerge::new(100);
        merge.begin_run();
        merge.push_api(reset(vec![]), t0);
        merge.push_api(append(vec![api_line(LogLevel::Info, "x")]), t0);
        merge.tick(t0 + PIPE_GRACE);
        merge.push_pipe(&[info("x")], t0 + PIPE_GRACE);
        merge.tick(t0 + PIPE_GRACE * 2);
        assert_eq!(merge.entries().len(), 2);
    }

    /// Re-subscribing after the idle timeout replays the ring: no duplicates,
    /// and lines logged during the gap appear once.
    #[test]
    fn resubscribe_snapshot_replaces_the_api_segment() {
        let t0 = Instant::now();
        let mut merge = LogMerge::new(100);
        merge.begin_run();
        merge.push_pipe(&[info("early")], t0);
        merge.push_api(reset(vec![api_line(LogLevel::Info, "one")]), t0);
        merge.push_api(append(vec![api_line(LogLevel::Debug, "two")]), t0);
        // Stream dropped; sing-box logs during the gap.
        merge.push_pipe(&[info("gap")], t0);

        merge.push_api(
            reset(vec![
                api_line(LogLevel::Info, "one"),
                api_line(LogLevel::Debug, "two"),
                api_line(LogLevel::Info, "gap"),
            ]),
            t0,
        );
        assert!(!merge.tick(t0 + PIPE_GRACE), "the held gap line matched");
        assert_eq!(
            texts(&merge),
            vec![
                "INFO[0000] early",
                "INFO[0000] one",
                "DEBUG[0000] two",
                "INFO[0000] gap",
            ]
        );
    }

    /// `ClearLogs` arrives as an empty reset: this run's API lines go.
    #[test]
    fn clear_logs_reset_empties_the_api_segment() {
        let t0 = Instant::now();
        let mut merge = LogMerge::new(100);
        merge.begin_run();
        merge.push_api(reset(vec![api_line(LogLevel::Info, "old")]), t0);
        assert!(merge.clear());
        assert!(merge.push_api(reset(vec![]), t0));
        merge.push_api(append(vec![api_line(LogLevel::Info, "new")]), t0);
        assert_eq!(texts(&merge), vec!["INFO[0000] new"]);
    }

    /// A previous run's lines are history: a new run's snapshot leaves them.
    #[test]
    fn reset_only_touches_the_current_run() {
        let t0 = Instant::now();
        let mut merge = LogMerge::new(100);
        merge.begin_run();
        merge.push_api(reset(vec![api_line(LogLevel::Info, "started")]), t0);
        merge.end_api();
        merge.begin_run();
        merge.push_pipe(&[info("started")], t0);
        merge.push_api(reset(vec![api_line(LogLevel::Info, "started")]), t0);
        assert_eq!(
            texts(&merge),
            vec!["INFO[0000] started", "INFO[0000] started"]
        );
        assert_eq!(sources(&merge), vec![LogSource::Api, LogSource::Api]);
    }

    /// A crash: the fatal line reaches stderr, the API stream dies with the
    /// process. Held lines show on the Stopped edge; stderr after exit shows
    /// at once, minus what the API had already delivered.
    #[test]
    fn crash_output_survives_the_end_of_the_api_stream() {
        let t0 = Instant::now();
        let mut merge = LogMerge::new(100);
        merge.begin_run();
        merge.push_api(reset(vec![]), t0);
        merge.push_api(append(vec![api_line(LogLevel::Error, "last words")]), t0);
        merge.push_pipe(&["FATAL[0009] start service: boom"], t0);
        assert!(merge.entries().len() == 1);

        assert!(merge.end_api());
        assert!(merge.push_pipe(&["panic: boom", "goroutine 1 [running]:"], t0));
        assert!(
            !merge.push_pipe(&["ERROR[0000] last words"], t0),
            "API had it"
        );
        assert_eq!(
            texts(&merge),
            vec![
                "ERROR[0000] last words",
                "FATAL[0009] start service: boom",
                "panic: boom",
                "goroutine 1 [running]:",
            ]
        );
        assert!(!merge.push_api(append(vec![api_line(LogLevel::Info, "stale")]), t0));
    }

    #[test]
    fn startup_failure_never_reaches_the_api() {
        let t0 = Instant::now();
        let mut merge = LogMerge::new(100);
        merge.begin_run();
        merge.push_pipe(
            &["FATAL[0000] start service: listen tcp: bind: address already in use"],
            t0,
        );
        merge.end_api();
        merge.push_pipe(&["", "  "], t0);
        assert_eq!(merge.entries().len(), 1, "blank lines are skipped");
        assert_eq!(merge.entries()[0].level, LogLevel::Fatal);
    }

    #[test]
    fn eviction_spares_visible_lines_first() {
        let t0 = Instant::now();
        let mut merge = LogMerge::new(4);
        merge.set_keep_level(LogLevel::Info);
        merge.begin_run();
        merge.push_api(
            reset(vec![
                api_line(LogLevel::Info, "i1"),
                api_line(LogLevel::Debug, "d1"),
                api_line(LogLevel::Info, "i2"),
                api_line(LogLevel::Debug, "d2"),
                api_line(LogLevel::Trace, "t1"),
                api_line(LogLevel::Warn, "w1"),
            ]),
            t0,
        );
        assert_eq!(
            texts(&merge),
            vec![
                "INFO[0000] i1",
                "INFO[0000] i2",
                "TRACE[0000] t1",
                "WARN[0000] w1"
            ]
        );
        // Only visible lines left to evict: oldest first.
        merge.push_api(
            append(vec![
                api_line(LogLevel::Error, "e1"),
                api_line(LogLevel::Error, "e2"),
            ]),
            t0,
        );
        assert_eq!(
            texts(&merge),
            vec![
                "INFO[0000] i2",
                "WARN[0000] w1",
                "ERROR[0000] e1",
                "ERROR[0000] e2"
            ]
        );
    }

    #[test]
    fn held_backlog_is_bounded() {
        let t0 = Instant::now();
        let mut merge = LogMerge::new(2);
        merge.begin_run();
        merge.push_api(reset(vec![]), t0);
        let flood: Vec<String> = (0..5).map(|i| info(&format!("l{i}"))).collect();
        assert!(merge.push_pipe(&flood, t0));
        assert!(merge.pending.len() <= 2);
        assert!(merge.entries().len() <= 2);
    }

    #[test]
    fn identical_lines_match_one_to_one() {
        let t0 = Instant::now();
        let mut merge = LogMerge::new(100);
        merge.begin_run();
        merge.push_pipe(&[info("dup"), info("dup")], t0);
        merge.push_api(reset(vec![api_line(LogLevel::Info, "dup")]), t0);
        assert_eq!(sources(&merge), vec![LogSource::Pipe, LogSource::Api]);
    }

    #[test]
    fn visible_filters_by_threshold() {
        let t0 = Instant::now();
        let mut merge = LogMerge::new(100);
        merge.push_pipe(
            &[
                "INFO[0000] a",
                "DEBUG[0000] hidden",
                "panic: x",
                "WARN[0001] b",
            ],
            t0,
        );
        let shown: Vec<&str> = visible(merge.entries(), LogLevel::Info)
            .map(|e| e.text.as_str())
            .collect();
        assert_eq!(shown, ["INFO[0000] a", "panic: x", "WARN[0001] b"]);
        assert_eq!(visible(merge.entries(), LogLevel::Trace).count(), 4);
        // Ids increase in view order.
        let ids: Vec<u64> = merge.entries().iter().map(LogEntry::id).collect();
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn lines_are_stamped_when_they_arrive() {
        let t0 = Instant::now();
        let mut merge = LogMerge::new(100);
        let start = merge.wall_ms(t0);
        merge.push_pipe(&["INFO[0000] a"], t0);
        merge.push_pipe(&["INFO[0000] b"], t0 + Duration::from_millis(1500));
        let stamps: Vec<i64> = merge.entries().iter().map(|e| e.at_ms - start).collect();
        assert_eq!(stamps, [0, 1500]);

        // A held pipe line keeps its read time, not the time it is shown.
        merge.begin_run();
        merge.push_api(append(vec![]), t0);
        merge.push_pipe(&["WARN[0000] stderr only"], t0 + Duration::from_secs(10));
        assert!(merge.tick(t0 + Duration::from_secs(20)));
        let held = merge.entries().back().unwrap();
        assert_eq!(held.text, "WARN[0000] stderr only");
        assert_eq!(held.at_ms - start, 10_000);
    }

    #[test]
    fn parts_split_tag_source_and_message() {
        let parts = LineParts::parse("[3417626869 12ms] inbound/mixed[mixed-in]: from 127.0.0.1:5");
        assert_eq!(parts.tag, Some("[3417626869 12ms]"));
        assert_eq!(parts.source, Some("inbound/mixed[mixed-in]"));
        assert_eq!(parts.message, "from 127.0.0.1:5");

        let parts = LineParts::parse("[42 1ms] outbound/direct[Japan 02]: outbound connection");
        assert_eq!(parts.source, Some("outbound/direct[Japan 02]"));
        assert_eq!(parts.message, "outbound connection");

        let parts = LineParts::parse("router: rule-set loaded");
        assert_eq!((parts.tag, parts.source), (None, Some("router")));
        assert_eq!(parts.message, "rule-set loaded");

        // Prose before a colon is not a source.
        let parts = LineParts::parse("sing-box started (0.00s)");
        assert_eq!((parts.tag, parts.source), (None, None));
        assert_eq!(parts.message, "sing-box started (0.00s)");
        let parts = LineParts::parse("failed to start: bind: address in use");
        assert_eq!(parts.source, None);
        assert_eq!(parts.message, "failed to start: bind: address in use");
        let parts = LineParts::parse("panic: runtime error");
        assert_eq!(parts.source, Some("panic"));
    }

    #[test]
    fn entries_split_after_the_level_word() {
        let t0 = Instant::now();
        let mut merge = LogMerge::new(100);
        merge.push_pipe(
            &[
                "INFO[0012] [42 5ms] router: ok",
                "+0800 2026-10-02 11:23:04 WARN dns: slow",
            ],
            t0,
        );
        let parts: Vec<LineParts> = merge.entries().iter().map(LogEntry::parts).collect();
        assert_eq!(
            parts[0],
            LineParts {
                tag: Some("[42 5ms]"),
                source: Some("router"),
                message: "ok"
            }
        );
        assert_eq!(parts[1].source, Some("dns"));
        assert_eq!(parts[1].message, "slow");
    }

    #[test]
    fn query_includes_and_excludes_terms() {
        let t0 = Instant::now();
        let mut merge = LogMerge::new(100);
        merge.push_pipe(
            &[
                "INFO[0000] dns: exchanged google.com",
                "INFO[0000] dns: healthcheck ok",
                "ERROR[0000] router: Google unreachable",
            ],
            t0,
        );
        let matching = |query: &str| -> Vec<usize> {
            let query = LogQuery::parse(query);
            merge
                .entries()
                .iter()
                .enumerate()
                .filter(|(_, e)| query.matches(e))
                .map(|(ix, _)| ix)
                .collect()
        };
        assert_eq!(matching(""), [0, 1, 2]);
        assert_eq!(matching("GOOGLE"), [0, 2]);
        assert_eq!(matching("dns -healthcheck"), [0]);
        assert_eq!(matching("-dns"), [2]);
        assert_eq!(
            matching("- dns"),
            Vec::<usize>::new(),
            "a lone dash is a term"
        );
        assert!(LogQuery::parse("  ").is_empty());
    }
}
