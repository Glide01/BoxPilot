//! sing-box's relaxed JSON: what its config parser accepts beyond plain JSON
//! (`sing/common/json/internal/contextjson/scanner.go`). Comments — `//` and
//! `#` to the end of the line, `/* … */` — wherever whitespace may go, and a
//! trailing comma before `}` or `]`. A config sing-box runs must import, so
//! BoxPilot reads configs the same way; the canonical copy it saves is plain
//! JSON (the comments don't survive `strip_inbounds`, nor does formatting).

use serde_json::Value;

/// Parse a config as sing-box would: `to_plain_json`, then `serde_json`.
/// Error positions are the input's, as `to_plain_json` keeps every byte's.
pub fn parse(text: &str) -> serde_json::Result<Value> {
    serde_json::from_str(&to_plain_json(text))
}

/// `text` with every comment and trailing comma blanked to spaces (a
/// comment's newlines kept), so each byte stays where it was and a parse
/// error still points into the original. Strings are left alone, `//` and `#`
/// in URLs included. A comma is trailing only after a value (`[,]` stays, and
/// stays an error, as in sing-box). Anything else malformed is passed through
/// for `serde_json` to report.
pub fn to_plain_json(text: &str) -> String {
    let mut out = text.as_bytes().to_vec();
    let mut i = 0;
    let mut in_string = false;
    // The last non-blank byte outside a comment, and the index of a comma
    // that may yet turn out to be trailing.
    let mut last = 0u8;
    let mut pending_comma: Option<usize> = None;
    while i < out.len() {
        let c = out[i];
        if in_string {
            match c {
                b'\\' => i += 1,
                b'"' => in_string = false,
                _ => {}
            }
            i += 1;
            continue;
        }
        let comment_end = match (c, out.get(i + 1)) {
            (b'#', _) | (b'/', Some(b'/')) => Some(line_end(&out, i)),
            (b'/', Some(b'*')) => block_end(&out, i),
            _ => None,
        };
        if let Some(end) = comment_end {
            blank(&mut out[i..end]);
            i = end;
            continue;
        }
        if !c.is_ascii_whitespace() {
            if let Some(comma) = pending_comma.take() {
                if c == b'}' || c == b']' {
                    out[comma] = b' ';
                }
            }
            match c {
                b'"' => in_string = true,
                b',' if !matches!(last, b'[' | b'{' | b',' | b':') => pending_comma = Some(i),
                _ => {}
            }
            last = c;
        }
        i += 1;
    }
    // Only ASCII was replaced, and only by ASCII; a comment is blanked whole,
    // its multi-byte characters with it.
    String::from_utf8(out).expect("blanking keeps UTF-8 valid")
}

/// The index of the newline ending the line comment at `start` (or the end).
fn line_end(bytes: &[u8], start: usize) -> usize {
    bytes[start..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(bytes.len(), |n| start + n)
}

/// The index just past the `*/` closing the block comment at `start`. `None`
/// for an unclosed one, which is left as it is, for `serde_json` to reject.
fn block_end(bytes: &[u8], start: usize) -> Option<usize> {
    bytes[start + 2..]
        .windows(2)
        .position(|w| w == b"*/")
        .map(|n| start + 2 + n + 2)
}

/// Spaces in place of `bytes`, newlines kept.
fn blank(bytes: &mut [u8]) {
    for b in bytes {
        if *b != b'\n' && *b != b'\r' {
            *b = b' ';
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plain_json_passes_unchanged() {
        let text = r#"{"a": [1, 2, {"b": "c"}], "d": null}"#;
        assert_eq!(to_plain_json(text), text);
    }

    #[test]
    fn comments_of_every_kind_go() {
        let text = r#"
            // sing-box config
            {
              # the log
              "log": {"level": "info"}, /* inline */
              "outbounds": [
                /* a
                   multi-line
                   comment */
                {"type": "direct", "tag": "direct"} // trailing note
              ]
            }
            # done
        "#;
        assert_eq!(
            parse(text).unwrap(),
            json!({"log": {"level": "info"}, "outbounds": [{"type": "direct", "tag": "direct"}]})
        );
    }

    #[test]
    fn comment_markers_inside_strings_stay() {
        let text =
            r#"{"url": "https://a.example/sub#frag", "p": "/* x */", "q": "say \"// hi\" # ok"}"#;
        assert_eq!(
            parse(text).unwrap(),
            json!({"url": "https://a.example/sub#frag", "p": "/* x */", "q": "say \"// hi\" # ok"})
        );
    }

    #[test]
    fn trailing_commas_go() {
        let text = "{\"a\": [1, 2, ], \"b\": {\"c\": 3, /* last */ }, }";
        assert_eq!(parse(text).unwrap(), json!({"a": [1, 2], "b": {"c": 3}}));
    }

    /// As in sing-box: a comma only ends a value, it never stands alone.
    #[test]
    fn stray_commas_stay_errors() {
        for text in ["[,]", "{,}", "[1,,]", "{\"a\":,}"] {
            assert!(parse(text).is_err(), "{}", text);
        }
    }

    #[test]
    fn errors_point_into_the_original() {
        let text = "{\n  // a comment with 中文\n  \"a\": 1,\n  \"b\": oops\n}";
        let e = parse(text).unwrap_err();
        assert_eq!(e.line(), 4);
    }

    #[test]
    fn unclosed_block_comment_is_an_error() {
        assert!(parse("{\"a\": 1} /* open").is_err());
    }

    #[test]
    fn multibyte_text_in_comments_keeps_utf8_valid() {
        let text = "{\"名\": \"值\" /* 注释 ✓ */, # 说明\n\"b\": 2}";
        assert_eq!(parse(text).unwrap(), json!({"名": "值", "b": 2}));
    }
}
