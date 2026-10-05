//! `format` / `fmt.sprintf` and friends: Go's Printf directives, checked
//! against their arguments at compile time (the format string is a literal).
//!
//! A directive is parsed as Go's doPrintf does (flags `#0+- `, a width or
//! `*`, a precision or `.*`, argument indexes `[n]`, a verb). Each one then
//! becomes a piece of the result:
//!  - the common plain forms (`%d`, `%v`, `%s`, `%x`, `%.2f`, ...) are
//!    runtime pieces (`FmtPiece`), as before;
//!  - anything else is a call into the fmt engine in builtin.alx
//!    (`__fmt_int`, `__fmt_float`, `__fmt_str`, ... : Go's fmt/format.go),
//!    written as alx text and checked like user code;
//!  - a slice, map, struct, tuple or optional applies the directive to each
//!    element, as Go's printValue does: also text, which calls `format`
//!    again with the same directive for each element.
//!
//! Go reports a bad verb, a missing or extra argument at run time
//! (`%!d(string=x)`); alx refuses them at compile time.

use super::*;

/// The flag bits the engine takes (builtin.alx).
pub(super) const F_MINUS: u32 = 1;
pub(super) const F_PLUS: u32 = 2;
pub(super) const F_SHARP: u32 = 4;
pub(super) const F_SPACE: u32 = 8;
pub(super) const F_ZERO: u32 = 16;
pub(super) const F_WID: u32 = 32;
pub(super) const F_PREC: u32 = 64;
pub(super) const F_PLUSV: u32 = 128;
pub(super) const F_SHARPV: u32 = 256;

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Num {
    None,
    Lit(i64),
    /// From the argument with this index (`*`).
    Arg(usize),
}

#[derive(Clone, Debug)]
pub(super) struct Dir {
    pub flags: u32,
    pub wid: Num,
    pub prec: Num,
    pub verb: char,
    pub arg: usize,
}

#[derive(Clone, Debug)]
pub(super) enum Item {
    Lit(String),
    Dir(Dir),
}

/// Go's parsenum: a decimal number at s[i..] (at most 1e6).
fn parsenum(s: &[char], mut i: usize) -> (Option<i64>, usize) {
    let start = i;
    let mut n: i64 = 0;
    while i < s.len() && s[i].is_ascii_digit() {
        n = n * 10 + (s[i] as i64 - '0' as i64);
        if n > 1_000_000 {
            return (None, i);
        }
        i += 1;
    }
    if i == start { (None, i) } else { (Some(n), i) }
}

/// Parse a format string as Go's doPrintf reads it.
pub(super) fn parse_format(f: &str, nargs: usize) -> Result<Vec<Item>, String> {
    let s: Vec<char> = f.chars().collect();
    let end = s.len();
    let mut items = vec![];
    let mut lit = String::new();
    let mut arg_num = 0usize;
    let mut reordered = false;
    let mut i = 0;
    // `[n]`: an explicit argument index.
    let arg_number = |i: usize, arg_num: usize, reordered: &mut bool| -> Result<(usize, usize, bool), String> {
        if i >= end || s[i] != '[' {
            return Ok((arg_num, i, false));
        }
        *reordered = true;
        let close = (i + 1..end).find(|&j| s[j] == ']').ok_or("an argument index `[n]` without its `]`")?;
        let (n, j) = parsenum(&s, i + 1);
        match n {
            Some(n) if j == close && n >= 1 && (n as usize) <= nargs => Ok((n as usize - 1, close + 1, true)),
            _ => Err(format!("bad argument index `{}`", s[i..=close].iter().collect::<String>())),
        }
    };
    while i < end {
        if s[i] != '%' {
            lit.push(s[i]);
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        let mut flags = 0u32;
        while i < end {
            match s[i] {
                '#' => flags |= F_SHARP,
                '0' => flags |= F_ZERO,
                '+' => flags |= F_PLUS,
                '-' => flags |= F_MINUS,
                ' ' => flags |= F_SPACE,
                _ => break,
            }
            i += 1;
        }
        let (a, ni, mut after_index) = arg_number(i, arg_num, &mut reordered)?;
        arg_num = a;
        i = ni;
        let mut wid = Num::None;
        if i < end && s[i] == '*' {
            i += 1;
            if arg_num >= nargs {
                return Err(format!("`{}` has no argument for its `*` width", s[start..i].iter().collect::<String>()));
            }
            wid = Num::Arg(arg_num);
            arg_num += 1;
            after_index = false;
        } else {
            let (n, ni) = parsenum(&s, i);
            if let Some(n) = n {
                if after_index {
                    return Err(format!("bad argument index in `{}`: a width can't follow `[n]`", s[start..ni].iter().collect::<String>()));
                }
                wid = Num::Lit(n);
            }
            i = ni;
        }
        let mut prec = Num::None;
        if i + 1 < end && s[i] == '.' {
            i += 1;
            if after_index {
                return Err(format!("bad argument index in `{}`: a precision can't follow `[n]`", s[start..i].iter().collect::<String>()));
            }
            let (a, ni, ai) = arg_number(i, arg_num, &mut reordered)?;
            arg_num = a;
            i = ni;
            after_index = ai;
            if i < end && s[i] == '*' {
                i += 1;
                if arg_num >= nargs {
                    return Err(format!("`{}` has no argument for its `*` precision", s[start..i].iter().collect::<String>()));
                }
                prec = Num::Arg(arg_num);
                arg_num += 1;
                after_index = false;
            } else {
                let (n, ni) = parsenum(&s, i);
                prec = Num::Lit(n.unwrap_or(0));
                i = ni;
            }
        } else if i < end && s[i] == '.' {
            // `%.` at the very end: Go reads it as precision 0, then finds no verb.
            i += 1;
            prec = Num::Lit(0);
        }
        if !after_index {
            let (a, ni, ai) = arg_number(i, arg_num, &mut reordered)?;
            arg_num = a;
            i = ni;
            after_index = ai;
        }
        let _ = after_index;
        if i >= end {
            return Err(format!("`{}` has no verb", s[start..].iter().collect::<String>()));
        }
        let verb = s[i];
        i += 1;
        if verb == '%' {
            // Percent absorbs no argument and ignores width and precision.
            lit.push('%');
            continue;
        }
        if arg_num >= nargs {
            return Err(format!("`{}` has no argument: {} given", s[start..i].iter().collect::<String>(), nargs));
        }
        if !lit.is_empty() {
            items.push(Item::Lit(std::mem::take(&mut lit)));
        }
        if wid != Num::None {
            flags |= F_WID;
        }
        if prec != Num::None {
            flags |= F_PREC;
        }
        if verb == 'v' || verb == 'w' {
            // %#v: Go syntax (here: alx syntax); %+v: field names.
            if flags & F_SHARP != 0 {
                flags = (flags & !F_SHARP) | F_SHARPV;
            }
            if flags & F_PLUS != 0 {
                flags = (flags & !F_PLUS) | F_PLUSV;
            }
        }
        items.push(Item::Dir(Dir { flags, wid, prec, verb, arg: arg_num }));
        arg_num += 1;
    }
    if !lit.is_empty() {
        items.push(Item::Lit(lit));
    }
    if !reordered && arg_num < nargs {
        return Err(format!("{} argument(s) but the format uses {}", nargs, arg_num));
    }
    Ok(items)
}

/// The directive as format text again, for applying it to an element
/// (`*` arguments named by their locals).
fn spec_text(d: &Dir, names: &[String]) -> (String, Vec<String>) {
    let mut s = String::from("%");
    let mut extra = vec![];
    let f = d.flags;
    if f & (F_SHARP | F_SHARPV) != 0 {
        s.push('#');
    }
    if f & F_ZERO != 0 {
        s.push('0');
    }
    if f & (F_PLUS | F_PLUSV) != 0 {
        s.push('+');
    }
    if f & F_MINUS != 0 {
        s.push('-');
    }
    if f & F_SPACE != 0 {
        s.push(' ');
    }
    match &d.wid {
        Num::None => {}
        Num::Lit(n) => s.push_str(&n.to_string()),
        Num::Arg(k) => {
            s.push('*');
            extra.push(names[*k].clone());
        }
    }
    match &d.prec {
        Num::None => {}
        Num::Lit(n) => s.push_str(&format!(".{n}")),
        Num::Arg(k) => {
            s.push_str(".*");
            extra.push(names[*k].clone());
        }
    }
    s.push(d.verb);
    (s, extra)
}

/// An alx string literal.
pub(super) fn alx_lit(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '#' => o.push_str("\\#"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            '\r' => o.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn is_int(t: &Ty) -> bool {
    t.int_kind().is_some()
}

fn is_bytes(t: &Ty) -> bool {
    matches!(t, Ty::Array(e) | Ty::Fixed(e, _) if **e == Ty::IntK(IntKind::U8))
}

impl<'w, 'a> FnCx<'w, 'a> {
    /// `format("...", args)`: Go's Printf directives, checked here.
    pub(super) fn format(&mut self, name: &str, args: &[Expr], sp: Span) -> R<TExpr> {
        let (pre, pieces, vals, _w) = self.format_parts(name, args, sp, false)?;
        let f = self.mk(TK::Format(pieces, vals), Ty::Str, sp);
        if pre.is_empty() {
            return Ok(f);
        }
        let mut pre = pre;
        pre.push(TStmt::Expr(f));
        Ok(self.mk(TK::Seq(pre), Ty::Str, sp))
    }

    /// The pieces of a format: the statements binding the arguments, the
    /// pieces, their values, and which arguments `%w` shows (errorf).
    pub(super) fn format_parts(&mut self, name: &str, args: &[Expr], sp: Span, allow_w: bool) -> R<(Vec<TStmt>, Vec<FmtPiece>, Vec<TExpr>, Vec<usize>)> {
        let Some(Expr { kind: ExprKind::Str(fmt), span: fsp, .. }) = args.first() else {
            return Err(Diag::new(sp, format!("`{name}` takes a format string literal first")));
        };
        let fsp = *fsp;
        let items = parse_format(fmt, args.len() - 1).map_err(|m| Diag::new(fsp, format!("`{name}`: {m}")))?;
        // Every argument is evaluated once, in order, into a local.
        let mut pre = vec![];
        let mut names = vec![];
        let mut tys = vec![];
        let mut locals = vec![];
        for a in &args[1..] {
            let v = self.value(a)?;
            let ty = self.resolve(&v.ty);
            let (id, st) = self.opt_tmp(v, a.span);
            pre.push(st);
            names.push(self.locals[id].name.clone());
            tys.push(ty);
            locals.push(id);
        }
        // Width and precision arguments are integers.
        for it in &items {
            if let Item::Dir(d) = it {
                for n in [&d.wid, &d.prec] {
                    if let Num::Arg(k) = n {
                        if !is_int(&tys[*k]) {
                            return Err(Diag::new(args[k + 1].span, format!("`{name}`: a `*` width or precision takes an integer, got {}", tys[*k].show())));
                        }
                    }
                }
            }
        }
        let mut pieces = vec![];
        let mut vals: Vec<TExpr> = vec![];
        let mut wrapped = vec![];
        for it in items {
            match it {
                Item::Lit(s) => pieces.push(FmtPiece::Lit(s)),
                Item::Dir(d) => {
                    if d.verb == 'w' {
                        if !allow_w {
                            return Err(Diag::new(fsp, format!("`{name}`: `%w` wraps an error: only `fmt.errorf` takes it")));
                        }
                        wrapped.push(d.arg);
                    }
                    let t = tys[d.arg].clone();
                    let l = self.mk(TK::Local(locals[d.arg]), t.clone(), args[d.arg + 1].span);
                    let piece = self.fmt_piece_for(name, &d, l, &t, &names, args[d.arg + 1].span, &mut vals)?;
                    pieces.push(piece);
                }
            }
        }
        Ok((pre, pieces, vals, wrapped))
    }

    /// One directive applied to one argument (`v`, a local): a runtime
    /// piece, or the value of engine text pushed onto `vals`.
    #[allow(clippy::too_many_arguments)]
    fn fmt_piece_for(&mut self, name: &str, d: &Dir, v: TExpr, t: &Ty, names: &[String], asp: Span, vals: &mut Vec<TExpr>) -> R<FmtPiece> {
        let k = vals.len();
        let verb = if d.verb == 'w' { 'v' } else { d.verb };
        let plain = d.flags == 0;
        let only_prec = d.flags == F_PREC && matches!(d.prec, Num::Lit(_));
        let bad = |what: &str| Diag::new(asp, format!("`{name}`: `%{}` needs {what}, got {}", d.verb, t.show()));
        if verb == 'T' {
            return Ok(FmtPiece::Lit(t.show()));
        }
        // The runtime's own pieces, for the plain forms.
        let int = is_int(t);
        let stringer = self.fmt_stringer(t);
        if plain && !stringer.is_some_and(|_| verb != 'v') {
            let piece = match (verb, t) {
                ('v', _) if printable(t) && !matches!(t, Ty::Fn(..)) => {
                    self.stringers(t, asp)?;
                    Some(FmtPiece::Str(k))
                }
                ('d', _) if int => Some(FmtPiece::Int(k)),
                ('x', _) | ('X', _) | ('o', _) | ('b', _) if int => Some(FmtPiece::Base(k, if verb == 'o' { 8 } else if verb == 'b' { 2 } else { 16 }, verb == 'X')),
                ('s', Ty::Str) | ('t', Ty::Bool) => Some(FmtPiece::Str(k)),
                ('g', Ty::Float) => Some(FmtPiece::Str(k)),
                ('f' | 'F', Ty::Float) => Some(FmtPiece::Fixed(k, 6)),
                ('e' | 'E', Ty::Float) => Some(FmtPiece::Exp(k, 6, verb == 'E')),
                _ => None,
            };
            if let Some(p) = piece {
                vals.push(v);
                return Ok(p);
            }
        }
        if only_prec && *t == Ty::Float {
            let Num::Lit(p) = d.prec else { unreachable!() };
            let p = p.clamp(0, 1000) as u32;
            let piece = match verb {
                'f' | 'F' => Some(FmtPiece::Fixed(k, p)),
                'e' | 'E' => Some(FmtPiece::Exp(k, p, verb == 'E')),
                _ => None,
            };
            if let Some(p) = piece {
                vals.push(v);
                return Ok(p);
            }
        }
        // Everything else: engine text over the argument's local.
        let LocalName = |x: &TExpr| -> String {
            match &x.kind {
                TK::Local(id) => self.locals[*id].name.clone(),
                _ => unreachable!("format arguments are locals"),
            }
        };
        let vname = LocalName(&v);
        let text = self.fmt_text(name, d, verb, &vname, t, names, asp).map_err(|m| match m {
            FmtErr::Bad(what) => bad(&what),
            FmtErr::Diag(dg) => dg,
        })?;
        let e = self.fmt_expand(&text, asp)?;
        vals.push(e);
        Ok(FmtPiece::Str(k))
    }

    /// Does `t` show itself through a method (Go's Stringer / error)? The
    /// method's name.
    fn fmt_stringer(&self, t: &Ty) -> Option<&'static str> {
        if *t == Ty::Error {
            return Some("message");
        }
        if is_complex(t) {
            return None;
        }
        let tn = t.type_name()?;
        if self.w.by_name.contains_key(&method_name(tn, "to_s")) {
            return Some("to_s");
        }
        None
    }

    /// The engine text formatting value `x` (a local's name) of type `t`.
    #[allow(clippy::too_many_arguments)]
    fn fmt_text(&mut self, name: &str, d: &Dir, verb: char, x: &str, t: &Ty, names: &[String], asp: Span) -> Result<String, FmtErr> {
        let flags = d.flags;
        let sharpv = flags & F_SHARPV != 0;
        // Flags, width and precision as engine arguments.
        let num = |n: &Num| match n {
            Num::None => "0".to_string(),
            Num::Lit(v) => v.to_string(),
            Num::Arg(k) => format!("{}.to_i", names[*k]),
        };
        let (wid, prec) = (num(&d.wid), num(&d.prec));
        let star = matches!(d.wid, Num::Arg(_)) || matches!(d.prec, Num::Arg(_));
        let fl = if star { format!("__fmt_star({flags}, {wid}, {}, {prec}, {})", matches!(d.wid, Num::Arg(_)), matches!(d.prec, Num::Arg(_))) } else { flags.to_string() };
        let wid = if matches!(d.wid, Num::Arg(_)) { format!("__fmt_abs({wid})") } else { wid };
        let v = verb as u32;
        let (spec, extra) = spec_text(d, names);
        let each = |e: &str| -> String {
            let mut a = vec![alx_lit(&spec)];
            a.extend(extra.iter().cloned());
            a.push(e.to_string());
            format!("format({})", a.join(", "))
        };
        // Go's handleMethods: a Stringer or error shows its text for v s x X q.
        if let Some(m) = self.fmt_stringer(t) {
            if sharpv {
                if let Some(tn) = t.type_name() {
                    if self.w.by_name.contains_key(&method_name(tn, "go_string")) {
                        return Ok(format!("__fmt_str({x}.go_string, 115, {fl} &^ 256, {wid}, {prec})"));
                    }
                }
            } else if matches!(verb, 'v' | 's' | 'x' | 'X' | 'q') {
                return Ok(format!("__fmt_str({x}.{m}, {v}, {fl}, {wid}, {prec})"));
            }
        }
        let _ = name;
        match t {
            _ if is_int(t) => {
                if matches!(verb, 'f' | 'F' | 'e' | 'E' | 'g' | 'G') {
                    return Ok(format!("__fmt_float({x}.to_f, {v}, {fl}, {wid}, {prec})"));
                }
                if !matches!(verb, 'v' | 'd' | 'b' | 'o' | 'O' | 'x' | 'X' | 'c' | 'q' | 'U') {
                    return Err(FmtErr::Bad("an integer verb (v d b o O x X c q U)".into()));
                }
                let signed = t.int_kind().is_some_and(|k| k.signed());
                Ok(format!("__fmt_int({x}, {signed}, {v}, {fl}, {wid}, {prec})"))
            }
            Ty::Float => {
                if !matches!(verb, 'v' | 'b' | 'g' | 'G' | 'x' | 'X' | 'f' | 'F' | 'e' | 'E') {
                    return Err(FmtErr::Bad("a float verb (v b g G x X f F e E)".into()));
                }
                Ok(format!("__fmt_float({x}, {v}, {fl}, {wid}, {prec})"))
            }
            _ if is_complex(t) => {
                if !matches!(verb, 'v' | 'b' | 'g' | 'G' | 'x' | 'X' | 'f' | 'F' | 'e' | 'E') {
                    return Err(FmtErr::Bad("a float verb (v b g G x X f F e E)".into()));
                }
                Ok(format!("__fmt_complex({x}, {v}, {fl}, {wid}, {prec})"))
            }
            Ty::Str => {
                if !matches!(verb, 'v' | 's' | 'x' | 'X' | 'q') {
                    return Err(FmtErr::Bad("a string verb (v s x X q)".into()));
                }
                Ok(format!("__fmt_str({x}, {v}, {fl}, {wid}, {prec})"))
            }
            Ty::Bool => {
                if !matches!(verb, 'v' | 't') {
                    return Err(FmtErr::Bad("a Bool verb (v t)".into()));
                }
                Ok(format!("__fmt_bool({x}, {fl}, {wid})"))
            }
            _ if is_bytes(t) && matches!(verb, 's' | 'q' | 'x' | 'X') => {
                let b = if matches!(t, Ty::Fixed(..)) { format!("{x}.to_a") } else { x.to_string() };
                Ok(format!("__fmt_str(Str.from_bytes({b}), {v}, {fl}, {wid}, {prec})"))
            }
            Ty::Array(_) | Ty::Fixed(..) => {
                let e = format!("__fe{}", self.locals.len());
                let sep = if sharpv { ", " } else { " " };
                Ok(format!("\"[\" + {x}.map {{ |{e}| {} }}.to_a.join({}) + \"]\"", each(&e), alx_lit(sep)))
            }
            Ty::Map(..) => {
                let (k, val) = (format!("__fk{}", self.locals.len()), format!("__fv{}", self.locals.len()));
                Ok(format!("__fmt_map({x}, {sharpv}) {{ |{k}, {val}| {} + {} + {} }}", each(&k), alx_lit(if sharpv { " => " } else { ":" }), each(&val)))
            }
            Ty::Opt(_) => {
                let o = format!("__fo{}", self.locals.len());
                Ok(format!("(if {o} = {x} {{ {} }} else {{ __fmt_pad(\"none\", {fl}, {wid}) }})", each(&o)))
            }
            Ty::Struct(sn, fs) => {
                if fs.is_empty() {
                    return Ok(if sharpv { alx_lit(&format!("{}.new", t.show())) } else { "\"{}\"".into() });
                }
                let _ = sn;
                let mut parts = vec![];
                for (i, (f, _)) in fs.iter().enumerate() {
                    let sep = if i == 0 { "" } else if sharpv { ", " } else { " " };
                    let label = if sharpv { format!("{f}: ") } else if flags & F_PLUSV != 0 { format!("{f}:") } else { String::new() };
                    parts.push(format!("{} + {}", alx_lit(&format!("{sep}{label}")), each(&format!("{x}.{f}"))));
                }
                let (open, close) = if sharpv { (format!("{}.new(", t.show()), ")") } else { ("{".to_string(), "}") };
                Ok(format!("{} + {} + {}", alx_lit(&open), parts.join(" + "), alx_lit(close)))
            }
            Ty::Tuple(ts) => {
                let mut parts = vec![];
                for i in 0..ts.len() {
                    let sep = if i == 0 { "" } else if sharpv { ", " } else { " " };
                    parts.push(format!("{} + {}", alx_lit(sep), each(&format!("{x}[{i}]"))));
                }
                let (open, close) = if sharpv { ("(", ")") } else { ("{", "}") };
                Ok(format!("{} + {} + {}", alx_lit(open), parts.join(" + "), alx_lit(close)))
            }
            Ty::Enum(..) | Ty::Iface(_) | Ty::Error | Ty::Handle(_) if printable(t) => {
                if !matches!(verb, 'v' | 's') {
                    return Err(FmtErr::Bad("`%v` or `%s` (it has no other verbs)".into()));
                }
                let shown = if sharpv && matches!(t, Ty::Enum(..)) { format!("{} + \".\" + format(\"%v\", {x})", alx_lit(&t.show())) } else { format!("format(\"%v\", {x})") };
                Ok(format!("__fmt_str({shown}, 115, {fl} &^ 256, {wid}, {prec})"))
            }
            _ => Err(FmtErr::Diag(Diag::new(asp, format!("`{name}` can't show a {}", t.show())))),
        }
    }

    /// `fmt.errorf(format, args)`: an Error with the formatted message.
    /// `"context: %w"` (the common form of Go's wrapping) gives the wrapped
    /// error itself with the context added (`e.wrap(context)`), so it still
    /// matches its type and variants (Go's errors.Is / As); `%w` anywhere
    /// else shows the error's message in a new Failure.
    pub(super) fn errorf(&mut self, args: &[Expr], sp: Span) -> R<TExpr> {
        if let Some(Expr { kind: ExprKind::Str(f), span: fsp, id }) = args.first() {
            if args.len() >= 2 && f.ends_with(": %w") && f.matches("%w").count() == 1 && !f.contains("%[") && !f.contains('*') {
                let prefix = f[..f.len() - 4].to_string();
                let mut pargs = vec![Expr { id: *id, kind: ExprKind::Str(prefix), span: *fsp }];
                pargs.extend_from_slice(&args[1..args.len() - 1]);
                let ctx = self.format("fmt.errorf", &pargs, sp)?;
                let e = self.value(&args[args.len() - 1])?;
                let e = self.to_error(e)?;
                return Ok(self.mk(TK::M(M::ErrWrap, Some(Box::new(e)), vec![ctx], None), Ty::Error, sp));
            }
        }
        let (mut pre, pieces, vals, _) = self.format_parts("fmt.errorf", args, sp, true)?;
        let text = self.mk(TK::Format(pieces, vals), Ty::Str, sp);
        let e = self.to_error(text)?;
        if pre.is_empty() {
            return Ok(e);
        }
        pre.push(TStmt::Expr(e));
        Ok(self.mk(TK::Seq(pre), Ty::Error, sp))
    }

    /// Check generated alx text (an expression) in the current scope.
    pub(super) fn fmt_expand(&mut self, text: &str, sp: Span) -> R<TExpr> {
        let mut toks = crate::lexer::lex(sp.file, text).map_err(|d| Diag::new(sp, format!("internal: generated format code doesn't lex: {}\n{text}", d.msg)))?;
        for t in &mut toks {
            t.span = sp;
        }
        let locals: Vec<String> = self.scopes.iter().flat_map(|s| s.keys().cloned()).collect();
        let mut id = self.gen_id;
        let e = crate::parser::parse_expr(&toks, &mut id, &locals).map_err(|d| Diag::new(sp, format!("internal: generated format code doesn't parse: {}\n{text}", d.msg)))?;
        self.gen_id = id;
        self.value(&e)
    }
}

pub(super) enum FmtErr {
    /// The verb doesn't fit the type: what it needs.
    Bad(String),
    Diag(Diag),
}
