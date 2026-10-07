//! Type data for the editor (`alx check --json`'s `types` and `members`,
//! docs/notes/lsp-plan.md M2.5): the type of every binding in the focused file
//! and of every expression written before a `.`, plus the members of the
//! types that appear. Data only; the server decides what to offer.
//!
//! Read from the checker's state after a collect-mode check (`front::check_collect_with`):
//! only instances that checked are there, so a def that failed contributes nothing.
//!
//! Spans. Locals don't carry their name's span in the typed tree, so a
//! binding's name is found in the source text near the node that introduces
//! it (an assignment, a block, a `case` arm), as a whole identifier. A
//! parameter's span comes from the def's AST. `self` and implicit block
//! parameters (`it`, `_1`) have no name in the source: their span is their
//! scope (the method's def, the block).
//!
//! Generics. Every instance of a generic def is walked; a span whose instances
//! disagree on its type is dropped (so a def instantiated once reports that
//! instance's types, and one used at several types reports only what they share).

use crate::check::World;
use crate::diag::{SourceFile, Span};
use crate::tast::{TBlock, TExpr, TFunc, TSelArm, TStep, TStmt, Ty, TK};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct Binding {
    pub lo: u32,
    pub hi: u32,
    pub name: String,
    pub ty: String,
    /// "local", "param", "self", "it" (an implicit block parameter: its span is the block), "recv" (an expression before a `.`).
    pub kind: &'static str,
    /// The type itself (for `members`).
    pub t: Ty,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub name: String,
    /// "method" or "field".
    pub kind: &'static str,
    /// A method's signature as written after its name (`(n: Int) -> ~Str`; "" when unknown or none),
    /// a field's type.
    pub sig: String,
}

#[derive(Debug, Default, Clone)]
pub struct TypeMap {
    /// By span start.
    pub types: Vec<Binding>,
    /// Each named type (and builtin) that appears in `types`, by the name `types` spells it.
    pub members: Vec<(String, Vec<Member>)>,
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// `name` as a whole identifier at byte `i` of `t`.
fn ident_at(t: &[u8], i: usize, name: &[u8]) -> bool {
    if !t[i..].starts_with(name) {
        return false;
    }
    let before = i.checked_sub(1).map(|j| t[j]);
    if before.is_some_and(|c| is_ident(c) || c == b'.' || c == b'@' || c == b':') {
        return false;
    }
    !t.get(i + name.len()).is_some_and(|&c| is_ident(c))
}

/// Where a search for a name starts.
#[derive(Clone, Copy, PartialEq)]
enum Dir {
    /// Forward from the anchor, then back (an assignment's span starts at or before its name).
    Fwd,
    /// Back from the anchor, then forward (the anchor is a use, a value, or a block written after its parameters).
    Back,
}

/// Is the identifier at `t[i..j]` where a name is bound? Followed by `=` (not `==`, `=>`),
/// `,`, `|`, `:`, `)` or `in`, and preceded by a line start, `(`, `,`, `|`, `{` or `for`/`if`/`when`/`elsif`/`while`.
fn binding_ctx(t: &[u8], i: usize, j: usize) -> bool {
    let rest = &t[j..];
    let k = rest.iter().position(|&c| c != b' ' && c != b'\t').unwrap_or(rest.len());
    let r = &rest[k..];
    let after = (r.starts_with(b"=") && !r.starts_with(b"==") && !r.starts_with(b"=>"))
        || r.starts_with(b",")
        || r.starts_with(b"|")
        || r.starts_with(b":") && !r.starts_with(b"::")
        || r.starts_with(b")")
        || (r.starts_with(b"in") && r.get(2).is_some_and(|c| !is_ident(*c)));
    if !after {
        return false;
    }
    let pre = &t[..i];
    let k = pre.iter().rposition(|&c| c != b' ' && c != b'\t').map_or(0, |k| k + 1);
    let p = &pre[..k];
    p.is_empty()
        || matches!(p.last(), Some(b'\n' | b'(' | b',' | b'|' | b'{' | b';'))
        || [&b"for"[..], b"if", b"when", b"elsif", b"while", b"unless"].iter().any(|kw| p.ends_with(kw) && (p.len() == kw.len() || !is_ident(p[p.len() - kw.len() - 1])))
}

/// The occurrence of `name` (where it is bound) nearest to `anchor` in `[lo, hi)`.
fn find_ident(f: &SourceFile, name: &str, anchor: u32, lo: u32, hi: u32, dir: Dir) -> Option<(u32, u32)> {
    let t = f.text.as_bytes();
    let n = name.as_bytes();
    let (lo, hi) = (lo as usize, (hi as usize).min(t.len()));
    let anchor = (anchor as usize).clamp(lo, hi);
    if n.is_empty() || hi < lo + n.len() {
        return None;
    }
    let ok = |i: usize| i + n.len() <= hi && ident_at(t, i, n) && binding_ctx(t, i, i + n.len());
    let fwd = || (anchor..=hi - n.len()).find(|&i| ok(i));
    let back = || (lo..anchor).rev().find(|&i| ok(i));
    let i = match dir {
        Dir::Fwd => fwd().or_else(back),
        Dir::Back => back().or_else(fwd),
    }?;
    Some((i as u32, (i + n.len()) as u32))
}

fn user_name(n: &str) -> bool {
    let b = n.as_bytes();
    !n.is_empty() && !n.starts_with("__") && (b[0].is_ascii_alphabetic() || b[0] == b'_') && b.iter().all(|&c| is_ident(c) || c == b'?' || c == b'!')
}

/// A type as the source spells it: `Ty::show`, but another package's types by
/// its import name (`crypto/tls.Config` is `tls.Config`) and `()` as `Unit`.
pub fn spell(t: &Ty) -> String {
    let sp = |ts: &[Ty]| ts.iter().map(spell).collect::<Vec<_>>().join(", ");
    match t {
        Ty::Unit => "Unit".into(),
        Ty::Struct(n, _) | Ty::Enum(n, _) | Ty::Iface(n) | Ty::Rec(n) => unpath(n),
        Ty::Handle(n) => format!("@{}", unpath(n)),
        Ty::Opt(t) => format!("{}?", spell(t)),
        Ty::Array(t) => format!("[{}]", spell(t)),
        Ty::Fixed(t, n) => format!("[{}; {n}]", spell(t)),
        Ty::Map(k, v) => format!("Map[{}, {}]", spell(k), spell(v)),
        Ty::Result(t) => format!("~{}", spell(t)),
        Ty::Task(t) => format!("Task[{}]", spell(t)),
        Ty::Chan(t) => format!("Chan[{}]", spell(t)),
        Ty::Pool(t) => format!("Pool[{}]", spell(t)),
        Ty::Mutex(t) => format!("Mutex[{}]", spell(t)),
        Ty::Atomic(t) => format!("Atomic[{}]", spell(t)),
        Ty::Fn(ps, r) => format!("({}) -> {}", sp(ps), spell(r)),
        Ty::Tuple(ts) => format!("({})", sp(ts)),
        Ty::Seq(t, true) => format!("Lazy[{}]", spell(t)),
        Ty::Seq(t, false) => format!("Enumerable[{}]", spell(t)),
        Ty::Gen(t) => format!("Enumerator[{}]", spell(t)),
        Ty::Yielder(t) => format!("Yielder[{}]", spell(t)),
        _ => t.show(),
    }
}

/// `crypto/tls.Config` → `tls.Config`, `math/rand/v2.Rand` → `rand.Rand`, inside generic arguments too.
fn unpath(n: &str) -> String {
    if !n.contains('/') {
        return n.to_string();
    }
    let mut out = String::with_capacity(n.len());
    let mut word = String::new();
    let flush = |w: &mut String, out: &mut String| {
        match w.rfind('/').and_then(|s| w[s..].find('.').map(|d| s + d)) {
            Some(dot) => {
                out.push_str(&crate::ast::default_import_name(&w[..dot]));
                out.push_str(&w[dot..]);
            }
            None => out.push_str(w),
        }
        w.clear();
    };
    for c in n.chars() {
        if c.is_alphanumeric() || matches!(c, '_' | '/' | '.' | '-') {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

struct Walk<'a> {
    file: u32,
    sf: &'a SourceFile,
    f: &'a TFunc,
    win: (u32, u32),
    seen: HashSet<usize>,
    out: &'a mut Vec<Binding>,
    /// For receivers: (callee FuncId) -> is a method with `self`.
    w: &'a World<'a>,
}

impl Walk<'_> {
    fn in_file(&self, sp: Span) -> bool {
        sp.file == self.file && sp.hi as usize <= self.sf.text.len()
    }

    fn bind(&mut self, id: usize, anchor: Span, kind: &'static str, dir: Dir) {
        if !self.seen.insert(id) {
            return;
        }
        let Some(l) = self.f.locals.get(id) else { return };
        if !user_name(&l.name) || !self.in_file(anchor) {
            return;
        }
        if let Some((lo, hi)) = find_ident(self.sf, &l.name, anchor.lo, self.win.0, self.win.1, dir) {
            self.out.push(Binding { lo, hi, name: l.name.clone(), ty: spell(&l.ty), t: (&l.ty).clone(), kind });
        }
    }

    /// An implicit block parameter (`it`, `_1`): its scope.
    fn implicit(&mut self, id: usize, b: &TBlock) {
        if !self.seen.insert(id) || !self.in_file(b.span) {
            return;
        }
        let Some(l) = self.f.locals.get(id) else { return };
        self.out.push(Binding { lo: b.span.lo, hi: b.span.hi, name: l.name.clone(), ty: spell(&l.ty), t: (&l.ty).clone(), kind: "it" });
    }

    /// `recv` is followed by `.` (or `?.`) in the source: the receiver of a method call or field access.
    fn recv(&mut self, r: &TExpr) {
        if !self.in_file(r.span) || r.span.lo >= r.span.hi {
            return;
        }
        let t = self.sf.text.as_bytes();
        let rest = &t[r.span.hi as usize..];
        let k = rest.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(rest.len());
        let after = &rest[k..];
        if !(after.starts_with(b".") && !after.starts_with(b"..") || after.starts_with(b"?.")) {
            return;
        }
        let name = String::from_utf8_lossy(&t[r.span.lo as usize..r.span.hi as usize]).to_string();
        // A `!` call's receiver is a one-element view of the place (`__view`): its element is the place's type.
        let ty = match (&r.kind, &r.ty) {
            (TK::Local(id), Ty::Array(e)) if self.f.locals.get(*id).is_some_and(|l| l.name.starts_with("__view")) => (**e).clone(),
            _ => r.ty.clone(),
        };
        self.out.push(Binding { lo: r.span.lo, hi: r.span.hi, name, ty: spell(&ty), t: ty, kind: "recv" });
    }

    /// `place.m!(...)`: the place (a local, then steps) is written before `.m!`.
    fn bang_recv(&mut self, e: &TExpr, view: usize, call: &TExpr) {
        if !self.in_file(e.span) {
            return;
        }
        let callee = match &call.kind {
            TK::Call(fid, _) => self.w.funcs.get(*fid).and_then(|f| f.as_ref()).map(|f| f.src_name.clone()),
            _ => None,
        };
        let Some(m) = callee.as_deref().and_then(|n| n.rsplit('.').next()) else { return };
        let Some(Ty::Array(t)) = self.f.locals.get(view).map(|l| &l.ty) else { return };
        let text = &self.sf.text[e.span.lo as usize..e.span.hi as usize];
        let pat = format!(".{m}");
        let Some(i) = text.find(&pat) else { return };
        let place = text[..i].trim_end();
        if place.is_empty() || place.contains(['\n', '(', ' ']) {
            return;
        }
        self.out.push(Binding { lo: e.span.lo, hi: e.span.lo + place.len() as u32, name: place.to_string(), ty: spell(t), t: (**t).clone(), kind: "recv" });
    }

    fn block(&mut self, b: &TBlock) {
        // `{ |a, b| ...}`: the names are in the header; otherwise (`for x in xs { }`,
        // a lambda's `->(x: T) { }`) they are written before the block.
        let header = header_len(self.sf, b.span);
        for &p in &b.params {
            let n = self.f.locals.get(p).map(|l| l.name.as_str()).unwrap_or("");
            let implicit = n == "it" || (n.len() > 1 && n.starts_with('_') && n[1..].bytes().all(|c| c.is_ascii_digit()));
            if header > 0 {
                let named = find_ident(self.sf, n, b.span.lo, b.span.lo, b.span.lo + header + 1, Dir::Fwd).is_some();
                if named {
                    self.bind(p, b.span, "local", Dir::Fwd);
                } else if implicit {
                    self.implicit(p, b);
                }
            } else if implicit {
                self.implicit(p, b);
            } else {
                self.bind(p, b.span, "local", Dir::Back);
            }
        }
        self.stmts(&b.body);
    }

    fn stmts(&mut self, ss: &[TStmt]) {
        for s in ss {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &TStmt) {
        match s {
            TStmt::Expr(e) | TStmt::Defer(e) | TStmt::Fail(e, _) | TStmt::Break(Some(e), _) | TStmt::Return(Some(e), _) => self.expr(e),
            TStmt::MultiAssign(ids, es) => {
                let anchor = es.first().map(|e| e.span).unwrap_or_default();
                // The names come before the values: search back from the first value.
                for &id in ids {
                    self.bind(id, Span { file: anchor.file, lo: anchor.lo, hi: anchor.lo }, "local", Dir::Back);
                }
                es.iter().for_each(|e| self.expr(e));
            }
            TStmt::While(c, b) => {
                self.expr(c);
                self.stmts(b);
            }
            TStmt::If(c, a, b) => {
                self.expr(c);
                self.stmts(a);
                self.stmts(b);
            }
            _ => {}
        }
    }

    fn expr(&mut self, e: &TExpr) {
        match &e.kind {
            TK::Assign(id, v) => {
                if !self.seen.contains(id) {
                    self.bind(*id, e.span, "local", Dir::Fwd);
                }
                self.expr(v);
            }
            TK::Local(id) => {
                // A local first seen at a use (a loop variable): the use is its name.
                if !self.seen.contains(id) {
                    self.bind(*id, e.span, "local", Dir::Back);
                }
            }
            TK::IndexAssign(_, i, v) => {
                self.expr(i);
                self.expr(v);
            }
            TK::Neg(v) | TK::Not(v) | TK::Try(v) | TK::Puts(v) | TK::Panic(v) | TK::Some(v) => self.expr(v),
            TK::Bin(_, a, b) | TK::Range(a, b, _) | TK::Index(a, b) => {
                self.expr(a);
                self.expr(b);
            }
            TK::Ternary(c, a, b) => {
                self.expr(c);
                self.expr(a);
                self.expr(b);
            }
            TK::Slice(a, lo, hi, _) => {
                self.expr(a);
                lo.iter().chain(hi.iter()).for_each(|x| self.expr(x));
            }
            TK::Call(fid, args) => {
                let method = self.w.funcs.get(*fid).and_then(|f| f.as_ref()).is_some_and(|f| f.src_name.contains('.') && f.params.first().is_some_and(|&p| f.locals[p].name == "self"));
                if method {
                    if let Some(r) = args.first() {
                        self.recv(r);
                    }
                }
                args.iter().for_each(|a| self.expr(a));
            }
            TK::Array(args) | TK::Format(_, args) => args.iter().for_each(|a| self.expr(a)),
            TK::Seq(ss) => self.stmts(ss),
            TK::PlaceAssign(_, steps, _, v) => {
                self.steps(steps);
                self.expr(v);
            }
            TK::Bang(_, steps, view, v) => {
                self.bang_recv(e, *view, v);
                self.steps(steps);
                self.expr(v);
            }
            TK::M(_, r, args, blk) => {
                if let Some(r) = r {
                    self.recv(r);
                    self.expr(r);
                }
                args.iter().for_each(|a| self.expr(a));
                if let Some(b) = blk {
                    self.block(b);
                }
            }
            TK::Select(arms, d) => {
                for a in arms {
                    match a {
                        TSelArm::Recv { ch, bind, body } => {
                            if let Some(id) = bind {
                                self.bind(*id, Span { file: ch.span.file, lo: ch.span.lo, hi: ch.span.lo }, "local", Dir::Back);
                            }
                            self.expr(ch);
                            self.stmts(body);
                        }
                        TSelArm::Send { ch, val, body } => {
                            self.expr(ch);
                            self.expr(val);
                            self.stmts(body);
                        }
                    }
                }
                if let Some(d) = d {
                    self.stmts(d);
                }
            }
            _ => {}
        }
    }

    fn steps(&mut self, steps: &[TStep]) {
        for st in steps {
            if let TStep::Index(i) = st {
                self.expr(i);
            }
        }
    }
}

/// Bytes from a block's start through its `|params|` header (0 when it has none).
fn header_len(f: &SourceFile, b: Span) -> u32 {
    let t = &f.text.as_bytes()[b.lo as usize..(b.hi as usize).min(f.text.len())];
    let open = t.iter().position(|&c| c == b'{').map(|i| i + 1).unwrap_or(0);
    let rest = &t[open..];
    let k = rest.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(rest.len());
    if rest.get(k) != Some(&b'|') {
        return 0;
    }
    match rest[k + 1..].iter().position(|&c| c == b'|') {
        Some(j) => (open + k + 1 + j) as u32,
        None => 0,
    }
}

/// The text of a def's signature after its name: `(a: Int) -> ~Str`, whitespace collapsed.
fn def_sig(f: &SourceFile, from: u32) -> String {
    let t = f.text.as_bytes();
    let mut depth = 0i32;
    let mut i = from as usize;
    while i < t.len() {
        match t[i] {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b'{' | b'\n' | b'#' if depth <= 0 => break,
            b'=' if depth <= 0 && t.get(i + 1) != Some(&b'>') => break,
            b'"' => {
                i += 1;
                while i < t.len() && t[i] != b'"' {
                    i += if t[i] == b'\\' { 2 } else { 1 };
                }
            }
            _ => {}
        }
        i += 1;
    }
    let s = String::from_utf8_lossy(&t[from as usize..i.min(t.len())]);
    s.split_whitespace().collect::<Vec<_>>().join(" ").replace("( ", "(").replace(" )", ")")
}

/// The named types inside `t` (a struct, enum or interface, as `Ty::show` names it), and `t` itself.
fn named_in(t: &Ty, out: &mut Vec<Ty>, depth: u32) {
    if depth > 4 {
        return;
    }
    match t {
        Ty::Struct(..) | Ty::Enum(..) | Ty::Iface(_) | Ty::Error => out.push(t.clone()),
        Ty::Array(e) | Ty::Fixed(e, _) | Ty::Opt(e) | Ty::Result(e) | Ty::Task(e) | Ty::Chan(e) | Ty::Mutex(e) | Ty::Atomic(e) | Ty::Seq(e, _) | Ty::Gen(e) => named_in(e, out, depth + 1),
        Ty::Map(k, v) => {
            named_in(k, out, depth + 1);
            named_in(v, out, depth + 1);
        }
        Ty::Tuple(ts) => ts.iter().for_each(|t| named_in(t, out, depth + 1)),
        _ => {}
    }
}

/// `Box[Int]` → `Box`.
fn base_name(n: &str) -> &str {
    n.split('[').next().unwrap_or(n)
}

fn members_of(w: &World, t: &Ty, by_owner: &HashMap<String, Vec<usize>>) -> Vec<Member> {
    let mut out = vec![];
    let mut seen = HashSet::new();
    let mut methods_of = |owner: &str, out: &mut Vec<Member>| {
        for &i in by_owner.get(owner).into_iter().flatten() {
            let d = &w.defs[i].def;
            if d.params.first().is_none_or(|p| p.name != "self") {
                continue; // static: `Type.m`, not `x.m`
            }
            let m = &d.name[owner.len() + 1..];
            if m.starts_with("__") || !seen.insert(m.to_string()) {
                continue;
            }
            let sig = w.sm.files.get(d.name_span.file as usize).map(|f| def_sig(f, d.name_span.hi)).unwrap_or_default();
            out.push(Member { name: m.to_string(), kind: "method", sig });
        }
    };
    match t {
        Ty::Struct(n, fs) => {
            for (f, ft) in fs.iter() {
                if !f.starts_with("__") {
                    out.push(Member { name: f.clone(), kind: "field", sig: spell(ft) });
                }
            }
            methods_of(base_name(n), &mut out);
        }
        Ty::Enum(n, _) => methods_of(base_name(n), &mut out),
        Ty::Iface(n) => {
            for m in w.ifaces.get(n).into_iter().flatten() {
                let ps: Vec<String> = m.params.iter().map(spell).collect();
                let args = if ps.is_empty() { String::new() } else { format!("({})", ps.join(", ")) };
                let sig = if matches!(m.ret, Ty::Unit) { args } else { format!("{args} -> {}", spell(&m.ret)).trim_start().to_string() };
                out.push(Member { name: m.name.clone(), kind: "method", sig });
            }
        }
        _ => {}
    }
    for m in crate::check::builtin_method_names(t) {
        if !out.iter().any(|x| x.name == m) {
            out.push(Member { name: m.to_string(), kind: "method", sig: String::new() });
        }
    }
    out
}

/// The type map of source file `file` from the checker's state.
pub fn build(w: &World, file: u32) -> TypeMap {
    let Some(sf) = w.sm.files.get(file as usize) else { return TypeMap::default() };
    let mut all: Vec<Binding> = vec![];
    for f in w.funcs.iter().flatten() {
        if f.span.file != file || f.external {
            continue;
        }
        let win = if f.span.hi > f.span.lo { (f.span.lo, f.span.hi) } else { (0, sf.text.len() as u32) };
        let mut wk = Walk { file, sf, f, win, seen: HashSet::new(), out: &mut all, w };
        // Parameters: the def's AST has their spans.
        let def = w.defs.iter().find(|d| d.def.name == f.src_name && d.def.span.file == file && d.def.span.lo <= f.span.lo && f.span.lo <= d.def.span.hi);
        for (k, &p) in f.params.iter().enumerate() {
            wk.seen.insert(p);
            let l = &f.locals[p];
            if l.name == "self" {
                if f.span.hi > f.span.lo {
                    wk.out.push(Binding { lo: f.span.lo, hi: f.span.hi, name: "self".into(), ty: spell(&l.ty), t: (&l.ty).clone(), kind: "self" });
                }
                continue;
            }
            if !user_name(&l.name) {
                continue;
            }
            let sp = def.and_then(|d| d.def.params.get(k)).filter(|q| q.name == l.name).map(|q| (q.span.lo, q.span.lo + q.name.len() as u32));
            if let Some((lo, hi)) = sp.or_else(|| find_ident(sf, &l.name, f.span.lo, f.span.lo, f.span.hi, Dir::Fwd)) {
                wk.out.push(Binding { lo, hi, name: l.name.clone(), ty: spell(&l.ty), t: (&l.ty).clone(), kind: "param" });
            }
        }
        wk.stmts(&f.body);
    }
    // One entry per span; instances that disagree drop it.
    all.sort_by(|a, b| (a.lo, a.hi, a.kind).cmp(&(b.lo, b.hi, b.kind)));
    let mut types: Vec<Binding> = vec![];
    let mut bad = false;
    for b in all {
        match types.last() {
            Some(p) if p.lo == b.lo && p.hi == b.hi && p.kind == b.kind => {
                if p.ty != b.ty {
                    bad = true;
                }
            }
            _ => {
                if bad {
                    types.pop();
                }
                bad = false;
                types.push(b);
            }
        }
    }
    if bad {
        types.pop();
    }
    // `self` of a `!` method is `[T]` (a one-element view): show T.
    for b in types.iter_mut().filter(|b| b.kind == "self") {
        if let Ty::Array(t) = &b.t {
            b.t = (**t).clone();
            b.ty = spell(&b.t);
        }
    }
    let members = members_for(w, file, &types);
    TypeMap { types, members }
}

/// Members of every type `types` names (its own type and the named types inside it).
fn members_for(w: &World, file: u32, types: &[Binding]) -> Vec<(String, Vec<Member>)> {
    let dir = w.sm.files.get(file as usize).and_then(|f| f.path.as_deref()).and_then(|p| p.parent());
    let mut have: HashSet<String> = HashSet::new();
    let mut tys: Vec<(String, Ty)> = vec![];
    for b in types {
        if have.contains(&b.ty) {
            continue;
        }
        let mut ns = vec![b.t.clone()];
        named_in(&b.t, &mut ns, 0);
        for n in ns {
            let s = spell(&n);
            if have.insert(s.clone()) {
                tys.push((s, n));
            }
        }
    }
    let mut by_owner: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, d) in w.defs.iter().enumerate() {
        if let Some((o, _)) = d.def.name.rsplit_once('.') {
            // Another package's methods: only its `pub` ones are callable here.
            let path = w.sm.files.get(d.def.span.file as usize).and_then(|f| f.path.as_deref());
            let local = path.is_some_and(|p| p.parent() == dir);
            if d.pkg.is_empty() || d.def.public || local {
                by_owner.entry(o.to_string()).or_default().push(i);
            }
        }
    }
    let mut out = vec![];
    for (s, t) in &tys {
        let ms = members_of(w, t, &by_owner);
        if !ms.is_empty() {
            out.push((s.clone(), ms));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}
