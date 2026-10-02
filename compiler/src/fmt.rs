//! `alx fmt`: the one canonical style (gofmt-style, no options).
//!
//! Token-based: the real lexer supplies tokens and spans; the gaps between
//! tokens (whitespace, comments, heredoc bodies) are re-synthesized. Line
//! breaks are never added or removed, except for collapsing runs of blank
//! lines. The result is re-lexed and compared with the input's tokens, so a
//! formatter bug can never silently change meaning.

use crate::lexer::{lex, IPiece, Kw, Tok, Token};
use std::path::{Path, PathBuf};

enum Line {
    Blank,
    Raw(String),
    Comment(String),
    Code(Code),
}

struct Code {
    text: String,
    /// Bracket tokens in order.
    brackets: Vec<&'static str>,
    lead_closers: usize,
    lead_cont: bool,
    trail_cont: bool,
    ends_open: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Normal,
    /// No space after (unary operator, opening generic, `|` opening params).
    Prefix,
    /// Space after (binary operator, `:`, closing `|`).
    Binary,
}

fn op(t: &Tok) -> Option<&'static str> {
    if let Tok::Op(o) = t {
        Some(o)
    } else {
        None
    }
}

fn value_end(t: &Tok) -> bool {
    match t {
        Tok::Ident(_) | Tok::Const(_) | Tok::Int(_) | Tok::BigInt(_) | Tok::Float(..) | Tok::Str(_) | Tok::Interp(_) | Tok::Sym(_) => true,
        Tok::Kw(k) => matches!(k, Kw::True | Kw::False | Kw::Nil | Kw::None),
        Tok::Op(o) => matches!(*o, ")" | "]" | "}"),
        _ => false,
    }
}

fn wordish(t: &Tok) -> bool {
    matches!(t, Tok::Ident(_) | Tok::Const(_) | Tok::Int(_) | Tok::BigInt(_) | Tok::Float(..) | Tok::Str(_) | Tok::Interp(_) | Tok::Sym(_) | Tok::Kw(_))
}

const ALWAYS_BIN: &[&str] = &[
    "=", "=>", "->", "==", "!=", "<=", ">=", "&&", "||", "^", "<=>", "<<", ">>", "%", "/", "**", "&^", "+%", "-%", "*%", "+=", "-=", "*=", "/=", "%=", "&=",
    "|=", "^=", "&^=", "+%=", "-%=", "*%=", "<<=", ">>=", "**=", "|", "<", ">",
];

#[derive(Default)]
struct State {
    angle: i32,
    params: bool,
    /// Bracket depth, and the depths of pending ternary `?`s.
    depth: i32,
    tern: Vec<i32>,
    /// One entry per open `{`: is it a tight hash literal?
    tight: Vec<bool>,
}

/// Spacing before token `i` (which has a previous token `pi` on its line),
/// and the (kind, value_end) the token leaves behind.
fn step(toks: &[Token], i: usize, pi: usize, pk: Kind, pv: bool, st: &mut State) -> (bool, Kind, bool) {
    let p = &toks[pi].tok;
    let c = &toks[i].tok;
    let cs = toks[i].space_before;
    let next = toks.get(i + 1);
    let next_adjacent = next.is_some_and(|n| !n.space_before && n.tok != Tok::Newline);
    let pop = op(p);
    let cop = op(c);
    let nrm = (Kind::Normal, value_end(c));
    // Operator-method names: `def +(o)`.
    if matches!(p, Tok::Kw(Kw::Def)) {
        if cop.is_some_and(|o| !matches!(o, "(" | "[" | "{")) {
            return (true, Kind::Prefix, false);
        }
        return (true, nrm.0, nrm.1);
    }
    if cop.is_some_and(|o| matches!(o, "," | ";" | ")" | "]")) {
        return (false, nrm.0, nrm.1);
    }
    if cop.is_some_and(|o| matches!(o, "." | "?." | ".." | "...")) {
        return (false, Kind::Prefix, false);
    }
    if pop.is_some_and(|o| matches!(o, "." | "?." | ".." | "...")) {
        return (false, nrm.0, nrm.1);
    }
    if cop == Some("|") && st.params {
        st.params = false;
        return (false, Kind::Binary, false);
    }
    if pk == Kind::Prefix {
        // A prefix operator may be followed by another one, e.g. `-~x`.
        let (_, k, v) = classify(c, cop, pv, false, cs, next_adjacent, st);
        return (false, k, v);
    }
    // `Const<` with no spaces opens a generic argument list.
    if cop == Some("<") && matches!(p, Tok::Const(_)) && !cs && next_adjacent {
        st.angle += 1;
        return (false, Kind::Prefix, false);
    }
    if matches!(cop, Some(">" | ">>")) && st.angle > 0 {
        st.angle = (st.angle - if cop == Some(">>") { 2 } else { 1 }).max(0);
        return (false, Kind::Normal, true);
    }
    if pop.is_some_and(|o| matches!(o, "(" | "[")) {
        let (_, k, v) = classify(c, cop, false, false, cs, next_adjacent, st);
        return (false, k, v);
    }
    // Hash literals (`{` after an operator) are tight inside; blocks and bodies are spaced.
    let tight = st.tight.last() == Some(&true);
    if pop == Some("->") && cop == Some("(") {
        return (cs, nrm.0, nrm.1);
    }
    if pop == Some("{") {
        if cop == Some("}") {
            return (false, nrm.0, nrm.1);
        }
        if cop == Some("|") {
            st.params = true;
            return (!tight, Kind::Prefix, false);
        }
        let (_, k, v) = classify(c, cop, false, false, cs, next_adjacent, st);
        return (!tight, k, v);
    }
    if cop == Some("}") {
        return (!tight, nrm.0, nrm.1);
    }
    if cop == Some("{") {
        return (true, nrm.0, nrm.1);
    }
    let p_ident = matches!(p, Tok::Ident(_));
    let (sp, k, v) = classify(c, cop, pv, p_ident, cs, next_adjacent, st);
    if pk == Kind::Binary || pop.is_some_and(|o| matches!(o, "," | ";" | ":")) {
        return (true, k, v);
    }
    if pop == Some("}") && wordish(c) {
        return (true, k, v);
    }
    (sp.unwrap_or(cs || (wordish(p) && wordish(c))), k, v)
}

/// Classify token `c`: (Some(space) if the operator decides it, kind, value_end).
fn classify(c: &Tok, cop: Option<&'static str>, pv: bool, p_ident: bool, cs: bool, next_adjacent: bool, st: &mut State) -> (Option<bool>, Kind, bool) {
    let Some(o) = cop else {
        return (None, Kind::Normal, value_end(c));
    };
    match o {
        "?" => {
            if cs {
                st.tern.push(st.depth);
                (Some(true), Kind::Binary, false)
            } else {
                (Some(false), Kind::Normal, true)
            }
        }
        ":" => {
            if st.tern.last() == Some(&st.depth) {
                st.tern.pop();
                (Some(true), Kind::Binary, false)
            } else {
                (Some(cs), Kind::Binary, false)
            }
        }
        "!" if pv && !cs => (Some(false), Kind::Normal, true),
        "-" | "+" | "*" | "&" | "^" => {
            let unary = !pv || (p_ident && cs && next_adjacent);
            if unary {
                (Some(cs), Kind::Prefix, false)
            } else {
                (Some(true), Kind::Binary, false)
            }
        }
        "&:" | "!" | "~" => (Some(cs), Kind::Prefix, false),
        _ if ALWAYS_BIN.contains(&o) => (Some(true), Kind::Binary, false),
        _ => (None, Kind::Normal, value_end(c)),
    }
}

fn normalize_comment(c: &str) -> String {
    let c = c.trim_end();
    let b = c.as_bytes();
    if b.len() > 1 && b[1].is_ascii_alphanumeric() {
        format!("# {}", &c[1..])
    } else {
        c.to_string()
    }
}

pub fn format(src: &str) -> Result<String, String> {
    let toks = lex(0, src).map_err(|d| d.msg.clone())?;
    let mut lines: Vec<Line> = Vec::new();
    let mut text = String::new();
    let mut brackets: Vec<&'static str> = Vec::new();
    let mut first: Option<usize> = None;
    let mut prev: Option<(usize, Kind, bool)> = None;
    let mut st = State::default();
    let mut pending: Vec<String> = Vec::new();
    let mut prev_end = 0usize;
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        let lo = t.span.lo as usize;
        let hi = t.span.hi as usize;
        if t.tok == Tok::Eof {
            break;
        }
        if t.tok == Tok::Newline {
            let gap = &src[prev_end.min(lo)..lo];
            let comment = gap.find('#').map(|k| normalize_comment(&gap[k..]));
            match (first, comment) {
                (None, None) => lines.push(Line::Blank),
                (None, Some(c)) => lines.push(Line::Comment(c)),
                (Some(f), c) => {
                    if let Some(c) = c {
                        text.push(' ');
                        text.push_str(&c);
                    }
                    let l = i - 1;
                    let lead_closers = toks[f..=l].iter().take_while(|t| op(&t.tok).is_some_and(|o| matches!(o, ")" | "]" | "}"))).count();
                    let lead_cont = op(&toks[f].tok).is_some_and(|o| matches!(o, "." | "?." | "&&" | "||"));
                    let trail_cont = op(&toks[l].tok).is_some_and(|o| matches!(o, "&&" | "||" | "+" | "-" | "*" | "/" | "=" | "." | "?." | "<<" | "==" | "!="));
                    let ends_open = op(&toks[l].tok).is_some_and(|o| matches!(o, "{" | "(" | "["));
                    lines.push(Line::Code(Code { text: std::mem::take(&mut text), brackets: std::mem::take(&mut brackets), lead_closers, lead_cont, trail_cont, ends_open }));
                }
            }
            first = None;
            prev = None;
            st = State::default();
            prev_end = hi;
            if !pending.is_empty() {
                let mut pos = hi.min(src.len());
                for term in std::mem::take(&mut pending) {
                    while pos < src.len() {
                        let end = src[pos..].find('\n').map_or(src.len(), |k| pos + k);
                        let line = &src[pos..end];
                        pos = (end + 1).min(src.len());
                        lines.push(Line::Raw(line.to_string()));
                        if line.trim() == term {
                            break;
                        }
                    }
                }
                prev_end = pos;
            }
            i += 1;
            continue;
        }
        let tok_text = &src[lo..hi];
        if let Tok::Str(_) = t.tok {
            if let Some(id) = tok_text.strip_prefix("<<~") {
                pending.push(id.to_string());
            }
        }
        let pre_tight = prev.is_some_and(|(pi, _, pv)| op(&toks[pi].tok).is_some() && !pv);
        let (k, v) = match prev {
            Some((pi, pk, pv)) => {
                let (sp, k, v) = step(&toks, i, pi, pk, pv, &mut st);
                if sp {
                    text.push(' ');
                }
                (k, v)
            }
            None => {
                first = Some(i);
                match op(&t.tok) {
                    Some("-" | "+" | "*" | "&" | "&:" | "!" | "~" | "^") => (Kind::Prefix, false),
                    Some("?") => (Kind::Normal, true),
                    _ => (Kind::Normal, value_end(&t.tok)),
                }
            }
        };
        prev = Some((i, k, v));
        if let Tok::Op(o) = t.tok {
            if matches!(o, "(" | ")" | "[" | "]" | "{" | "}") {
                st.depth += if matches!(o, "(" | "[" | "{") { 1 } else { -1 };
                if o == "{" {
                    st.tight.push(pre_tight);
                } else if o == "}" {
                    st.tight.pop();
                }
                brackets.push(o);
            }
            text.push_str(o);
        } else {
            text.push_str(tok_text);
        }
        prev_end = hi;
        i += 1;
    }
    Ok(layout(lines))
}

fn layout(lines: Vec<Line>) -> String {
    let mut stack: Vec<usize> = Vec::new();
    let mut cont = false;
    let mut last_blank = true; // suppresses leading blanks
    let mut last_open = false;
    let mut out_lines: Vec<String> = Vec::new();
    let level = |stack: &Vec<usize>| {
        let mut n = 0;
        let mut prev = usize::MAX;
        for &s in stack {
            if s != prev {
                n += 1;
                prev = s;
            }
        }
        n
    };
    for (id, l) in lines.into_iter().enumerate() {
        match l {
            Line::Blank => {
                if !last_blank && !last_open {
                    out_lines.push(String::new());
                    last_blank = true;
                }
            }
            Line::Raw(s) => {
                out_lines.push(s);
                last_blank = false;
                last_open = false;
                cont = false;
            }
            Line::Comment(c) => {
                out_lines.push(format!("{}{}", "  ".repeat(level(&stack)), c));
                last_blank = false;
                last_open = false;
            }
            Line::Code(c) => {
                if c.lead_closers > 0 && last_blank && out_lines.last().is_some_and(|s| s.is_empty()) {
                    out_lines.pop();
                }
                for _ in 0..c.lead_closers {
                    stack.pop();
                }
                let mut lv = level(&stack);
                if c.lead_cont || cont {
                    lv += 1;
                }
                for b in &c.brackets[c.lead_closers..] {
                    match *b {
                        "(" | "[" | "{" => stack.push(id),
                        _ => {
                            stack.pop();
                        }
                    }
                }
                out_lines.push(format!("{}{}", "  ".repeat(lv), c.text));
                cont = c.trail_cont;
                last_blank = false;
                last_open = c.ends_open;
            }
        }
    }
    while out_lines.last().is_some_and(|s| s.is_empty()) {
        out_lines.pop();
    }
    let mut out = String::new();
    for l in out_lines {
        out.push_str(&l);
        out.push('\n');
    }
    out
}

/// Tokens with positions erased and newline runs collapsed.
pub fn norm_tokens(src: &str) -> Result<Vec<Tok>, String> {
    let toks = lex(0, src).map_err(|d| d.msg.clone())?;
    let mut out: Vec<Tok> = Vec::new();
    for t in toks {
        let tok = match t.tok {
            Tok::Interp(ps) => Tok::Interp(ps.into_iter().map(|p| if let IPiece::Code(s, _) = p { IPiece::Code(s, 0) } else { p }).collect()),
            o => o,
        };
        if tok == Tok::Newline && (out.is_empty() || out.last() == Some(&Tok::Newline)) {
            continue;
        }
        out.push(tok);
    }
    Ok(out)
}

/// Format and verify: the output must lex to the same tokens.
pub fn format_checked(src: &str) -> Result<String, String> {
    let out = format(src)?;
    if norm_tokens(src)? != norm_tokens(&out)? {
        return Err("internal formatter error: tokens would change; left untouched".into());
    }
    Ok(out)
}

fn collect(path: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if path.is_dir() {
        let mut es: Vec<_> = std::fs::read_dir(path)?.collect::<Result<_, _>>()?;
        es.sort_by_key(|e| e.file_name());
        for e in es {
            let p = e.path();
            let hidden = e.file_name().to_string_lossy().starts_with('.');
            if p.is_dir() && !hidden || p.is_file() && p.extension().is_some_and(|x| x == "alx") {
                collect(&p, out)?;
            }
        }
    } else {
        out.push(path.to_path_buf());
    }
    Ok(())
}

const USAGE: &str = "usage: alx fmt [--check] [files|dirs...]   (`-`: stdin to stdout)";

/// `alx fmt [--check] [-] [files|dirs...]`. Returns the process exit code.
pub fn cli(args: &[String]) -> i32 {
    let mut check = false;
    let mut stdin = false;
    let mut paths = Vec::new();
    for a in args {
        match a.as_str() {
            "--check" => check = true,
            "-" => stdin = true,
            f if f.starts_with('-') => {
                eprintln!("alx fmt: unknown flag `{f}`\n{USAGE}");
                return 2;
            }
            f => paths.push(f.to_string()),
        }
    }
    if stdin {
        use std::io::{Read, Write};
        let mut s = String::new();
        if std::io::stdin().read_to_string(&mut s).is_err() {
            eprintln!("alx fmt: cannot read stdin");
            return 2;
        }
        return match format_checked(&s) {
            Ok(o) => {
                let _ = std::io::stdout().write_all(o.as_bytes());
                i32::from(check && o != s)
            }
            Err(e) => {
                eprintln!("alx fmt: <stdin>: {e}");
                2
            }
        };
    }
    if paths.is_empty() {
        eprintln!("{USAGE}");
        return 2;
    }
    let mut files = Vec::new();
    for p in &paths {
        if let Err(e) = collect(Path::new(p), &mut files) {
            eprintln!("alx fmt: {p}: {e}");
            return 2;
        }
    }
    let (mut changed, mut failed) = (false, false);
    for f in files {
        let src = match std::fs::read_to_string(&f) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("alx fmt: {}: {e}", f.display());
                failed = true;
                continue;
            }
        };
        match format_checked(&src) {
            Ok(o) if o != src => {
                changed = true;
                if check {
                    println!("{}", f.display());
                } else if let Err(e) = std::fs::write(&f, o) {
                    eprintln!("alx fmt: {}: {e}", f.display());
                    failed = true;
                }
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("alx fmt: {}: {e}", f.display());
                failed = true;
            }
        }
    }
    if failed {
        2
    } else {
        i32::from(check && changed)
    }
}
