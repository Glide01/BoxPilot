//! sing-box's stdout and stderr, as lines.
//!
//! The helper reads both pipes continuously, whether or not anyone reads
//! the events: a pipe nobody drains fills up, and sing-box then blocks on
//! its next log write, with TUN up. So a line's length is capped while it
//! is read, not after: sing-box can print a line of any length (a panic, a
//! config dump), and the helper holds at most `max_line` bytes of it.

#![forbid(unsafe_code)]

use std::io::{ErrorKind, Read};

/// Bytes read from the pipe at a time.
const CHUNK: usize = 8 * 1024;

/// Read `reader` to its end, calling `emit(line, truncated)` for each line:
/// without its `\n` or `\r\n`, decoded as UTF-8 with invalid bytes replaced
/// (U+FFFD), and cut at a char boundary to at most `max_line` bytes, which
/// sets `truncated`. A last line without a newline still comes out. A read
/// error other than an interruption ends the reading as the end of the pipe
/// would.
pub fn read_lines<R: Read>(mut reader: R, max_line: usize, mut emit: impl FnMut(String, bool)) {
    let mut chunk = vec![0u8; CHUNK];
    let mut line = Line::new(max_line);
    loop {
        let n = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let mut rest = &chunk[..n];
        while let Some(end) = rest.iter().position(|&b| b == b'\n') {
            line.push(&rest[..end]);
            let (text, truncated) = line.take();
            emit(text, truncated);
            rest = &rest[end + 1..];
        }
        line.push(rest);
    }
    if !line.is_empty() {
        let (text, truncated) = line.take();
        emit(text, truncated);
    }
}

/// The line being read: at most `max + 1` bytes kept (room for a `\r` that
/// a `\n` will turn out to follow), the rest only noted.
struct Line {
    max: usize,
    bytes: Vec<u8>,
    overflowed: bool,
}

impl Line {
    fn new(max: usize) -> Self {
        Self {
            max,
            bytes: Vec::new(),
            overflowed: false,
        }
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty() && !self.overflowed
    }

    fn push(&mut self, data: &[u8]) {
        let room = (self.max + 1).saturating_sub(self.bytes.len());
        if data.len() > room {
            self.overflowed = true;
        }
        self.bytes.extend_from_slice(&data[..data.len().min(room)]);
    }

    fn take(&mut self) -> (String, bool) {
        let mut bytes = std::mem::take(&mut self.bytes);
        let mut truncated = std::mem::take(&mut self.overflowed);
        if !truncated && bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        if text.len() > self.max {
            let mut end = self.max;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            truncated = true;
        }
        (text, truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    fn lines(input: &[u8], max: usize) -> Vec<(String, bool)> {
        let mut out = Vec::new();
        read_lines(input, max, |line, truncated| out.push((line, truncated)));
        out
    }

    fn whole(text: &str) -> (String, bool) {
        (text.to_owned(), false)
    }

    #[test]
    fn lines_lose_their_endings_and_the_last_one_comes_out() {
        assert_eq!(
            lines(b"one\r\ntwo\n\nthree", 64),
            vec![whole("one"), whole("two"), whole(""), whole("three")]
        );
        assert_eq!(lines(b"", 64), vec![]);
        assert_eq!(lines(b"\n", 64), vec![whole("")]);
        assert_eq!(lines(b"a\r", 64), vec![whole("a")]);
        assert_eq!(lines(b"a\rb\n", 64), vec![whole("a\rb")]);
    }

    #[test]
    fn invalid_utf8_is_replaced_not_fatal() {
        assert_eq!(
            lines(b"bad \xff byte\n\xe4\xb8\xad\n", 64),
            vec![whole("bad \u{fffd} byte"), whole("中")]
        );
    }

    #[test]
    fn a_long_line_is_cut_while_reading_and_the_next_is_whole() {
        let mut input = vec![b'x'; 100_000];
        input.extend_from_slice(b"\nnext\n");
        assert_eq!(
            lines(&input, 10),
            vec![("x".repeat(10), true), whole("next")]
        );
        // Exactly the limit, with or without `\r`, is not truncated.
        assert_eq!(lines(b"0123456789\r\n", 10), vec![whole("0123456789")]);
        assert_eq!(lines(b"0123456789", 10), vec![whole("0123456789")]);
        assert_eq!(
            lines(b"0123456789a\n", 10),
            vec![("0123456789".into(), true)]
        );
        // A long last line without a newline.
        assert_eq!(lines(&[b'y'; 50], 10), vec![("y".repeat(10), true)]);
    }

    #[test]
    fn a_cut_never_splits_a_character() {
        // 中 is three bytes: 4 bytes of budget keep one of them.
        assert_eq!(lines("中中中\n".as_bytes(), 4), vec![("中".into(), true)]);
        // Replacement characters are three bytes too, so invalid bytes up to
        // the limit can still need a cut after decoding.
        assert_eq!(lines(b"\xff\xff\xff\n", 4), vec![("\u{fffd}".into(), true)]);
    }

    /// A reader that hands out one byte at a time, then fails.
    struct Trickle<'a>(&'a [u8]);

    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.0.split_first() {
                Some((&b, rest)) => {
                    buf[0] = b;
                    self.0 = rest;
                    Ok(1)
                }
                None => Err(io::Error::new(io::ErrorKind::BrokenPipe, "gone")),
            }
        }
    }

    #[test]
    fn chunk_boundaries_and_errors_change_nothing() {
        let mut out = Vec::new();
        read_lines(Trickle(b"ab\r\ncd\nlong line"), 6, |line, truncated| {
            out.push((line, truncated))
        });
        assert_eq!(out, vec![whole("ab"), whole("cd"), ("long l".into(), true)]);
    }
}
