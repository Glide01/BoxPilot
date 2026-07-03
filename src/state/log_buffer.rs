use crate::core::settings::{matches_filter, LogEntry, LogFilter, MAX_LOG_LINES};
use gpui::Context;
use std::collections::VecDeque;

/// Mutated only via the public methods on this type — every mutator emits
/// `cx.notify()` so observing views re-render.
pub struct LogBuffer {
    pub entries: VecDeque<LogEntry>,
    pub filter: LogFilter,
}

impl LogBuffer {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            filter: LogFilter::All,
        }
    }

    /// Append a batch from the log-drain task. Bulk-pushes all entries, evicts
    /// the oldest if the buffer overflows `MAX_LOG_LINES`, then a single
    /// `notify` — absorbs verbose sing-box log floods without a render per line.
    pub fn extend(&mut self, batch: Vec<LogEntry>, cx: &mut Context<Self>) {
        if batch.is_empty() {
            return;
        }
        for entry in batch {
            self.entries.push_back(entry);
        }
        while self.entries.len() > MAX_LOG_LINES {
            self.entries.pop_front();
        }
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        if self.entries.is_empty() {
            return;
        }
        self.entries.clear();
        cx.notify();
    }

    pub fn set_filter(&mut self, filter: LogFilter, cx: &mut Context<Self>) {
        if self.filter != filter {
            self.filter = filter;
            cx.notify();
        }
    }

    pub fn visible_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| matches_filter(e.level, self.filter))
            .count()
    }
}
