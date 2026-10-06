//! Tokens for the braces-only Alexandrite syntax.

use crate::diag::{Diag, Span};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Int(i64),
    /// An integer literal too large for i64, in decimal.
    BigInt(String),
    /// Value and source text.
    Float(f64, String),
    /// An imaginary literal (`2i`, `1.5e3i`): the value and text of its
    /// coefficient (Go's imaginary literals; decimal forms only).
    Imag(f64, String),
    /// A double-quoted string with `#{...}` parts.
    Interp(Vec<IPiece>),
    /// A command literal: `` `cat #{f} | grep -c x` ``.
    Cmd(Vec<CmdPart>),
    Str(String),
    Ident(String),
    Const(String),
    /// `:name`, `:*`, `:even?`
    Sym(String),
    /// `#[pure]` and friends: the attribute name.
    Attr(String),
    /// `#![overflow(promote)]`: (name, argument).
    Directive(String, String),
    Kw(Kw),
    Op(&'static str),
    Newline,
    Eof,
}

/// A piece of an interpolated string: literal text, or the source of an
/// embedded expression and its byte offset in the file.
#[derive(Debug, Clone, PartialEq)]
pub enum IPiece {
    Lit(String),
    Code(String, u32),
}

/// A piece of a command literal: a word (literal text and `#{}` pieces, one
/// argument), `#{*xs}` (a [Str] spliced as several arguments), or an operator
/// (the `os/exec` literal kinds: 1 `|`, 2 `<`, 3 `>`, 4 `>>`, 5 `2>`, 6 `2>>`,
/// 7 `2>&1`, 8 `>&2`, 9 `&>`). Each piece has its span.
#[derive(Debug, Clone, PartialEq)]
pub enum CmdPart {
    Word(Vec<IPiece>, Span),
    Splice(String, u32, Span),
    Op(i64, Span),
    /// Something the literal doesn't support: reported by the parser (so
    /// `alx fmt` still reads the file). Message, note, span.
    Bad(String, String, Span),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kw {
    Def,
    If,
    Unless,
    Else,
    Elsif,
    While,
    Next,
    Break,
    Return,
    True,
    False,
    Nil,
    Try,
    Require,
    Struct,
    Enum,
    Interface,
    /// `fn` and `ƒ`: a pure `def`
    Fn,
    For,
    In,
    Case,
    Defer,
    /// `fail e`: return an error (a jump, so a hard keyword like `return`)
    Fail,
    /// `spawn { ... }` / `spawn f(x)`: start a task
    Spawn,
    /// `none`: the absent value of a `T?`
    None,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
    /// Whitespace immediately before this token (decides `f (x)` vs `f(x)`).
    pub space_before: bool,
}

/// Longest first: the lexer takes the first match.
const OPS: [&str; 54] = [
    "&^=", "+%=", "-%=", "*%=", "<<=", ">>=", "**=", "...", "<=>", "**", "==", "=>", "!=", "<=", ">=", "&&", "||", "&^", "<<", ">>", "..", "?.",
    "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "+%", "-%", "*%", "->", "&:", "+", "-", "*", "/", "%", "<", ">", "=", "!", "?",
    ":", ".", ",", ";", "|", "&", "^", "~", "@",
];
const BRACKETS: [&str; 6] = ["(", ")", "[", "]", "{", "}"];

pub fn lex(file: u32, src: &str) -> Result<Vec<Token>, Diag> {
    lex_at(file, src, 0)
}

/// Lex `src`, which starts at byte `base` of the file (interpolated code).
pub fn lex_at(file: u32, src: &str, base: u32) -> Result<Vec<Token>, Diag> {
    let b = src.as_bytes();
    let mut i = 0usize;
    let mut out: Vec<Token> = Vec::new();
    // Heredocs whose bodies start on the next line: (index in `out`, terminator, raw).
    let mut pending: Vec<(usize, String, bool)> = Vec::new();
    let mut space = false;
    let sp = |lo: usize, hi: usize| Span { file, lo: base + lo as u32, hi: base + hi as u32 };
    while i < b.len() {
        let c = b[i];
        if c == b' ' || c == b'\t' || c == b'\r' {
            i += 1;
            space = true;
            continue;
        }
        if c == b'\n' {
            out.push(Token { tok: Tok::Newline, span: sp(i, i + 1), space_before: space });
            i += 1;
            space = false;
            // Heredoc bodies follow the line that introduced them.
            for (idx, term, raw) in std::mem::take(&mut pending) {
                let body_lo = i;
                let mut lines = Vec::new();
                loop {
                    if i >= b.len() {
                        return Err(Diag::new(out[idx].span, format!("unterminated heredoc `{term}`")));
                    }
                    let end = src[i..].find('\n').map_or(b.len(), |k| i + k);
                    let line = &src[i..end];
                    if line.trim() == term {
                        break;
                    }
                    i = (end + 1).min(b.len());
                    lines.push(line);
                }
                // The body is [body_lo, i); the terminator line follows.
                let body_hi = i;
                i = src[i..].find('\n').map_or(b.len(), |k| i + k + 1);
                // Squiggly: the common indent of the non-blank lines, measured
                // on the source text (before escapes), as Ruby does.
                let indent = lines.iter().filter(|l| !l.trim().is_empty()).map(|l| l.len() - l.trim_start().len()).min().unwrap_or(0);
                out[idx].tok = heredoc_body(src, body_lo, body_hi, indent, raw, sp)?;
            }
            continue;
        }
        let start = i;
        // Attributes, directives, comments.
        if c == b'#' {
            if src[i..].starts_with("#![") {
                let end = src[i..].find(']').map(|k| i + k).ok_or_else(|| Diag::new(sp(i, i + 3), "unterminated `#![`"))?;
                let inner = &src[i + 3..end];
                let (name, arg) = match inner.find('(') {
                    Some(p) => {
                        let a = inner[p + 1..].trim_end();
                        (inner[..p].trim().to_string(), a.strip_suffix(')').unwrap_or(a).trim().to_string())
                    }
                    None => (inner.trim().to_string(), String::new()),
                };
                out.push(Token { tok: Tok::Directive(name, arg), span: sp(i, end + 1), space_before: space });
                i = end + 1;
                space = false;
                continue;
            }
            if src[i..].starts_with("#[") {
                // The closing `]`, skipping nested brackets and "strings" (`#[json("a]b")]`).
                let end = {
                    let (mut depth, mut j, mut in_str, mut found) = (0usize, i + 2, false, None);
                    while j < b.len() && b[j] != b'\n' {
                        match (in_str, b[j]) {
                            (true, b'\\') => j += 1,
                            (true, b'"') => in_str = false,
                            (true, _) => {}
                            (false, b'"') => in_str = true,
                            (false, b'[') => depth += 1,
                            (false, b']') if depth == 0 => {
                                found = Some(j);
                                break;
                            }
                            (false, b']') => depth -= 1,
                            _ => {}
                        }
                        j += 1;
                    }
                    found.ok_or_else(|| Diag::new(sp(i, i + 2), "unterminated `#[`"))?
                };
                out.push(Token { tok: Tok::Attr(src[i + 2..end].trim().to_string()), span: sp(i, end + 1), space_before: space });
                i = end + 1;
                space = false;
                continue;
            }
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c.is_ascii_digit() {
            // 0x / 0o / 0b integers.
            if c == b'0' && i + 2 < b.len() && matches!(b[i + 1], b'x' | b'X' | b'o' | b'O' | b'b' | b'B') {
                let radix = match b[i + 1] {
                    b'x' | b'X' => 16,
                    b'o' | b'O' => 8,
                    _ => 2,
                };
                let mut j = i + 2;
                let mut digits = String::new();
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                    if b[j] != b'_' {
                        digits.push(b[j] as char);
                    }
                    j += 1;
                }
                let v = num_bigint::BigInt::parse_bytes(digits.as_bytes(), radix).ok_or_else(|| Diag::new(sp(start, j), format!("malformed base-{radix} literal")))?;
                i = j;
                let tok = match i64::try_from(&v) {
                    Ok(x) => Tok::Int(x),
                    Err(_) => Tok::BigInt(v.to_string()),
                };
                out.push(Token { tok, span: sp(start, i), space_before: space });
                space = false;
                continue;
            }
            let mut s = String::new();
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
                if b[i] != b'_' {
                    s.push(b[i] as char);
                }
                i += 1;
            }
            // A Float: digits `.` digits (so `1..10` and `1.to_s` stay Int), and/or an exponent.
            let mut float = false;
            if i + 1 < b.len() && b[i] == b'.' && b[i + 1].is_ascii_digit() {
                float = true;
                s.push('.');
                i += 1;
                while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
                    if b[i] != b'_' {
                        s.push(b[i] as char);
                    }
                    i += 1;
                }
            }
            if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
                let mut j = i + 1;
                if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
                    j += 1;
                }
                if j < b.len() && b[j].is_ascii_digit() {
                    float = true;
                    s.push_str(&src[i..j]);
                    i = j;
                    while i < b.len() && b[i].is_ascii_digit() {
                        s.push(b[i] as char);
                        i += 1;
                    }
                }
            }
            // `2i`, `1.5i`: an imaginary literal (the `i` ends the word).
            if i < b.len() && b[i] == b'i' && !(i + 1 < b.len() && (b[i + 1].is_ascii_alphanumeric() || b[i + 1] == b'_' || b[i + 1] == b'?' || b[i + 1] == b'!')) {
                let v: f64 = s.parse().map_err(|_| Diag::new(sp(start, i), "malformed imaginary literal"))?;
                i += 1;
                out.push(Token { tok: Tok::Imag(v, s), span: sp(start, i), space_before: space });
                space = false;
                continue;
            }
            if float {
                let v: f64 = s.parse().map_err(|_| Diag::new(sp(start, i), "malformed Float literal"))?;
                out.push(Token { tok: Tok::Float(v, s), span: sp(start, i), space_before: space });
                space = false;
                continue;
            }
            let tok = match s.parse::<i64>() {
                Ok(v) => Tok::Int(v),
                Err(_) => Tok::BigInt(s),
            };
            out.push(Token { tok, span: sp(start, i), space_before: space });
            space = false;
            continue;
        }
        if src[i..].starts_with('ƒ') {
            out.push(Token { tok: Tok::Kw(Kw::Fn), span: sp(i, i + 'ƒ'.len_utf8()), space_before: space });
            i += 'ƒ'.len_utf8();
            space = false;
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            // `even?`, `strip!`: a trailing ? or ! belongs to the name unless it
            // starts `!=` / `?:`-style punctuation.
            // `x?.f` is optional chaining, so `?.` is never part of a name.
            if i < b.len() && !b[start].is_ascii_uppercase() && (b[i] == b'?' || b[i] == b'!') && b.get(i + 1) != Some(&b'=') {
                let next = b.get(i + 1).copied().unwrap_or(b' ');
                if b[i] == b'!' || (next != b':' && next != b'.') {
                    i += 1;
                }
            }
            let word = &src[start..i];
            let tok = match word {
                "def" => Tok::Kw(Kw::Def),
                "if" => Tok::Kw(Kw::If),
                "unless" => Tok::Kw(Kw::Unless),
                "else" => Tok::Kw(Kw::Else),
                "elsif" => Tok::Kw(Kw::Elsif),
                "while" => Tok::Kw(Kw::While),
                "next" => Tok::Kw(Kw::Next),
                "break" => Tok::Kw(Kw::Break),
                "return" => Tok::Kw(Kw::Return),
                "true" => Tok::Kw(Kw::True),
                "false" => Tok::Kw(Kw::False),
                "nil" => Tok::Kw(Kw::Nil),
                "try" => Tok::Kw(Kw::Try),
                "require" => Tok::Kw(Kw::Require),
                "struct" => Tok::Kw(Kw::Struct),
                "enum" => Tok::Kw(Kw::Enum),
                "interface" => Tok::Kw(Kw::Interface),
                "fn" => Tok::Kw(Kw::Fn),
                "for" => Tok::Kw(Kw::For),
                "in" => Tok::Kw(Kw::In),
                "case" => Tok::Kw(Kw::Case),
                "defer" => Tok::Kw(Kw::Defer),
                "fail" => Tok::Kw(Kw::Fail),
                "spawn" => Tok::Kw(Kw::Spawn),
                "none" => Tok::Kw(Kw::None),
                w if w.as_bytes()[0].is_ascii_uppercase() => Tok::Const(w.to_string()),
                w => Tok::Ident(w.to_string()),
            };
            out.push(Token { tok, span: sp(start, i), space_before: space });
            space = false;
            continue;
        }
        if c == b'`' {
            let (tok, end) = lex_cmd(file, src, base, i)?;
            out.push(Token { tok, span: sp(i, end), space_before: space });
            i = end;
            space = false;
            continue;
        }
        if c == b'"' || c == b'\'' {
            let q = c;
            i += 1;
            let mut s = String::new();
            let mut pieces: Vec<IPiece> = Vec::new();
            loop {
                if i >= b.len() || b[i] == b'\n' {
                    return Err(Diag::new(sp(start, i), "unterminated string"));
                }
                if b[i] == q {
                    i += 1;
                    break;
                }
                if b[i] == b'\\' && q == b'"' && i + 1 < b.len() {
                    let (ch, len) = escape(src, i, sp)?;
                    s.push(ch);
                    i += len;
                    continue;
                }
                if q == b'"' && b[i] == b'#' && b.get(i + 1) == Some(&b'{') {
                    // `#{ expr }`: find the matching brace (strings and comments inside count).
                    let open = i + 2;
                    let Some(j) = interp_end(b, open) else {
                        return Err(Diag::new(sp(i, i + 2), "unterminated `#{` in string"));
                    };
                    if !s.is_empty() {
                        pieces.push(IPiece::Lit(std::mem::take(&mut s)));
                    }
                    pieces.push(IPiece::Code(src[open..j].to_string(), base + open as u32));
                    i = j + 1;
                    continue;
                }
                let ch = src[i..].chars().next().unwrap();
                s.push(ch);
                i += ch.len_utf8();
            }
            let tok = if pieces.is_empty() {
                Tok::Str(s)
            } else {
                if !s.is_empty() {
                    pieces.push(IPiece::Lit(s));
                }
                Tok::Interp(pieces)
            };
            out.push(Token { tok, span: sp(start, i), space_before: space });
            space = false;
            continue;
        }
        // Heredoc: only the squiggly form, `<<~ID` / `<<~"ID"` (escapes and
        // `#{}`, like "...") or `<<~'ID'` (raw, like '...').
        if src[i..].starts_with("<<~") {
            let quote = b.get(i + 3).copied().filter(|q| *q == b'"' || *q == b'\'');
            let id_start = i + 3 + quote.is_some() as usize;
            let mut j = id_start;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            if j > id_start && quote.is_none_or(|q| b.get(j) == Some(&q)) {
                pending.push((out.len(), src[id_start..j].to_string(), quote == Some(b'\'')));
                let end = j + quote.is_some() as usize;
                out.push(Token { tok: Tok::Str(String::new()), span: sp(i, end), space_before: space });
                i = end;
                space = false;
                continue;
            }
        }
        // Symbols: `:name`, `:*`; but not `a ? b : c` (space after colon).
        if c == b':' && i + 1 < b.len() && !b[i + 1].is_ascii_whitespace() {
            let prev_is_value = matches!(out.last().map(|t| &t.tok), Some(Tok::Ident(_) | Tok::Int(_) | Tok::BigInt(_) | Tok::Float(..) | Tok::Imag(..) | Tok::Op(")") | Tok::Op("]")));
            if !prev_is_value || space {
                let mut j = i + 1;
                if b[j].is_ascii_alphabetic() || b[j] == b'_' {
                    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                        j += 1;
                    }
                    if j < b.len() && (b[j] == b'?' || b[j] == b'!') {
                        j += 1;
                    }
                } else {
                    for op in ["**", "<=>", "==", "+", "-", "*", "/", "%", "<", ">"] {
                        if src[j..].starts_with(op) {
                            j += op.len();
                            break;
                        }
                    }
                }
                if j > i + 1 {
                    out.push(Token { tok: Tok::Sym(src[i + 1..j].to_string()), span: sp(i, j), space_before: space });
                    i = j;
                    space = false;
                    continue;
                }
            }
        }
        if let Some(op) = BRACKETS.iter().chain(OPS.iter()).find(|op| src[i..].starts_with(**op)) {
            // `&:sym` is only an operator when a symbol name follows.
            if *op == "&:" && !b.get(i + 2).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'*' || *c == b'+') {
                out.push(Token { tok: Tok::Op("&"), span: sp(i, i + 1), space_before: space });
                i += 1;
                space = false;
                continue;
            }
            out.push(Token { tok: Tok::Op(op), span: sp(i, i + op.len()), space_before: space });
            i += op.len();
            space = false;
            continue;
        }
        return Err(Diag::new(sp(i, i + 1), format!("unexpected character `{}`", src[i..].chars().next().unwrap())));
    }
    if let Some((idx, term, _)) = pending.first() {
        return Err(Diag::new(out[*idx].span, format!("unterminated heredoc `{term}`")));
    }
    out.push(Token { tok: Tok::Newline, span: sp(b.len(), b.len()), space_before: false });
    out.push(Token { tok: Tok::Eof, span: sp(b.len(), b.len()), space_before: false });
    Ok(out)
}

/// A heredoc's body, `src[lo..hi]` (whole lines), with `indent` bytes of
/// leading blanks taken off each line. Like a double-quoted string, `#{...}`
/// interpolates and backslash escapes work (`\#` is `#`, so `\#{` stays
/// literal), and a `\` ending a line joins the next one to it (Ruby's
/// continuation; the joined line still loses its indent). `raw` (`<<~'ID'`):
/// the text as written, like '...'.
fn heredoc_body(src: &str, lo: usize, hi: usize, indent: usize, raw: bool, sp: impl Fn(usize, usize) -> Span + Copy) -> Result<Tok, Diag> {
    let b = src.as_bytes();
    let (mut s, mut pieces) = (String::new(), Vec::new());
    let (mut k, mut line_start) = (lo, true);
    while k < hi {
        if line_start {
            let mut n = 0;
            while n < indent && k < hi && matches!(b[k], b' ' | b'\t' | b'\r') {
                k += 1;
                n += 1;
            }
            line_start = false;
            continue;
        }
        match b[k] {
            b'\n' => {
                s.push('\n');
                k += 1;
                line_start = true;
            }
            b'\\' if !raw && b.get(k + 1) == Some(&b'\n') => {
                k += 2;
                line_start = true;
            }
            b'\\' if !raw && b.get(k + 1) == Some(&b'\r') && b.get(k + 2) == Some(&b'\n') => {
                k += 3;
                line_start = true;
            }
            b'\\' if !raw && k + 1 < hi => {
                let (ch, len) = escape(src, k, sp).map_err(|d| d.note("a heredoc takes a double-quoted string's escapes: write `\\\\` for a backslash, or use `<<~'ID'` for raw text"))?;
                s.push(ch);
                k += len;
            }
            b'#' if !raw && b.get(k + 1) == Some(&b'{') => {
                let end = interp_end(b, k + 2).filter(|e| *e < hi).ok_or_else(|| Diag::new(sp(k, k + 2), "unterminated `#{` in heredoc"))?;
                if !s.is_empty() {
                    pieces.push(IPiece::Lit(std::mem::take(&mut s)));
                }
                pieces.push(IPiece::Code(src[k + 2..end].to_string(), sp(k + 2, k + 2).lo));
                k = end + 1;
            }
            _ => {
                let ch = src[k..].chars().next().unwrap();
                s.push(ch);
                k += ch.len_utf8();
            }
        }
    }
    if pieces.is_empty() {
        return Ok(Tok::Str(s));
    }
    if !s.is_empty() {
        pieces.push(IPiece::Lit(s));
    }
    Ok(Tok::Interp(pieces))
}

/// The backslash escape at `src[i]` (with at least one byte after it) in a
/// double-quoted string or heredoc: its character and length. Go's escapes;
/// `\x` up to 7f (a Str is UTF-8 text here), `\u` / `\U` code points;
/// punctuation (any other character) stands for itself.
fn escape(src: &str, i: usize, sp: impl Fn(usize, usize) -> Span) -> Result<(char, usize), Diag> {
    let b = src.as_bytes();
    let hex = |from: usize, n: usize| -> Option<u32> {
        let h = b.get(from..from + n)?;
        u32::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok()
    };
    Ok(match b[i + 1] {
        b'n' => ('\n', 2),
        b't' => ('\t', 2),
        b'r' => ('\r', 2),
        b'0' => ('\0', 2),
        b'a' => ('\x07', 2),
        b'b' => ('\x08', 2),
        b'f' => ('\x0c', 2),
        b'v' => ('\x0b', 2),
        b'e' => ('\x1b', 2),
        b'x' => match hex(i + 2, 2) {
            Some(v) if v < 0x80 => (char::from(v as u8), 4),
            Some(_) => return Err(Diag::new(sp(i, i + 4), "`\\x` escapes above 7f would make invalid UTF-8; write the character, use `\\u`, or build bytes with `Str.from_bytes`")),
            None => return Err(Diag::new(sp(i, i + 2), "`\\x` needs two hex digits")),
        },
        b'u' | b'U' => {
            let n = if b[i + 1] == b'u' { 4 } else { 8 };
            match hex(i + 2, n).and_then(char::from_u32) {
                Some(c) => (c, 2 + n),
                None => return Err(Diag::new(sp(i, i + 2), format!("`\\{}` needs {n} hex digits naming a valid code point", b[i + 1] as char))),
            }
        }
        o if o.is_ascii_alphanumeric() => return Err(Diag::new(sp(i, i + 2), format!("unknown escape `\\{}`", o as char))),
        _ => {
            let ch = src[i + 1..].chars().next().unwrap();
            (ch, 1 + ch.len_utf8())
        }
    })
}

/// The end of a `#{...}` whose code starts at `open`: the index of its `}`.
/// The one scanner for string, heredoc and command-literal interpolation: it
/// skips nested strings and command literals (with their own `#{}`) and `#`
/// comments, so a `}` in either doesn't close the code. The code may span
/// lines (a comment ends at its line).
fn interp_end(b: &[u8], open: usize) -> Option<usize> {
    let (mut j, mut depth) = (open, 1usize);
    while j < b.len() {
        match b[j] {
            b'"' | b'\'' | b'`' => {
                j = quoted_end(b, j, false)?;
                continue;
            }
            b'#' => {
                while j < b.len() && b[j] != b'\n' {
                    j += 1;
                }
                continue;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(j);
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

/// The index after the quoted text whose opening quote is at `start`:
/// `"..."` (escapes, `#{}`), `'...'` (raw) or a command literal (`\` escapes,
/// `#{}`, and '...' / "..." inside it). A string ends with its line, except
/// inside a command literal (`in_cmd`), which may span lines.
fn quoted_end(b: &[u8], start: usize, in_cmd: bool) -> Option<usize> {
    let q = b[start];
    let mut j = start + 1;
    while j < b.len() {
        match b[j] {
            c if c == q => return Some(j + 1),
            b'\n' if !in_cmd && q != b'`' => return None,
            b'\\' if q != b'\'' => j += 2,
            b'#' if q != b'\'' && b.get(j + 1) == Some(&b'{') => j = interp_end(b, j + 2)? + 1,
            b'\'' | b'"' if q == b'`' => j = quoted_end(b, j, true)?,
            _ => j += 1,
        }
    }
    None
}

/// A command literal starting at the backtick at `start`: its token and the
/// index after the closing backtick. Words split on blanks; '...' is literal;
/// "..." takes \" \\ \` \$ escapes and `#{}`; `\c` outside quotes is c.
fn lex_cmd(file: u32, src: &str, base: u32, start: usize) -> Result<(Tok, usize), Diag> {
    let b = src.as_bytes();
    let sp = |lo: usize, hi: usize| Span { file, lo: base + lo as u32, hi: base + hi as u32 };
    let mut parts: Vec<CmdPart> = vec![];
    let mut pieces: Vec<IPiece> = vec![];
    let mut lit = String::new();
    let mut in_word = false;
    let mut word_lo = 0usize;
    let mut i = start + 1;
    fn flush(parts: &mut Vec<CmdPart>, pieces: &mut Vec<IPiece>, lit: &mut String, in_word: &mut bool, span: Span) {
        if !*in_word {
            return;
        }
        if !lit.is_empty() || pieces.is_empty() {
            pieces.push(IPiece::Lit(std::mem::take(lit)));
        }
        parts.push(CmdPart::Word(std::mem::take(pieces), span));
        *in_word = false;
    }
    // Operators, longest first: (text, kind, only at the start of a word).
    const OPS: &[(&str, i64, bool)] = &[("2>&1", 7, true), ("1>&2", 8, true), ("2>>", 6, true), ("2>", 5, true), (">&2", 8, false), ("&>", 9, false), (">>", 4, false), (">", 3, false), ("<", 2, false), ("|", 1, false)];
    loop {
        if i >= b.len() {
            return Err(Diag::new(sp(start, start + 1), "unterminated command literal: no closing backtick"));
        }
        let c = b[i];
        if c == b'`' {
            flush(&mut parts, &mut pieces, &mut lit, &mut in_word, sp(word_lo, i));
            i += 1;
            break;
        }
        if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' {
            flush(&mut parts, &mut pieces, &mut lit, &mut in_word, sp(word_lo, i));
            i += 1;
            continue;
        }
        let rest = &src[i..];
        if rest.starts_with("||") || rest.starts_with("&&") || c == b';' {
            let t = if c == b';' { ";" } else { &rest[..2] };
            flush(&mut parts, &mut pieces, &mut lit, &mut in_word, sp(word_lo, i));
            parts.push(CmdPart::Bad(format!("`{t}` isn't supported in a command literal: run the commands separately (`a`.~run, `b`.~run) and use the language's control flow"), String::new(), sp(i, i + t.len())));
            i += t.len();
            continue;
        }
        if let Some(&(text, kind, _)) = OPS.iter().find(|(t, _, ws)| rest.starts_with(t) && (!ws || !in_word)) {
            flush(&mut parts, &mut pieces, &mut lit, &mut in_word, sp(word_lo, i));
            parts.push(CmdPart::Op(kind, sp(i, i + text.len())));
            i += text.len();
            continue;
        }
        let unsupported = match c {
            b'&' => Some(("there are no background jobs", "start it in a task: `spawn { `cmd`.~run }`")),
            b'$' => Some(("there are no shell variables", "interpolate: `echo #{x}`, `#{os.getenv(\"HOME\") || \"\"}`")),
            b'*' | b'?' | b'[' => Some(("there is no globbing", "quote it ('*.c'), or expand it in the program: `rm #{*sh.~glob(\"*.o\")}`")),
            b'(' | b')' | b'{' | b'}' | b'~' => Some(("this is shell syntax the literal doesn't have", "quote it if it's part of an argument: '(...)'")),
            _ => None,
        };
        if let Some((what, hint)) = unsupported {
            parts.push(CmdPart::Bad(format!("unquoted `{}` in a command literal: {what}", c as char), hint.to_string(), sp(i, i + 1)));
            i += 1;
            continue;
        }
        if !in_word {
            in_word = true;
            word_lo = i;
        }
        match c {
            b'\\' => {
                let Some(ch) = src[i + 1..].chars().next() else {
                    return Err(Diag::new(sp(i, i + 1), "a command literal can't end with `\\`"));
                };
                lit.push(ch);
                i += 1 + ch.len_utf8();
            }
            b'\'' => {
                let close = src[i + 1..].find('\'').ok_or_else(|| Diag::new(sp(i, i + 1), "unterminated '...' in a command literal"))?;
                lit.push_str(&src[i + 1..i + 1 + close]);
                i += close + 2;
            }
            b'"' => {
                i += 1;
                loop {
                    if i >= b.len() {
                        return Err(Diag::new(sp(word_lo, i), "unterminated \"...\" in a command literal"));
                    }
                    match b[i] {
                        b'"' => {
                            i += 1;
                            break;
                        }
                        b'\\' if matches!(b.get(i + 1), Some(b'"' | b'\\' | b'`' | b'$')) => {
                            lit.push(b[i + 1] as char);
                            i += 2;
                        }
                        b'#' if b.get(i + 1) == Some(&b'{') => {
                            let end = interp_end(b, i + 2).ok_or_else(|| Diag::new(sp(i, i + 2), "unterminated `#{` in a command literal"))?;
                            if !lit.is_empty() {
                                pieces.push(IPiece::Lit(std::mem::take(&mut lit)));
                            }
                            pieces.push(IPiece::Code(src[i + 2..end].to_string(), base + (i + 2) as u32));
                            i = end + 1;
                        }
                        _ => {
                            let ch = src[i..].chars().next().unwrap();
                            lit.push(ch);
                            i += ch.len_utf8();
                        }
                    }
                }
            }
            b'#' if b.get(i + 1) == Some(&b'{') => {
                let end = interp_end(b, i + 2).ok_or_else(|| Diag::new(sp(i, i + 2), "unterminated `#{` in a command literal"))?;
                let body = &src[i + 2..end];
                let alone = word_lo == i && lit.is_empty() && pieces.is_empty();
                let next_ends = matches!(b.get(end + 1), None | Some(b' ' | b'\t' | b'\n' | b'`' | b'|' | b'<' | b'>'));
                if let Some(code) = body.trim_start().strip_prefix('*') {
                    if !(alone && next_ends) {
                        parts.push(CmdPart::Bad("`#{*xs}` splices a list as separate arguments, so it must be a whole word".into(), String::new(), sp(i, end + 1)));
                        i = end + 1;
                        continue;
                    }
                    let off = body.len() - code.len();
                    parts.push(CmdPart::Splice(code.to_string(), base + (i + 2 + off) as u32, sp(i, end + 1)));
                    in_word = false;
                    i = end + 1;
                    continue;
                }
                if !lit.is_empty() {
                    pieces.push(IPiece::Lit(std::mem::take(&mut lit)));
                }
                pieces.push(IPiece::Code(body.to_string(), base + (i + 2) as u32));
                i = end + 1;
            }
            _ => {
                let ch = src[i..].chars().next().unwrap();
                lit.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    // Shape: stages of at least one word; a redirect takes a word.
    let mut k = 0;
    let mut stage_words = 0;
    let mut bad: Option<CmdPart> = None;
    while k < parts.len() && bad.is_none() {
        match &parts[k] {
            CmdPart::Word(..) | CmdPart::Splice(..) => stage_words += 1,
            CmdPart::Bad(..) => {}
            CmdPart::Op(1, s) => {
                if stage_words == 0 {
                    bad = Some(CmdPart::Bad("`|` needs a command on its left".into(), String::new(), *s));
                }
                stage_words = 0;
            }
            CmdPart::Op(7 | 8, _) => {}
            CmdPart::Op(_, s) => {
                if !matches!(parts.get(k + 1), Some(CmdPart::Word(..))) {
                    bad = Some(CmdPart::Bad("a redirect needs a file name after it".into(), String::new(), *s));
                }
                k += 1;
            }
        }
        k += 1;
    }
    if bad.is_none() && stage_words == 0 && !parts.iter().any(|p| matches!(p, CmdPart::Bad(..))) {
        let msg = if parts.is_empty() { "an empty command literal" } else { "a command literal needs a command after its last `|`" };
        bad = Some(CmdPart::Bad(msg.into(), String::new(), sp(start, i)));
    }
    if let Some(b) = bad {
        parts.push(b);
    }
    Ok((Tok::Cmd(parts), i))
}
