//! `alx check --json`: run `analyze::analyze` and print its diagnostics as one JSON
//! object (docs/notes/lsp-plan.md, 1.1/1.2/1.6). No JSON crate: the output is
//! built by hand and the overlay map is read by a small parser.
//!
//! Position rules:
//! - `file` is the real absolute path of the file the span is in.
//! - A span in a file with no real path (`<builtin>`, the generated test
//!   runner, the embedded std, a load failure on a directory) is reported on the
//!   focused file at 0:0, and its message is prefixed `name:line:col: ` when the
//!   span's file has text (so the origin is not lost).
//! - `line` is 0-based, `col` and `end_col` are byte offsets in their line.

use crate::analyze::{self, Force, Item, Report};
use crate::diag::Span;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

/// Absolute and lexically normalised (no `.`, `..` folded), without touching the disk.
pub fn norm(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().unwrap_or_default().join(p) };
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// `{absolute path: text}`; keys are normalised.
pub fn parse_overlays(src: &str) -> Result<HashMap<PathBuf, String>, String> {
    let mut p = Json { s: src.as_bytes(), i: 0 };
    p.ws();
    p.expect(b'{')?;
    let mut m = HashMap::new();
    p.ws();
    if p.peek() == Some(b'}') {
        p.i += 1;
    } else {
        loop {
            p.ws();
            let k = p.string()?;
            p.ws();
            p.expect(b':')?;
            p.ws();
            let v = p.string()?;
            m.insert(norm(Path::new(&k)), v);
            p.ws();
            match p.next() {
                Some(b',') => {}
                Some(b'}') => break,
                _ => return Err(format!("overlays: expected `,` or `}}` at byte {}", p.i)),
            }
        }
    }
    p.ws();
    if p.i != p.s.len() {
        return Err(format!("overlays: trailing data at byte {}", p.i));
    }
    Ok(m)
}

struct Json<'a> {
    s: &'a [u8],
    i: usize,
}

impl Json<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }
    fn next(&mut self) -> Option<u8> {
        let c = self.peek();
        self.i += 1;
        c
    }
    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }
    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.next() == Some(c) {
            Ok(())
        } else {
            Err(format!("overlays: expected `{}` at byte {}", c as char, self.i.saturating_sub(1)))
        }
    }
    fn hex4(&mut self) -> Result<u32, String> {
        let h = self.s.get(self.i..self.i + 4).and_then(|b| std::str::from_utf8(b).ok()).and_then(|h| u32::from_str_radix(h, 16).ok());
        self.i += 4;
        h.ok_or_else(|| format!("overlays: bad \\u escape at byte {}", self.i - 4))
    }
    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out: Vec<u8> = vec![];
        loop {
            match self.next() {
                None => return Err("overlays: unterminated string".into()),
                Some(b'"') => break,
                Some(b'\\') => {
                    let c = match self.next() {
                        Some(b'"') => '"',
                        Some(b'\\') => '\\',
                        Some(b'/') => '/',
                        Some(b'b') => '\u{8}',
                        Some(b'f') => '\u{c}',
                        Some(b'n') => '\n',
                        Some(b'r') => '\r',
                        Some(b't') => '\t',
                        Some(b'u') => {
                            let mut u = self.hex4()?;
                            if (0xD800..0xDC00).contains(&u) && self.s.get(self.i..self.i + 2) == Some(b"\\u") {
                                self.i += 2;
                                let lo = self.hex4()?;
                                if (0xDC00..0xE000).contains(&lo) {
                                    u = 0x10000 + ((u - 0xD800) << 10) + (lo - 0xDC00);
                                } else {
                                    u = 0xFFFD;
                                }
                            }
                            char::from_u32(u).unwrap_or('\u{FFFD}')
                        }
                        _ => return Err(format!("overlays: bad escape at byte {}", self.i - 1)),
                    };
                    let mut b = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
                }
                Some(c) => out.push(c),
            }
        }
        String::from_utf8(out).map_err(|_| "overlays: invalid UTF-8".to_string())
    }
}

/// A JSON string literal (with quotes).
pub fn quote(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().to_string()
}

/// The report as the plan's JSON object (no trailing newline). `focused` is the checked file.
pub fn render(r: &Report, focused: &Path) -> String {
    let mut diags: Vec<String> = vec![];
    for it in &r.items {
        diags.push(item_json(r, it, focused));
    }
    format!("{{\"unit\":{},\"root\":{},\"diagnostics\":[{}]}}", quote(r.unit), quote(&path_str(&norm(&r.root))), diags.join(","))
}

fn item_json(r: &Report, it: &Item, focused: &Path) -> String {
    let Span { file, lo, hi } = it.diag.span;
    let f = r.sm.files.get(file as usize);
    let mut msg = it.diag.msg.clone();
    let (path, lo, hi, line, col, eline, ecol) = match f.and_then(|f| f.path.as_ref().map(|p| (f, p))) {
        Some((f, p)) => {
            let (l, c) = f.line_byte_col(lo);
            let (el, ec) = f.line_byte_col(hi.max(lo));
            let n = f.text.len() as u32;
            (norm(p), lo.min(n), hi.max(lo).min(n), l, c, el, ec)
        }
        None => {
            if let Some(f) = f.filter(|f| !f.text.is_empty()) {
                let (l, c) = f.line_col(lo.min(f.text.len() as u32));
                msg = format!("{}:{l}:{c}: {msg}", f.name);
            }
            (norm(focused), 0, 0, 0, 0, 0, 0)
        }
    };
    let sev = if it.error { "error" } else { "warning" };
    let notes: Vec<String> = it.diag.notes.iter().map(|n| quote(n)).collect();
    format!(
        "{{\"file\":{},\"phase\":{},\"severity\":{},\"lo\":{lo},\"hi\":{hi},\"line\":{line},\"col\":{col},\"end_line\":{eline},\"end_col\":{ecol},\"message\":{},\"notes\":[{}]}}",
        quote(&path_str(&path)),
        quote(it.phase.name()),
        quote(sev),
        quote(&msg),
        notes.join(",")
    )
}

/// A single `internal` diagnostic on the focused file.
fn internal(focused: &Path, msg: &str) -> String {
    let f = path_str(&norm(focused));
    format!(
        "{{\"unit\":\"script\",\"root\":{},\"diagnostics\":[{{\"file\":{},\"phase\":\"internal\",\"severity\":\"error\",\"lo\":0,\"hi\":0,\"line\":0,\"col\":0,\"end_line\":0,\"end_col\":0,\"message\":{},\"notes\":[]}}]}}",
        quote(&f),
        quote(&f),
        quote(msg)
    )
}

/// Run `body` and turn a panic into the `internal` JSON. Returns (json, exit code).
pub fn guarded(focused: &Path, body: impl FnOnce() -> String) -> (String, i32) {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(j) => (j, 0),
        Err(e) => {
            let m = e.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| e.downcast_ref::<String>().cloned()).unwrap_or_else(|| "panic".into());
            (internal(focused, &format!("internal compiler error: {m}")), 3)
        }
    }
}

/// Check `file` with `overlays` shadowing disk files; the JSON and the exit code.
pub fn check_json(file: &Path, overlays: &HashMap<PathBuf, String>, force: Force) -> (String, i32) {
    let file = norm(file);
    guarded(&file, || {
        // An overlay shadows a file that exists on disk; it never adds one,
        // except the focused file itself (an editor's new, unsaved buffer).
        let read = |p: &Path| -> std::io::Result<String> {
            if let Some(t) = overlays.get(&norm(p)) {
                if p.is_file() || norm(p) == file {
                    return Ok(t.clone());
                }
            }
            std::fs::read_to_string(p)
        };
        let list = |p: &Path| -> std::io::Result<Vec<PathBuf>> {
            let mut v: Vec<PathBuf> = std::fs::read_dir(p)?.filter_map(|e| e.ok().map(|e| e.path())).collect();
            v.sort();
            Ok(v)
        };
        let r = analyze::analyze(&file, force, &read, &list);
        render(&r, &file)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_parse() {
        let m = parse_overlays(r#" {"/a/b.alx": "x = \"q\"\né😀", "/c/../d": ""} "#).unwrap();
        assert_eq!(m[Path::new("/a/b.alx")], "x = \"q\"\n\u{e9}\u{1F600}");
        assert_eq!(m[Path::new("/d")], "");
        assert!(parse_overlays("{}").unwrap().is_empty());
        assert!(parse_overlays("{\"a\": 1}").is_err());
        assert!(parse_overlays("{\"a\": \"b\"} x").is_err());
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("a\"b\\c\n\u{1}é"), "\"a\\\"b\\\\c\\n\\u0001é\"");
    }

    #[test]
    fn panic_becomes_internal() {
        let (j, code) = guarded(Path::new("/x/f.alx"), || panic!("boom {}", 1));
        assert_eq!(code, 3);
        assert!(j.contains("\"phase\":\"internal\"") && j.contains("boom 1") && j.contains("/x/f.alx"));
    }
}
