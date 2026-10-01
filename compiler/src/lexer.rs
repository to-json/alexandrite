//! Tokens for the braces-only Alexandrite syntax.

use crate::diag::{Diag, Span};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Int(i64),
    /// An integer literal too large for i64, in decimal.
    BigInt(String),
    /// Value and source text.
    Float(f64, String),
    /// A double-quoted string with `#{...}` parts.
    Interp(Vec<IPiece>),
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
    /// `fn` and `ƒ`: a pure `def`
    Fn,
    For,
    In,
    Case,
    Defer,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
    /// Whitespace immediately before this token (decides `f (x)` vs `f(x)`).
    pub space_before: bool,
}

/// Longest first: the lexer takes the first match.
const OPS: [&str; 51] = [
    "&^=", "+%=", "-%=", "*%=", "<<=", ">>=", "**=", "...", "<=>", "**", "==", "=>", "!=", "<=", ">=", "&&", "||", "&^", "<<", ">>", "..",
    "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "+%", "-%", "*%", "->", "&:", "+", "-", "*", "/", "%", "<", ">", "=", "!", "?",
    ":", ".", ",", ";", "|", "&", "^",
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
    // Heredocs whose bodies start on the next line: (index in `out`, terminator, squiggly).
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
            for (idx, term, squiggly) in std::mem::take(&mut pending) {
                let mut lines = Vec::new();
                loop {
                    if i >= b.len() {
                        return Err(Diag::new(out[idx].span, format!("unterminated heredoc `{term}`")));
                    }
                    let end = src[i..].find('\n').map_or(b.len(), |k| i + k);
                    let line = &src[i..end];
                    i = (end + 1).min(b.len());
                    if line.trim() == term {
                        break;
                    }
                    lines.push(line.to_string());
                }
                let indent = if squiggly {
                    lines.iter().filter(|l| !l.trim().is_empty()).map(|l| l.len() - l.trim_start().len()).min().unwrap_or(0)
                } else {
                    0
                };
                let body: String = lines.iter().map(|l| format!("{}\n", l.get(indent..).unwrap_or(""))).collect();
                out[idx].tok = Tok::Str(body);
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
                    Some(p) => (inner[..p].trim().to_string(), inner[p + 1..].trim_end_matches(')').trim().to_string()),
                    None => (inner.trim().to_string(), String::new()),
                };
                out.push(Token { tok: Tok::Directive(name, arg), span: sp(i, end + 1), space_before: space });
                i = end + 1;
                space = false;
                continue;
            }
            if src[i..].starts_with("#[") {
                let end = src[i..].find(']').map(|k| i + k).ok_or_else(|| Diag::new(sp(i, i + 2), "unterminated `#[`"))?;
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
            if i < b.len() && !b[start].is_ascii_uppercase() && (b[i] == b'?' || b[i] == b'!') && b.get(i + 1) != Some(&b'=') {
                let next = b.get(i + 1).copied().unwrap_or(b' ');
                if b[i] == b'!' || next != b':' {
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
                "fn" => Tok::Kw(Kw::Fn),
                "for" => Tok::Kw(Kw::For),
                "in" => Tok::Kw(Kw::In),
                "case" => Tok::Kw(Kw::Case),
                "defer" => Tok::Kw(Kw::Defer),
                w if w.as_bytes()[0].is_ascii_uppercase() => Tok::Const(w.to_string()),
                w => Tok::Ident(w.to_string()),
            };
            out.push(Token { tok, span: sp(start, i), space_before: space });
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
                    s.push(match b[i + 1] {
                        b'n' => '\n',
                        b't' => '\t',
                        b'0' => '\0',
                        o => o as char,
                    });
                    i += 2;
                    continue;
                }
                if q == b'"' && b[i] == b'#' && b.get(i + 1) == Some(&b'{') {
                    // `#{ expr }`: find the matching brace (strings inside count).
                    let open = i + 2;
                    let mut j = open;
                    let mut depth = 1;
                    let mut in_str: Option<u8> = None;
                    while j < b.len() && b[j] != b'\n' {
                        match (in_str, b[j]) {
                            (Some(qq), x) if x == qq => in_str = None,
                            (Some(_), b'\\') => j += 1,
                            (Some(_), _) => {}
                            (None, b'"' | b'\'') => in_str = Some(b[j]),
                            (None, b'{') => depth += 1,
                            (None, b'}') => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        j += 1;
                    }
                    if j >= b.len() || b[j] != b'}' {
                        return Err(Diag::new(sp(i, i + 2), "unterminated `#{` in string"));
                    }
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
        // Heredoc: only the squiggly form, `<<~ID`.
        if src[i..].starts_with("<<~") {
            let id_start = i + 3;
            let mut j = id_start;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            if j > id_start {
                pending.push((out.len(), src[id_start..j].to_string(), true));
                out.push(Token { tok: Tok::Str(String::new()), span: sp(i, j), space_before: space });
                i = j;
                space = false;
                continue;
            }
        }
        // Symbols: `:name`, `:*`; but not `a ? b : c` (space after colon).
        if c == b':' && i + 1 < b.len() && !b[i + 1].is_ascii_whitespace() {
            let prev_is_value = matches!(out.last().map(|t| &t.tok), Some(Tok::Ident(_) | Tok::Int(_) | Tok::BigInt(_) | Tok::Float(..) | Tok::Op(")") | Tok::Op("]")));
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
