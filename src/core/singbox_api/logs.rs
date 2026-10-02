//! sing-box's own log over the API: `SubscribeLog`, `GetDefaultLogLevel`,
//! `ClearLogs`.

use super::transport::{ApiError, IDLE_STREAM_READ_TIMEOUT};
use super::{pb, SingBoxApi};
use std::borrow::Cow;

impl SingBoxApi {
    /// Stream `SubscribeLog`. The first batch has `reset` set and carries the
    /// buffered history (a ring of the last 3000 lines since the `api`
    /// service started — lines logged during sing-box's own startup, before
    /// the service is up, are not in it). After that every batch appends
    /// whatever was logged since the previous one; a batch with `reset` set
    /// means `clear_logs` ran: drop what you hold, then append its lines.
    ///
    /// Every level reaches the stream regardless of `log.level` in the
    /// config — filter with `get_default_log_level` if the UI should honour
    /// it. Messages are preformatted like the console (`INFO[0012] ` prefix,
    /// connection id, ANSI colours); see `strip_ansi`.
    ///
    /// Idle when nothing is logged: `TimedOut` after
    /// `IDLE_STREAM_READ_TIMEOUT`; re-subscribe (the new stream opens with a
    /// fresh `reset` snapshot).
    pub fn stream_logs(&self, mut on_batch: impl FnMut(LogBatch) -> bool) -> Result<(), ApiError> {
        self.stream(
            "SubscribeLog",
            &(),
            IDLE_STREAM_READ_TIMEOUT,
            |log: pb::Log| on_batch(LogBatch::from_proto(log)),
        )
    }

    /// `GetDefaultLogLevel` — the configured `log.level` (`Trace` when the
    /// config sets none).
    pub fn get_default_log_level(&self) -> Result<LogLevel, ApiError> {
        self.unary("GetDefaultLogLevel", &())
            .map(|level: pb::DefaultLogLevel| LogLevel::from_proto(level.level))
    }

    /// `ClearLogs` — empty sing-box's log buffer. Every open `stream_logs`
    /// then receives a batch with `reset` set.
    pub fn clear_logs(&self) -> Result<(), ApiError> {
        self.unary("ClearLogs", &())
    }
}

/// sing-box log level, most severe first, so `line.level <= threshold`
/// selects what a threshold shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LogLevel {
    Panic,
    Fatal,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn from_proto(value: i32) -> Self {
        match value {
            i32::MIN..=0 => LogLevel::Panic,
            1 => LogLevel::Fatal,
            2 => LogLevel::Error,
            3 => LogLevel::Warn,
            4 => LogLevel::Info,
            5 => LogLevel::Debug,
            _ => LogLevel::Trace,
        }
    }

    /// The config spelling (`log.level`): `panic` … `trace`.
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Panic => "panic",
            LogLevel::Fatal => "fatal",
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }
}

/// One log line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLine {
    pub level: LogLevel,
    /// As sing-box formatted it, ANSI colour codes included.
    pub message: String,
}

/// One `SubscribeLog` message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogBatch {
    /// Drop every line held so far before appending `lines`.
    pub reset: bool,
    /// Oldest first.
    pub lines: Vec<LogLine>,
}

impl LogBatch {
    fn from_proto(log: pb::Log) -> Self {
        Self {
            reset: log.reset,
            lines: log
                .messages
                .into_iter()
                .map(|message| LogLine {
                    level: LogLevel::from_proto(message.level),
                    message: message.message,
                })
                .collect(),
        }
    }
}

/// Remove ANSI escape sequences (CSI `ESC [ … final`, and any other `ESC x`
/// pair) from a log message. Borrows when there is nothing to strip.
pub fn strip_ansi(text: &str) -> Cow<'_, str> {
    if !text.contains('\x1b') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        // CSI: parameters and intermediates up to a final byte @..~. Any
        // other two-character escape: drop both.
        if chars.next() == Some('[') {
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_levels_map_upstream_values_in_severity_order() {
        let levels: Vec<LogLevel> = (0..=6).map(LogLevel::from_proto).collect();
        assert_eq!(
            levels,
            vec![
                LogLevel::Panic,
                LogLevel::Fatal,
                LogLevel::Error,
                LogLevel::Warn,
                LogLevel::Info,
                LogLevel::Debug,
                LogLevel::Trace,
            ]
        );
        assert!(LogLevel::Error < LogLevel::Warn, "more severe sorts first");
        assert_eq!(LogLevel::from_proto(9), LogLevel::Trace);
        assert_eq!(LogLevel::from_proto(-1), LogLevel::Panic);
        assert_eq!(LogLevel::Warn.as_str(), "warn");
    }

    #[test]
    fn log_batch_keeps_reset_and_order() {
        let batch = LogBatch::from_proto(pb::Log {
            messages: vec![
                pb::LogMessage {
                    level: 3,
                    message: "first".into(),
                },
                pb::LogMessage {
                    level: 5,
                    message: "second".into(),
                },
            ],
            reset: true,
        });
        assert!(batch.reset);
        assert_eq!(
            batch.lines,
            vec![
                LogLine {
                    level: LogLevel::Warn,
                    message: "first".into()
                },
                LogLine {
                    level: LogLevel::Debug,
                    message: "second".into()
                },
            ]
        );
        assert_eq!(
            LogBatch::from_proto(pb::Log::default()),
            LogBatch::default()
        );
    }

    #[test]
    fn strip_ansi_removes_colour_codes_only() {
        assert_eq!(
            strip_ansi("\x1b[36mINFO\x1b[0m[0003] [\x1b[38;5;123m42\x1b[0m 5ms] router: ok"),
            "INFO[0003] [42 5ms] router: ok"
        );
        assert!(matches!(
            strip_ansi("plain 节点"),
            Cow::Borrowed("plain 节点")
        ));
        assert_eq!(strip_ansi("a\x1b"), "a", "dangling escape dropped");
        assert_eq!(strip_ansi("\x1b[0"), "", "unterminated CSI dropped");
    }
}
