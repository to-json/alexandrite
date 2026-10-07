//! Recursive-descent parser. Tracks local variable names the way Ruby's
//! parser does, so `name arg` (a command call) and `name` (a local) can be
//! told apart.

use crate::ast::*;
use crate::diag::{Diag, Span};
use crate::lexer::{CmdPart, IPiece, Kw, Tok, Token};
use std::collections::HashSet;

/// Methods of a generic type take `self` untyped: each instance is checked
/// with the concrete type.
fn generic_self(methods: &mut [Def], tparams: &[TParam]) {
    for m in methods {
        if let Some(p) = m.params.first_mut().filter(|p| p.name == "self") {
            p.ty = None;
        } else {
            // A static method (`def self.make(v: T) -> R[T]`) is generic over the type's parameters.
            let own = std::mem::take(&mut m.tparams);
            m.tparams = tparams.iter().cloned().chain(own).collect();
        }
    }
}

/// A def whose body may be missing (an interface's required method).
pub struct DefSig {
    pub errs: Option<Vec<String>>,
    pub name: String,
    pub span: Span,
    pub tparams: Vec<TParam>,
    pub name_span: Span,
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    pub fallible: bool,
    pub pure: bool,
    pub track_caller: bool,
    /// `#[attr] pub def`: `pub` after the attributes.
    pub public: bool,
    pub ffi: Option<String>,
    pub body: Option<Vec<Stmt>>,
}

pub struct Parser<'a> {
    toks: &'a [Token],
    pos: usize,
    next_id: &'a mut NodeId,
    scopes: Vec<HashSet<String>>,
    /// Inside an `if`/`while` condition: `{` is a block only if `|` follows.
    in_cond: bool,
    /// File-level `using`s so far (defs below see them).
    usings: Vec<String>,
    /// Parsing an `extern def`: no body, an optional `= "symbol"`.
    extern_mode: bool,
    /// `#[derive(...)]` names read before the declaration being parsed.
    cur_derives: Vec<String>,
    /// Type-level `#[asn1(...)]` options read before the declaration.
    cur_asn1: crate::derive_asn1::A1,
    /// Derives to expand once the whole module is read (see derive.rs).
    jobs: Vec<crate::derive::DeriveJob>,
    /// `#[data("pkg.Name")]` read before the declaration being parsed.
    cur_data_name: Option<String>,
    /// `#[json(transparent)]` read before the declaration being parsed.
    cur_transparent: bool,
    /// `#[shareable]` read before the interface being parsed (port-issues #270).
    cur_shareable: Option<Span>,
    /// derive(Data) jobs (see derive_data.rs).
    djobs: Vec<crate::derive_data::DataJob>,
    /// derive(Gob) jobs (see derive_gob.rs).
    gjobs: Vec<crate::derive_gob::GobJob>,
    /// derive(Xml) jobs (see derive_xml.rs).
    xjobs: Vec<crate::derive_xml::XmlJob>,
    /// The file's own name for `os/exec` (`import "os/exec"`), which command
    /// literals use when no local hides it.
    cmd_pkg: Option<String>,
    /// A command literal that needed the parser's own `os/exec` import
    /// (`CMD_PKG`): the first one's span.
    cmd_own: Option<Span>,
    /// Recovering mode: errors are recorded and parsing resumes at the next declaration.
    recover: bool,
    errors: Vec<Diag>,
}

/// The most syntax errors one file reports.
pub const MAX_ERRORS: usize = 50;

type PResult<T> = Result<T, Diag>;

/// The local name of the `os/exec` import that command literals use when the
/// file doesn't import it itself.
const CMD_PKG: &str = "alxexec";

pub fn parse(file: u32, toks: &[Token], next_id: &mut NodeId) -> PResult<Module> {
    let mut p = Parser::new(toks, next_id, vec![HashSet::new()]);
    p.setup(toks);
    p.finish(file)
}

/// Parse keeping going after a syntax error: one error per broken top-level
/// declaration (at most `MAX_ERRORS`), resuming at the next declaration keyword.
/// The first error equals the one `parse` returns. The module is partial when
/// the list is not empty (derives are not expanded).
pub fn parse_recovering(file: u32, toks: &[Token], next_id: &mut NodeId) -> (Module, Vec<Diag>) {
    let mut p = Parser::new(toks, next_id, vec![HashSet::new()]);
    p.recover = true;
    p.setup(toks);
    match p.finish(file) {
        Ok(m) => (m, std::mem::take(&mut p.errors)),
        Err(e) => {
            let mut errs = std::mem::take(&mut p.errors);
            errs.push(e);
            let m = Module { file, overflow: Overflow::Abort, external_test: false, imports: vec![], public: Default::default(), defs: vec![], structs: vec![], enums: vec![], refines: vec![], ifaces: vec![], consts: vec![], main: vec![], tests: vec![] };
            (m, errs)
        }
    }
}

/// One expression (the whole token stream), for code the checker writes as
/// text (`format`'s composite values: check.rs `fmt_text`). `locals` are the
/// names in scope.
pub fn parse_expr(toks: &[Token], next_id: &mut NodeId, locals: &[String]) -> PResult<Expr> {
    let mut p = Parser::new(toks, next_id, vec![locals.iter().cloned().collect()]);
    let e = p.expr()?;
    p.skip_newlines();
    if !matches!(p.peek(), Tok::Eof) {
        return Err(Diag::new(p.span(), format!("generated code: unexpected {}", describe(p.peek()))));
    }
    Ok(e)
}

impl<'a> Parser<'a> {
    fn setup(&mut self, toks: &[Token]) {
        // A file that imports os/exec itself: its command literals use that import.
        self.cmd_pkg = (0..toks.len()).find_map(|k| {
            let line_start = k == 0 || matches!(toks[k - 1].tok, Tok::Newline | Tok::Op(";"));
            match (&toks[k].tok, toks.get(k + 1).map(|t| &t.tok), toks.get(k + 2).map(|t| &t.tok)) {
                (Tok::Ident(kw), Some(Tok::Str(p)), _) if line_start && kw == "import" && p == "os/exec" => Some(default_import_name(p)),
                (Tok::Ident(kw), Some(Tok::Ident(a)), Some(Tok::Str(p))) if line_start && kw == "import" && p == "os/exec" => Some(a.clone()),
                _ => None,
            }
        });
    }

    fn finish(&mut self, file: u32) -> PResult<Module> {
        let mut m = self.module(file)?;
        // Command literals call into os/exec (also from inside `#{...}`).
        if let Some(span) = self.cmd_own {
            m.imports.push(Import { alias: Some(CMD_PKG.into()), path: "os/exec".into(), span });
        }
        Ok(m)
    }

    fn new(toks: &'a [Token], next_id: &'a mut NodeId, scopes: Vec<HashSet<String>>) -> Self {
        Parser {
            toks,
            pos: 0,
            next_id,
            scopes,
            in_cond: false,
            usings: vec![],
            extern_mode: false,
            cur_derives: vec![],
            cur_asn1: Default::default(),
            jobs: vec![],
            cur_data_name: None,
            djobs: vec![],
            xjobs: vec![],
            cur_transparent: false,
            cur_shareable: None,
            gjobs: vec![],
            cmd_pkg: None,
            cmd_own: None,
            recover: false,
            errors: vec![],
        }
    }
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }
    fn peek_at(&self, k: usize) -> &Tok {
        &self.toks[(self.pos + k).min(self.toks.len() - 1)].tok
    }
    fn span(&self) -> Span {
        self.toks[self.pos].span
    }
    fn prev_span(&self) -> Span {
        self.toks[self.pos.saturating_sub(1)].span
    }
    fn space_before(&self) -> bool {
        self.toks[self.pos].space_before
    }
    fn space_before_at(&self, k: usize) -> bool {
        self.toks[(self.pos + k).min(self.toks.len() - 1)].space_before
    }
    fn bump(&mut self) -> Token {
        let t = self.toks[self.pos].clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }
    fn is_op(&self, op: &str) -> bool {
        matches!(self.peek(), Tok::Op(o) if *o == op)
    }
    fn is_kw(&self, k: Kw) -> bool {
        matches!(self.peek(), Tok::Kw(x) if *x == k)
    }
    fn eat_op(&mut self, op: &str) -> bool {
        if self.is_op(op) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn expect_op(&mut self, op: &str) -> PResult<Span> {
        if self.is_op(op) {
            Ok(self.bump().span)
        } else {
            Err(Diag::new(self.span(), format!("expected `{op}`, found {}", describe(self.peek()))))
        }
    }
    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Tok::Newline) || self.is_op(";") {
            self.bump();
        }
    }
    fn id(&mut self) -> NodeId {
        *self.next_id += 1;
        *self.next_id
    }
    /// `"a #{x} b"` from its lexed pieces.
    fn interp(&mut self, pieces: Vec<IPiece>, sp: Span) -> PResult<Expr> {
        let mut parts = vec![];
        for p in pieces {
            match p {
                IPiece::Lit(s) => parts.push(InterpPart::Lit(s)),
                IPiece::Code(src, base) => parts.push(InterpPart::Expr(self.code_expr(&src, base, sp, "`#{...}`")?)),
            }
        }
        Ok(self.mk(ExprKind::Interp(parts), sp))
    }

    /// The expression in an embedded piece of source (`#{...}`).
    fn code_expr(&mut self, src: &str, base: u32, sp: Span, what: &str) -> PResult<Expr> {
        let toks = crate::lexer::lex_at(sp.file, src, base)?;
        let mut sub = Parser::new(&toks, self.next_id, self.scopes.clone());
        sub.cmd_pkg = self.cmd_pkg.clone();
        sub.skip_newlines();
        let e = sub.expr()?;
        sub.skip_newlines();
        if !matches!(sub.peek(), Tok::Eof) {
            return Err(Diag::new(sub.span(), format!("unexpected {} in {what}", describe(sub.peek()))));
        }
        self.cmd_own = self.cmd_own.or(sub.cmd_own);
        Ok(e)
    }

    fn mk(&mut self, kind: ExprKind, span: Span) -> Expr {
        Expr { id: self.id(), kind, span }
    }
    fn declare(&mut self, name: &str) {
        self.scopes.last_mut().unwrap().insert(name.to_string());
    }
    fn is_local(&self, name: &str) -> bool {
        self.scopes.iter().any(|s| s.contains(name))
    }

    // ---------- module ----------

    fn module(&mut self, file: u32) -> PResult<Module> {
        let mut m = Module { file, overflow: Overflow::Abort, external_test: false, imports: vec![], public: Default::default(), defs: vec![], structs: vec![], enums: vec![], refines: vec![], ifaces: vec![], consts: vec![], main: vec![], tests: vec![] };
        self.skip_newlines();
        while let Tok::Directive(name, arg) = self.peek().clone() {
            let sp = self.bump().span;
            match (name.as_str(), arg.as_str()) {
                ("overflow", "abort") => m.overflow = Overflow::Abort,
                ("overflow", "wrap") => m.overflow = Overflow::Wrap,
                ("overflow", "promote") => m.overflow = Overflow::Promote,
                // S11: an external test file (Go's `package x_test`).
                ("test", "external") => m.external_test = true,
                _ => return Err(Diag::new(sp, format!("unknown directive `#![{name}({arg})]`")).note("known: #![overflow(abort | wrap | promote)], #![test(external)]")),
            }
            self.skip_newlines();
        }
        loop {
            let start = self.pos;
            match self.top_decl(&mut m) {
                Ok(true) => break,
                Ok(false) => {}
                Err(e) => {
                    if !self.recover {
                        return Err(e);
                    }
                    self.errors.push(e);
                    if self.errors.len() >= MAX_ERRORS {
                        break;
                    }
                    self.resync(start);
                }
            }
        }
        if !self.errors.is_empty() {
            return Ok(m);
        }
        self.expand_derives(&mut m)?;
        Ok(m)
    }

    /// One top-level declaration or statement; `Ok(true)` at the end of the file.
    fn top_decl(&mut self, m: &mut Module) -> PResult<bool> {
            self.skip_newlines();
            match self.peek().clone() {
                Tok::Eof => return Ok(true),
                Tok::Kw(Kw::Require) => {
                    return Err(Diag::new(self.span(), "`require` is now `import \"path\"` (a package directory)"));
                }
                Tok::Ident(kw) if kw == "refine" && matches!(self.peek_at(1), Tok::Const(_)) && !self.is_local("refine") => {
                    let r = self.refine_def(&mut m.defs)?;
                    m.refines.push(r);
                }
                Tok::Ident(kw) if kw == "import" && !matches!(self.peek_at(1), Tok::Op(_) | Tok::Newline | Tok::Eof) && !self.is_local("import") => {
                    let sp = self.bump().span;
                    let alias = match self.peek().clone() {
                        Tok::Ident(a) => {
                            self.bump();
                            Some(a)
                        }
                        _ => None,
                    };
                    match self.bump().tok {
                        Tok::Str(s) => m.imports.push(Import { alias, path: s, span: sp.to(self.prev_span()) }),
                        _ => return Err(Diag::new(sp, "`import` takes a string path: `import \"geom\"`")),
                    }
                }
                Tok::Ident(kw) if matches!(kw.as_str(), "test" | "bench" | "example") && matches!(self.peek_at(1), Tok::Str(_)) && matches!(self.peek_at(2), Tok::Op("{")) && !self.is_local(&kw) => {
                    let start = self.bump().span;
                    let Tok::Str(name) = self.bump().tok else { unreachable!() };
                    let name_span = self.prev_span();
                    self.scopes.push(HashSet::new());
                    // `test "x" { |t| ... }` / `bench "x" { |b| ... }`: the body takes
                    // the package testing's T (or B); the front end gives its type.
                    let mut param = None;
                    let body = if kw != "example" && matches!(self.peek_at(1), Tok::Op("|")) && matches!(self.peek_at(2), Tok::Ident(_)) && matches!(self.peek_at(3), Tok::Op("|")) {
                        self.bump();
                        self.bump();
                        let psp = self.span();
                        let Tok::Ident(p) = self.bump().tok else { unreachable!() };
                        self.bump();
                        self.declare(&p);
                        param = Some(Param { name: p, ty: None, span: psp, default: None });
                        self.stmts_to_brace()
                    } else {
                        self.braced_stmts()
                    };
                    self.scopes.pop();
                    let body = body?;
                    let kind = match kw.as_str() {
                        "test" => TestKind::Test,
                        "bench" => TestKind::Bench,
                        _ => TestKind::Example,
                    };
                    let mut outputs = None;
                    if kind == TestKind::Example {
                        if !matches!(self.peek(), Tok::Ident(o) if o == "outputs") {
                            return Err(Diag::new(self.span(), "an `example` ends with `outputs \"expected text\"`"));
                        }
                        self.bump();
                        match self.bump().tok {
                            Tok::Str(s) => outputs = Some(s),
                            _ => return Err(Diag::new(self.prev_span(), "`outputs` takes a string literal")),
                        }
                    }
                    let span = start.to(self.prev_span());
                    let def = Def { public: false, using: self.usings.clone(), name: name.clone(), span, tparams: vec![], name_span, params: param.into_iter().collect(), ret: None, fallible: true, errs: None, pure: false, track_caller: false, ffi: None, body };
                    m.tests.push(TestDecl { kind, name, span, outputs, def });
                }
                Tok::Ident(kw) if kw == "pub" && !matches!(self.peek_at(1), Tok::Op(_) | Tok::Newline | Tok::Eof) && !self.is_local("pub") => {
                    // `pub def`, `pub struct`, `pub enum`, `pub error`, `pub interface`, `pub NAME = ...`
                    self.bump();
                    let before = (m.defs.len(), m.structs.len(), m.enums.len(), m.ifaces.len(), m.consts.len());
                    match self.peek().clone() {
                        Tok::Attr(_) | Tok::Kw(Kw::Def) | Tok::Kw(Kw::Fn) => {
                            let mut d = self.def()?;
                            d.public = true;
                            m.defs.push(d);
                        }
                        Tok::Ident(kw) if kw == "extern" && matches!(self.peek_at(1), Tok::Kw(Kw::Def)) => {
                            let mut d = self.extern_def()?;
                            d.public = true;
                            m.defs.push(d);
                        }
                        Tok::Kw(Kw::Struct) => {
                            let s = self.struct_def(&mut m.defs)?;
                            self.mark_pub(&s.name);
                            m.public.insert(s.name.clone());
                            m.structs.push(s);
                        }
                        Tok::Kw(Kw::Enum) => {
                            let e = self.enum_def(&mut m.defs)?;
                            self.mark_pub(&e.name);
                            m.public.insert(e.name.clone());
                            m.enums.push(e);
                        }
                        Tok::Ident(kw) if kw == "error" => {
                            let mut e = self.enum_def(&mut m.defs)?;
                            e.error = true;
                            m.public.insert(e.name.clone());
                            m.enums.push(e);
                        }
                        Tok::Kw(Kw::Interface) => {
                            let i = self.iface_def(&mut m.defs)?;
                            m.public.insert(i.name.clone());
                            m.ifaces.push(i);
                        }
                        Tok::Ident(kw) if kw == "refine" => {
                            let r = self.refine_def(&mut m.defs)?;
                            m.public.insert(r.name.clone());
                            m.refines.push(r);
                        }
                        Tok::Const(name) if matches!(self.peek_at(1), Tok::Op("=") | Tok::Op(":")) => {
                            let sp = self.bump().span;
                            let ty = if self.eat_op(":") { Some(self.type_expr()?) } else { None };
                            self.expect_op("=")?;
                            self.skip_line_continuation();
                            let value = self.expr()?;
                            m.public.insert(name.clone());
                            self.top_value(m, name, sp, ty, value);
                        }
                        t => return Err(Diag::new(self.span(), format!("`pub` goes before a declaration, found {}", describe(&t)))),
                    }
                }
                Tok::Attr(a) if a.starts_with("link(") => {
                    // `#[link("sqlite3")] extern def ...`: the C library the function is in.
                    let sp = self.bump().span;
                    let lib = a["link(".len()..].strip_suffix(')').map(str::trim).and_then(|x| x.strip_prefix('"')).and_then(|x| x.strip_suffix('"')).filter(|x| !x.is_empty() && !x.contains(['"', '\0'])).map(str::to_string);
                    let Some(lib) = lib else {
                        return Err(Diag::new(sp, "`#[link(\"name\")]`: the library's name, as `-l` takes it (`#[link(\"sqlite3\")]`)"));
                    };
                    self.skip_newlines();
                    let public = matches!(self.peek(), Tok::Ident(p) if p == "pub");
                    if public {
                        self.bump();
                    }
                    if !matches!(self.peek(), Tok::Ident(kw) if kw == "extern") {
                        return Err(Diag::new(sp, "`#[link(...)]` goes before an `extern def`"));
                    }
                    let mut d = self.extern_def()?;
                    d.public = public;
                    d.ffi = d.ffi.map(|sym| crate::ast::ffi_with_lib(&lib, &sym));
                    m.defs.push(d);
                }
                Tok::Attr(a) if is_embed_attr(&a) => {
                    let (c, public) = self.embed_const()?;
                    if public {
                        m.public.insert(c.name.clone());
                    }
                    m.consts.push(c);
                }
                Tok::Attr(_) if self.attrs_precede_type() => self.type_attrs()?,
                Tok::Attr(_) | Tok::Kw(Kw::Def) | Tok::Kw(Kw::Fn) => m.defs.push(self.def()?),
                Tok::Ident(kw) if kw == "extern" && matches!(self.peek_at(1), Tok::Kw(Kw::Def)) && !self.is_local("extern") => m.defs.push(self.extern_def()?),
                Tok::Kw(Kw::Struct) => {
                    let s = self.struct_def(&mut m.defs)?;
                    m.structs.push(s);
                }
                Tok::Kw(Kw::Enum) => {
                    let e = self.enum_def(&mut m.defs)?;
                    m.enums.push(e);
                }
                Tok::Ident(kw) if kw == "error" && matches!(self.peek_at(1), Tok::Const(_)) && !self.is_local("error") => {
                    let mut e = self.enum_def(&mut m.defs)?;
                    e.error = true;
                    m.enums.push(e);
                }
                Tok::Kw(Kw::Interface) => {
                    let i = self.iface_def(&mut m.defs)?;
                    m.ifaces.push(i);
                }
                Tok::Const(name) if matches!(self.peek_at(1), Tok::Op("=") | Tok::Op(":")) => {
                    // `NAME = expr` / `NAME: Type = expr`: a constant.
                    let sp = self.bump().span;
                    let ty = if self.eat_op(":") { Some(self.type_expr()?) } else { None };
                    self.expect_op("=")?;
                    self.skip_line_continuation();
                    let value = self.expr()?;
                    self.top_value(m, name, sp, ty, value);
                }
                Tok::Directive(..) => return Err(Diag::new(self.span(), "directives must come first in the file")),
                _ => {
                    let s = self.stmt()?;
                    m.main.push(s);
                }
            }
        Ok(false)
    }

    /// Error recovery: move to the next top-level declaration keyword that starts a
    /// line outside every brace, scanning from the declaration that failed.
    fn resync(&mut self, start: usize) {
        let mut depth = 0i32;
        let from = self.pos.max(start + 1);
        let mut k = start;
        while k < self.toks.len() {
            let t = &self.toks[k].tok;
            match t {
                Tok::Op("{") => depth += 1,
                Tok::Op("}") => depth = (depth - 1).max(0),
                Tok::Eof => break,
                _ => {}
            }
            if k >= from && depth == 0 && k > start && matches!(self.toks[k - 1].tok, Tok::Newline | Tok::Op(";")) {
                let decl = match t {
                    Tok::Kw(Kw::Def | Kw::Fn | Kw::Struct | Kw::Enum | Kw::Interface) | Tok::Attr(_) | Tok::Directive(..) => true,
                    Tok::Ident(kw) => matches!(kw.as_str(), "import" | "pub" | "error" | "refine" | "test" | "bench" | "example" | "extern"),
                    Tok::Const(_) => matches!(self.toks.get(k + 1).map(|t| &t.tok), Some(Tok::Op("=") | Tok::Op(":"))),
                    _ => false,
                };
                if decl {
                    break;
                }
            }
            k += 1;
        }
        self.pos = k.min(self.toks.len().saturating_sub(1));
        self.scopes.truncate(1);
        self.in_cond = false;
        self.extern_mode = false;
        self.cur_derives.clear();
        self.cur_asn1 = Default::default();
        self.cur_data_name = None;
        self.cur_transparent = false;
    }

    /// `#[embed("pattern", ...)] [pub] NAME: Type` (Go's `//go:embed`): a
    /// constant whose value is the matching files, read by the front end.
    fn embed_const(&mut self) -> PResult<(ConstDef, bool)> {
        let Tok::Attr(a) = self.peek().clone() else { unreachable!() };
        let asp = self.bump().span;
        let mut patterns = embed_patterns(&a).map_err(|m| Diag::new(asp, m))?;
        self.skip_newlines();
        // Several `#[embed]` lines add up, as Go's `//go:embed` lines do.
        while let Tok::Attr(a) = self.peek().clone() {
            if !is_embed_attr(&a) {
                break;
            }
            let sp = self.bump().span;
            patterns.extend(embed_patterns(&a).map_err(|m| Diag::new(sp, m))?);
            self.skip_newlines();
        }
        let public = matches!(self.peek(), Tok::Ident(p) if p == "pub");
        if public {
            self.bump();
        }
        let Tok::Const(name) = self.peek().clone() else {
            return Err(Diag::new(self.span(), "`#[embed(...)]` goes before a constant declaration: `NAME: Str`, `NAME: [Byte]` or `NAME: embed.FS`"));
        };
        let sp = self.bump().span;
        if !self.is_op(":") {
            return Err(Diag::new(self.span(), "an embedded constant needs its type: `NAME: Str`, `NAME: [Byte]` or `NAME: embed.FS`"));
        }
        self.bump();
        let ty = self.type_expr()?;
        if self.is_op("=") {
            return Err(Diag::new(self.span(), "an embedded constant has no `= value`: the files are its value"));
        }
        let value = Expr { id: self.id(), kind: ExprKind::Bool(false), span: sp };
        let embed = Some(Embed { patterns, span: asp, files: Default::default() });
        Ok((ConstDef { name, span: sp, ty: Some(ty), value, embed, var: false }, public))
    }

    /// `NAME = expr` at the top level: a constant, or (R11) a package-level
    /// `Atomic[T]` / `Mutex[T]`, whose value a def `__init_NAME` computes.
    fn top_value(&mut self, m: &mut Module, name: String, sp: Span, ty: Option<TypeExpr>, value: Expr) {
        let var = is_sync_decl(ty.as_ref(), &value);
        if var {
            let body = vec![Stmt { span: value.span, kind: StmtKind::Expr(value.clone()) }];
            m.defs.push(Def { name: var_init_name(&name), span: sp, tparams: vec![], name_span: sp, params: vec![], ret: ty.clone(), fallible: false, public: false, using: self.usings.clone(), errs: None, pure: false, track_caller: false, ffi: None, body });
        }
        m.consts.push(ConstDef { name, span: sp, ty, value, var, embed: None });
    }

    /// Are the `#[...]` attributes at the cursor followed by a struct or enum?
    fn attrs_precede_type(&self) -> bool {
        let mut k = 0;
        while matches!(self.peek_at(k), Tok::Attr(_) | Tok::Newline) {
            k += 1;
        }
        if matches!(self.peek_at(k), Tok::Ident(p) if p == "pub") {
            k += 1;
        }
        matches!(self.peek_at(k), Tok::Kw(Kw::Struct) | Tok::Kw(Kw::Enum) | Tok::Kw(Kw::Interface))
    }

    /// `#[derive(Json, Eq)]` before a struct or enum.
    fn type_attrs(&mut self) -> PResult<()> {
        while let Tok::Attr(a) = self.peek().clone() {
            let sp = self.bump().span;
            if a.trim_start().starts_with("asn1") {
                crate::derive_asn1::apply_asn1_attr(&a, sp, &mut self.cur_asn1)?;
                self.skip_newlines();
                continue;
            }
            if a.trim() == "shareable" {
                self.cur_shareable = Some(sp);
                self.skip_newlines();
                continue;
            }
            if crate::derive::is_transparent_attr(&a, sp)? {
                self.cur_transparent = true;
                self.skip_newlines();
                continue;
            }
            if a.trim_start().starts_with("data") {
                let (rename, _) = crate::derive::parse_data_attr(&a, sp)?;
                self.cur_data_name = rename;
                self.skip_newlines();
                continue;
            }
            match crate::derive::derive_names(&a, sp)? {
                Some(names) => self.cur_derives.extend(names),
                None => return Err(Diag::new(sp, format!("unknown attribute `#[{a}]` on a type")).note("known: #[derive(Json)], #[derive(Data)], #[data(\"pkg.Name\")], #[shareable] (interfaces); #[field(...)], #[json(...)] and #[data(...)] go on fields and variants")),
            }
            self.skip_newlines();
        }
        Ok(())
    }

    fn mark_pub(&mut self, name: &str) {
        for j in self.jobs.iter_mut().filter(|j| j.name == name) {
            j.public = true;
        }
    }

    /// Write the code of every recorded derive and parse it in as methods.
    fn expand_derives(&mut self, m: &mut Module) -> PResult<()> {
        self.expand_data_derives(m)?;
        self.expand_gob_derives(m)?;
        self.expand_xml_derives(m)?;
        if self.jobs.is_empty() {
            return Ok(());
        }
        let jobs = std::mem::take(&mut self.jobs);
        // The local name of an import the generated code needs (added if the file has none).
        let mut alias_of = |m: &mut Module, path: &str, own: &str, kind: &str| -> String {
            if !jobs.iter().any(|j| j.derive == kind) {
                return String::new();
            }
            match m.imports.iter().find(|i| i.path == path) {
                Some(i) => import_name(i),
                None => {
                    m.imports.push(Import { alias: Some(own.into()), path: path.into(), span: jobs[0].span });
                    own.to_string()
                }
            }
        };
        let alias = alias_of(m, "encoding/json", "alxjson", "Json");
        let quick = alias_of(m, "testing/quick", "alxquick", "Arbitrary");
        let rand = alias_of(m, "math/rand", "alxrand", "Arbitrary");
        let sql_alias = alias_of(m, "database/sql", "alxsql", "Row");
        let asn1_alias = alias_of(m, "encoding/asn1", "alxasn1", "Asn1");
        let imports: std::collections::HashMap<String, String> = m.imports.iter().map(|i| (import_name(i), i.path.clone())).collect();
        let mut asn1_types = crate::derive::ModuleTypes { local: Default::default(), enums: Default::default(), protocol: Default::default() };
        for s in &m.structs {
            asn1_types.local.insert(s.name.clone(), jobs.iter().any(|j| j.name == s.name && j.derive == "Asn1"));
        }
        for e in &m.enums {
            asn1_types.local.insert(e.name.clone(), jobs.iter().any(|j| j.name == e.name && j.derive == "Asn1"));
            asn1_types.enums.insert(e.name.clone());
        }
        let mut types = crate::derive::ModuleTypes { local: Default::default(), enums: Default::default(), protocol: Default::default() };
        for s in &m.structs {
            types.local.insert(s.name.clone(), jobs.iter().any(|j| j.name == s.name && j.derive == "Json"));
        }
        for e in &m.enums {
            types.local.insert(e.name.clone(), jobs.iter().any(|j| j.name == e.name && j.derive == "Json"));
            types.enums.insert(e.name.clone());
        }
        for d in &m.defs {
            if let Some((ty, meth)) = d.name.rsplit_once('.') {
                if matches!(meth, "json_str" | "json_enc" | "marshal_json" | "marshal_text") {
                    types.protocol.insert(ty.to_string());
                }
            }
        }
        // encoding/json/v2's methods, for files that use v2 (derive_json2.rs).
        let wants_v2 = jobs.iter().any(|j| j.derive == "Json") && m.imports.iter().any(|i| i.path == "encoding/json/v2" || i.path == "encoding/json/jsontext");
        let mut v2_texts: std::collections::HashMap<String, String> = Default::default();
        let mut v2_only: Vec<String> = vec![];
        if wants_v2 {
            let mut alias_of2 = |m: &mut Module, path: &str, fallback: &str| match m.imports.iter().find(|i| i.path == path) {
                Some(i) => import_name(i),
                None => {
                    m.imports.push(Import { alias: Some(fallback.into()), path: path.into(), span: jobs[0].span });
                    fallback.to_string()
                }
            };
            let j2 = alias_of2(m, "encoding/json/v2", "alxjson2");
            let jt = alias_of2(m, "encoding/json/jsontext", "alxjsontext");
            v2_only.push(format!("{jt}.Value"));
            let go_names: std::collections::HashMap<String, String> = jobs.iter().filter(|j| j.derive == "Json").filter_map(|j| j.go_name.clone().map(|g| (j.name.clone(), g))).collect();
            let cx = crate::derive_json2::Ctx { j: &j2, t: &jt, types: &types, go_names: &go_names };
            for job in jobs.iter().filter(|j| j.derive == "Json") {
                let gn = job.go_name.clone().unwrap_or_else(|| job.name.clone());
                v2_texts.insert(job.name.clone(), crate::derive_json2::json2_source(job, &gn, &cx)?);
            }
        }
        for job in &jobs {
            let text = if job.derive == "Asn1" { crate::derive_asn1::asn1_source(job, &asn1_alias, &imports, &asn1_types)? } else if job.derive == "Row" { crate::derive::row_source(job, &sql_alias)? } else if job.derive == "Arbitrary" { crate::derive::arbitrary_source(job, &quick, &rand)? } else { crate::derive::json_source(job, &alias, &types, wants_v2, &v2_only, &jobs)? };
            // One struct body: v1's methods, then v2's (both texts are `struct Name { ... }`).
            let text = match v2_texts.get(&job.name).filter(|_| job.derive == "Json") {
                Some(v2) => {
                    let v1 = &text[..text.rfind('}').unwrap()];
                    format!("{v1}{}", &v2[v2.find('\n').unwrap() + 1..])
                }
                None => text,
            };
            if std::env::var("ALX_DERIVE_DEBUG").is_ok() {
                eprintln!("{text}");
            }
            let mut toks = crate::lexer::lex(job.span.file, &text).map_err(|d| Diag::new(job.span, format!("derive({}) on `{}` made code that doesn't lex: {}\n{text}", job.derive, job.name, d.msg)))?;
            for t in &mut toks {
                t.span = job.span;
            }
            let mut sub = Parser::new(&toks, self.next_id, vec![HashSet::new()]);
            sub.skip_newlines();
            let mut defs = vec![];
            sub.struct_def(&mut defs).map_err(|d| Diag::new(job.span, format!("derive({}) on `{}` made code that doesn't parse: {}\n{text}", job.derive, job.name, d.msg)))?;
            for d in &mut defs {
                d.span = job.span;
            }
            m.defs.extend(defs);
        }
        Ok(())
    }

    /// Write the code of every derive(Xml) and parse it in as methods.
    fn expand_xml_derives(&mut self, m: &mut Module) -> PResult<()> {
        if self.xjobs.is_empty() {
            return Ok(());
        }
        let xjobs = std::mem::take(&mut self.xjobs);
        // The local name of the `encoding/xml` import (added if the file has none).
        let alias = match m.imports.iter().find(|i| i.path == "encoding/xml") {
            Some(i) => import_name(i),
            None => {
                m.imports.push(Import { alias: Some("alxxml".into()), path: "encoding/xml".into(), span: xjobs[0].job.span });
                "alxxml".to_string()
            }
        };
        let by_name: std::collections::HashMap<String, &crate::derive_xml::XmlJob> = xjobs.iter().map(|j| (j.job.name.clone(), j)).collect();
        for xj in &xjobs {
            let job = &xj.job;
            let text = crate::derive_xml::xml_source(xj, &alias, &by_name)?;
            if std::env::var("ALX_DERIVE_DEBUG").is_ok() {
                eprintln!("{text}");
            }
            let mut toks = crate::lexer::lex(job.span.file, &text).map_err(|d| Diag::new(job.span, format!("derive(Xml) on `{}` made code that doesn't lex: {}\n{text}", job.name, d.msg)))?;
            for t in &mut toks {
                t.span = job.span;
            }
            let mut sub = Parser::new(&toks, self.next_id, vec![HashSet::new()]);
            sub.skip_newlines();
            let mut defs = vec![];
            sub.struct_def(&mut defs).map_err(|d| Diag::new(job.span, format!("derive(Xml) on `{}` made code that doesn't parse: {}\n{text}", job.name, d.msg)))?;
            for d in &mut defs {
                d.span = job.span;
            }
            m.defs.extend(defs);
        }
        Ok(())
    }

    /// Write the code of every derive(Gob) and parse it in as methods.
    fn expand_gob_derives(&mut self, m: &mut Module) -> PResult<()> {
        if self.gjobs.is_empty() {
            return Ok(());
        }
        let gjobs = std::mem::take(&mut self.gjobs);
        let alias = match m.imports.iter().find(|i| i.path == "encoding/gob") {
            Some(i) => import_name(i),
            None => {
                m.imports.push(Import { alias: Some("alxgob".into()), path: "encoding/gob".into(), span: gjobs[0].job.span });
                "alxgob".to_string()
            }
        };
        let mut locals = std::collections::HashMap::new();
        for s in &m.structs {
            locals.insert(s.name.clone(), crate::derive_gob::LocalType { is_enum: false, derived: gjobs.iter().any(|j| j.job.name == s.name) });
        }
        for e in &m.enums {
            locals.insert(e.name.clone(), crate::derive_gob::LocalType { is_enum: true, derived: gjobs.iter().any(|j| j.job.name == e.name) });
        }
        for gj in &gjobs {
            let text = crate::derive_gob::source(gj, &alias, &locals)?;
            if std::env::var("ALX_DERIVE_DEBUG").is_ok() {
                eprintln!("{text}");
            }
            let job = &gj.job;
            let mut toks = crate::lexer::lex(job.span.file, &text).map_err(|d| Diag::new(job.span, format!("derive(Gob) on `{}` made code that doesn't lex: {}\n{text}", job.name, d.msg)))?;
            for t in &mut toks {
                t.span = job.span;
            }
            let mut sub = Parser::new(&toks, self.next_id, vec![HashSet::new()]);
            sub.skip_newlines();
            let mut defs = vec![];
            sub.struct_def(&mut defs).map_err(|d| Diag::new(job.span, format!("derive(Gob) on `{}` made code that doesn't parse: {}\n{text}", job.name, d.msg)))?;
            for d in &mut defs {
                d.span = job.span;
                d.public = true;
            }
            m.defs.extend(defs);
        }
        Ok(())
    }

    /// Write the code of every derive(Data) and parse it in as methods.
    fn expand_data_derives(&mut self, m: &mut Module) -> PResult<()> {
        if self.djobs.is_empty() {
            return Ok(());
        }
        let djobs = std::mem::take(&mut self.djobs);
        // The local name of the `dyn` import (added if the file has none).
        let alias = match m.imports.iter().find(|i| i.path == "dyn") {
            Some(i) => import_name(i),
            None => {
                m.imports.push(Import { alias: Some("alxdyn".into()), path: "dyn".into(), span: djobs[0].job.span });
                "alxdyn".to_string()
            }
        };
        let mut go_names = std::collections::HashMap::new();
        let mut underived = std::collections::HashMap::new();
        for s in &m.structs {
            underived.insert(s.name.clone(), djobs.iter().any(|j| j.job.name == s.name));
        }
        for e in &m.enums {
            underived.insert(e.name.clone(), djobs.iter().any(|j| j.job.name == e.name));
        }
        for j in &djobs {
            if let Some(g) = &j.go_name {
                go_names.insert(j.job.name.clone(), g.clone());
            }
        }
        // Which types get from_data: a fixpoint, since a type comes back only
        // if the derived types of its fields do (non-deriving local types never).
        let mut from_ok: std::collections::HashMap<String, bool> = underived.iter().map(|(n, d)| (n.clone(), *d)).collect();
        loop {
            let mut changed = false;
            for j in &djobs {
                if from_ok.get(&j.job.name) == Some(&true) && !crate::derive_data::job_fromable(j, &from_ok) {
                    from_ok.insert(j.job.name.clone(), false);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        for tj in &djobs {
            let fo = from_ok.get(&tj.job.name) == Some(&true);
            let text = crate::derive_data::source(tj, &alias, &go_names, &underived, fo)?;
            if std::env::var("ALX_DERIVE_DEBUG").is_ok() {
                eprintln!("{text}");
            }
            let job = &tj.job;
            let mut toks = crate::lexer::lex(job.span.file, &text).map_err(|d| Diag::new(job.span, format!("derive(Data) on `{}` made code that doesn't lex: {}\n{text}", job.name, d.msg)))?;
            for t in &mut toks {
                t.span = job.span;
            }
            let mut sub = Parser::new(&toks, self.next_id, vec![HashSet::new()]);
            sub.skip_newlines();
            let mut defs = vec![];
            sub.struct_def(&mut defs).map_err(|d| Diag::new(job.span, format!("derive(Data) on `{}` made code that doesn't parse: {}\n{text}", job.name, d.msg)))?;
            for d in &mut defs {
                d.span = job.span;
                d.public = true;
            }
            m.defs.extend(defs);
        }
        Ok(())
    }

    fn def(&mut self) -> PResult<Def> {
        self.def_in(None)
    }

    /// `extern def name(p: T, ...) -> R [= "symbol"]`: a C function.
    fn extern_def(&mut self) -> PResult<Def> {
        let sp = self.bump().span; // `extern`
        if !self.is_kw(Kw::Def) {
            return Err(Diag::new(sp, "`extern` goes before `def`: `extern def write(fd: I32, buf: [Byte], n: Int) -> Int`"));
        }
        self.extern_mode = true;
        let d = self.def_sig(None);
        self.extern_mode = false;
        let d = d?;
        let sym = d.ffi.clone().unwrap_or_else(|| d.name.clone());
        if d.fallible || !d.tparams.is_empty() {
            return Err(Diag::new(d.name_span, "an `extern def` can't be fallible or generic"));
        }
        Ok(Def { public: false, using: self.usings.clone(), name: d.name, span: sp.to(self.prev_span()), tparams: vec![], name_span: d.name_span, params: d.params, ret: d.ret, fallible: false, errs: None, pure: d.pure, track_caller: d.track_caller, ffi: Some(sym), body: vec![] })
    }

    /// A def; inside `struct Owner { }` it is a method taking `self`.
    fn def_in(&mut self, owner: Option<&str>) -> PResult<Def> {
        let d = self.def_sig(owner.map(|o| (o, false)))?;
        let Some(body) = d.body else { unreachable!("a body is required outside interfaces") };
        Ok(Def { public: d.public, using: self.usings.clone(), name: d.name, span: d.span, tparams: d.tparams, name_span: d.name_span, params: d.params, ret: d.ret, fallible: d.fallible, errs: d.errs, pure: d.pure, track_caller: d.track_caller, ffi: d.ffi, body })
    }

    /// A def's signature and body. In an interface (`owner.1`), `self` is
    /// generic and the body may be left out.
    fn def_sig(&mut self, owner: Option<(&str, bool)>) -> PResult<DefSig> {
        let in_iface = owner.is_some_and(|o| o.1);
        let owner = owner.map(|o| o.0);
        let start = self.span();
        let mut pure = false;
        let mut track_caller = false;
        while let Tok::Attr(a) = self.peek().clone() {
            let sp = self.bump().span;
            match a.as_str() {
                "pure" => pure = true,
                "track_caller" if self.extern_mode => return Err(Diag::new(sp, "an `extern def` can't be `#[track_caller]`")),
                "track_caller" => track_caller = true,
                _ => return Err(Diag::new(sp, format!("unknown attribute `#[{a}]`"))),
            }
            self.skip_newlines();
        }
        // `#[track_caller] pub def`: the attributes may come before `pub` too.
        let public = !in_iface && start != self.span() && matches!(self.peek(), Tok::Ident(p) if p == "pub") && matches!(self.peek_at(1), Tok::Kw(Kw::Def) | Tok::Kw(Kw::Fn));
        if public {
            self.bump();
        }
        if self.is_kw(Kw::Fn) {
            // `fn` / `ƒ`: a pure def
            pure = true;
        } else if !self.is_kw(Kw::Def) {
            return Err(Diag::new(self.span(), "expected `def` after attribute"));
        }
        self.bump();
        // `def self.name(...)` inside a type: a static method (no receiver), called `Type.name(...)`.
        let is_static = owner.is_some() && !in_iface && matches!(self.peek(), Tok::Ident(s) if s == "self") && matches!(self.peek_at(1), Tok::Op("."));
        if is_static {
            self.bump();
            self.bump();
        }
        let name_span = self.span();
        let name = match self.bump().tok {
            Tok::Ident(n) => n,
            // A keyword names a method (Ruby's `def next`): `e.next` can't be read as one.
            Tok::Kw(k) if owner.is_some() => kw_name(k).to_string(),
            Tok::Kw(k) => return Err(reserved(name_span, k, "a function")),
            // Operators, inside a struct: `def +(o)`, `def ==(o)`, `def <=>(o)`, `def [](i)`.
            Tok::Op(op @ ("+" | "-" | "*" | "/" | "%" | "==" | "<=>")) if owner.is_some() => op.to_string(),
            Tok::Op("[") if owner.is_some() && self.eat_op("]") => "[]".to_string(),
            t => return Err(Diag::new(name_span, format!("expected a method name, found {}", describe(&t)))),
        };
        let tparams = self.tparams()?;
        self.scopes.push(HashSet::new());
        let mut params = vec![];
        if let Some(o) = owner.filter(|_| !is_static) {
            let t = TypeExpr::Named(o.to_string(), name_span);
            let ty = if name.ends_with('!') { TypeExpr::Array(Box::new(t), name_span) } else { t };
            self.declare("self");
            params.push(Param { name: "self".into(), ty: if in_iface { None } else { Some(ty) }, span: name_span, default: None });
        }
        let name = match owner {
            Some(o) => method_name(o, &name),
            None => name,
        };
        if self.is_op("(") && !self.space_before() {
            self.bump();
            let mut defaults_at = vec![];
            while !self.is_op(")") {
                let sp = self.span();
                let pname = match self.bump().tok {
                    Tok::Ident(n) => n,
                    t => return Err(Diag::new(sp, format!("expected a parameter name, found {}", describe(&t)))),
                };
                let ty = if self.eat_op(":") { Some(self.type_expr()?) } else { None };
                // S8: `name: T = default`.
                let default = if self.is_op("=") {
                    let esp = self.bump().span;
                    if ty.is_none() {
                        return Err(Diag::new(esp, format!("a parameter with a default needs a type: `{pname}: Type = value`")));
                    }
                    if self.extern_mode {
                        return Err(Diag::new(esp, "an `extern def` can't have default values"));
                    }
                    if in_iface {
                        return Err(Diag::new(esp, "an interface method can't have default values").note("an implementor's own method may; calls through the interface pass every argument"));
                    }
                    let from = self.pos;
                    let e = self.ternary()?;
                    defaults_at.push((params.len(), from, self.pos));
                    Some(Box::new(e))
                } else {
                    None
                };
                self.declare(&pname);
                params.push(Param { name: pname, ty, span: sp, default });
                if !self.eat_op(",") {
                    break;
                }
            }
            self.expect_op(")")?;
            self.check_defaults(&params, &defaults_at)?;
        }
        let (mut ret, mut fallible, mut errs) = (None, false, None);
        if self.eat_op("->") {
            match self.type_expr()? {
                TypeExpr::Result(t, e, _) => {
                    fallible = true;
                    errs = e;
                    ret = Some(*t);
                }
                t => ret = Some(t),
            }
            if self.is_op("!") {
                return Err(Diag::new(self.span(), "a fallible return type is now written `-> ~T` (or `-> ~T<ErrorType>`)"));
            }
        } else if self.is_op("~") {
            // `def f ~ { }`: fallible, returning nothing.
            self.bump();
            fallible = true;
            ret = None;
        }
        let mut ffi = None;
        let body = if self.extern_mode {
            if self.eat_op("=") {
                match self.bump().tok {
                    Tok::Str(s) => ffi = Some(s),
                    _ => return Err(Diag::new(self.prev_span(), "`extern def f(...) = \"symbol\"`: the link name is a string literal")),
                }
            }
            if self.is_op("{") {
                return Err(Diag::new(self.span(), "an `extern def` has no body: it is a C function"));
            }
            None
        } else if in_iface && !self.is_op("{") {
            None
        } else {
            Some(self.braced_stmts()?)
        };
        self.scopes.pop();
        Ok(DefSig { name, span: start.to(self.prev_span()), tparams, name_span, params, ret, fallible, errs, pure, track_caller, public, ffi, body })
    }

    /// S8: defaults use no parameters (they are evaluated at the call site,
    /// without the call's other arguments), and a required parameter can't
    /// follow one with a default (except a last function parameter, which
    /// a block fills). `at`: each default's parameter and token range.
    fn check_defaults(&self, params: &[Param], at: &[(usize, usize, usize)]) -> PResult<()> {
        for &(k, from, to) in at {
            for i in from..to {
                let Tok::Ident(n) = &self.toks[i].tok else { continue };
                let after_dot = i > from && matches!(self.toks[i - 1].tok, Tok::Op(".") | Tok::Op("&."));
                let kw = matches!(self.toks.get(i + 1).map(|t| &t.tok), Some(Tok::Op(":"))) && !matches!(self.toks.get(i + 2).map(|t| &t.tok), Some(Tok::Op(":")));
                if !after_dot && !kw && params.iter().any(|p| &p.name == n) {
                    return Err(Diag::new(self.toks[i].span, format!("the default of `{}` uses parameter `{n}`", params[k].name)).note("defaults are evaluated at the call site, without the call's other arguments (S8)"));
                }
            }
        }
        if let Some(first) = params.iter().position(|p| p.default.is_some()) {
            let last = params.len() - 1;
            for (i, p) in params.iter().enumerate().skip(first) {
                let block = i == last && matches!(p.ty, Some(TypeExpr::Fn(..)));
                if p.default.is_none() && !block {
                    return Err(Diag::new(p.span, format!("parameter `{}` needs a default: it follows `{}`, which has one", p.name, params[first].name)).note("parameters with defaults come last (a last function parameter, filled by a block, may follow them)"));
                }
            }
        }
        Ok(())
    }

    /// `[T, U: Shape, N: like Int]` after a type or def name (no space before `[`).
    fn tparams(&mut self) -> PResult<Vec<TParam>> {
        let mut out = vec![];
        if !(self.is_op("[") && !self.space_before()) {
            return Ok(out);
        }
        self.bump();
        loop {
            let sp = self.span();
            let name = match self.bump().tok {
                Tok::Const(n) => n,
                t => return Err(Diag::new(sp, format!("expected a type parameter (capitalized), found {}", describe(&t)))),
            };
            let bound = if self.eat_op(":") {
                let bsp = self.span();
                match self.bump().tok {
                    Tok::Ident(l) if l == "like" => match self.bump().tok {
                        Tok::Const(t) => Some(Bound::Like(t)),
                        t => return Err(Diag::new(bsp, format!("expected a type after `like`, found {}", describe(&t)))),
                    },
                    Tok::Const(i) => Some(Bound::Iface(i)),
                    // `io.Reader`: an interface from an imported package.
                    Tok::Ident(pkg) if self.is_op(".") && matches!(self.peek_at(1), Tok::Const(_)) => {
                        self.bump();
                        match self.bump().tok {
                            Tok::Const(i) => Some(Bound::Iface(format!("{pkg}.{i}"))),
                            _ => unreachable!(),
                        }
                    }
                    t => return Err(Diag::new(bsp, format!("expected a bound (an interface, or `like Int`), found {}", describe(&t)))),
                }
            } else {
                None
            };
            out.push(TParam { name, bound, span: sp });
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op("]")?;
        Ok(out)
    }

    /// `refine Name for Type { def ... }`
    fn refine_def(&mut self, defs: &mut Vec<Def>) -> PResult<RefineDef> {
        let start = self.bump().span;
        let sp = self.span();
        let name = match self.bump().tok {
            Tok::Const(n) => n,
            t => return Err(Diag::new(sp, format!("expected a refinement name (capitalized), found {}", describe(&t)))),
        };
        let rtparams = self.tparams()?;
        if !self.is_kw(Kw::For) {
            return Err(Diag::new(self.span(), "expected `for`: `refine Name for Type { ... }`"));
        }
        self.bump();
        let target = self.type_expr()?;
        let tshow = texpr_word(&target);
        self.expect_op("{")?;
        let mut methods = vec![];
        // The refinement's own methods see each other.
        self.usings.push(name.clone());
        loop {
            self.skip_newlines();
            if self.eat_op("}") {
                break;
            }
            let public = if matches!(self.peek(), Tok::Ident(p) if p == "pub") {
                self.bump();
                true
            } else {
                false
            };
            if !matches!(self.peek(), Tok::Attr(_) | Tok::Kw(Kw::Def) | Tok::Kw(Kw::Fn)) {
                self.usings.pop();
                return Err(Diag::new(self.span(), format!("a refinement holds `def`s, found {}", describe(self.peek()))));
            }
            let mut d = self.def_in(Some(&name))?;
            let m = d.name.rsplit_once('.').map_or(d.name.clone(), |(_, m)| m.to_string());
            d.name = refine_def_name(&name, &tshow, &m);
            d.public = public;
            // A static method (`def self.from_sql(v)`) has no receiver.
            if let Some(p) = d.params.first_mut().filter(|p| p.name == "self") {
                p.ty = Some(if m.ends_with('!') { TypeExpr::Array(Box::new(target.clone()), p.span) } else { target.clone() });
            }
            if !rtparams.is_empty() {
                let mut tps = rtparams.clone();
                tps.extend(d.tparams);
                d.tparams = tps;
            }
            methods.push(m);
            defs.push(d);
        }
        self.usings.pop();
        Ok(RefineDef { name, tparams: rtparams, target, methods, span: start.to(self.prev_span()) })
    }

    fn iface_def(&mut self, defaults: &mut Vec<Def>) -> PResult<IfaceDef> {
        let shareable = self.cur_shareable.take().is_some();
        if !self.cur_derives.is_empty() || self.cur_data_name.is_some() || self.cur_transparent {
            return Err(Diag::new(self.span(), "an interface takes only `#[shareable]`; derives and #[data] / #[transparent] go before a struct or enum"));
        }
        let start = self.bump().span;
        let sp = self.span();
        let name = match self.bump().tok {
            Tok::Const(n) => n,
            t => return Err(Diag::new(sp, format!("expected an interface name (capitalized), found {}", describe(&t)))),
        };
        self.expect_op("{")?;
        let mut methods = vec![];
        let mut tracked = vec![];
        loop {
            self.skip_newlines();
            while self.eat_op(",") || self.eat_op(";") {
                self.skip_newlines();
            }
            if self.eat_op("}") {
                break;
            }
            if !matches!(self.peek(), Tok::Kw(Kw::Def) | Tok::Attr(_)) {
                return Err(Diag::new(self.span(), format!("an interface holds `def`s, found {}", describe(self.peek()))));
            }
            let d = self.def_sig(Some((&name, true)))?;
            if d.track_caller {
                tracked.push(d.name.rsplit_once('.').map_or(d.name.clone(), |(_, m)| m.to_string()));
            }
            let has_body = d.body.is_some();
            let short = d.name.rsplit_once('.').map_or(d.name.clone(), |(_, m)| m.to_string());
            // `def m -> ~T`: the call's value is a held `~T`.
            let ret = if d.fallible {
                let t = d.ret.clone().unwrap_or(TypeExpr::Named("Unit".into(), d.name_span));
                Some(TypeExpr::Result(Box::new(t), d.errs.clone(), d.name_span))
            } else {
                d.ret.clone()
            };
            methods.push((short, d.params[1..].to_vec(), ret, has_body, d.name_span));
            if let Some(body) = d.body {
                defaults.push(Def { public: true, using: self.usings.clone(), name: d.name, span: d.span, tparams: d.tparams, name_span: d.name_span, params: d.params, ret: d.ret, fallible: d.fallible, errs: d.errs, pure: d.pure, track_caller: d.track_caller, ffi: d.ffi, body });
            }
        }
        Ok(IfaceDef { name, span: start.to(self.prev_span()), methods, tracked, shareable })
    }

    fn enum_def(&mut self, methods: &mut Vec<Def>) -> PResult<EnumDef> {
        if let Some(sp) = self.cur_shareable.take() {
            return Err(Diag::new(sp, "`#[shareable]` goes before an interface").note("it says the interface's values may be handed to tasks: no implementor holds unsynchronized storage"));
        }
        let start = self.bump().span;
        let sp = self.span();
        let name = match self.bump().tok {
            Tok::Const(n) => n,
            t => return Err(Diag::new(sp, format!("expected an enum name (capitalized), found {}", describe(&t)))),
        };
        let tparams = self.tparams()?;
        let first_method = methods.len();
        let derives = std::mem::take(&mut self.cur_derives);
        let data_name = self.cur_data_name.take();
        let transparent = std::mem::take(&mut self.cur_transparent);
        self.expect_op("{")?;
        let mut variants: Vec<(String, Vec<(String, TypeExpr, Span)>, Span)> = vec![];
        let mut dvariants: Vec<crate::derive::DVariant> = vec![];
        let mut vpend = crate::derive::Opts::default();
        let mut mopts: Vec<(usize, Option<String>, bool)> = vec![];
        loop {
            self.skip_newlines();
            while self.eat_op(",") {
                self.skip_newlines();
            }
            if self.eat_op("}") {
                break;
            }
            // `#[json("name")]` / `#[data("Name")]` before a variant or method.
            if let Tok::Attr(a) = self.peek().clone() {
                if crate::derive::is_field_attr(&a) {
                    let asp = self.bump().span;
                    crate::derive::apply_field_attr(&a, asp, &mut vpend)?;
                    continue;
                }
            }
            if matches!(self.peek(), Tok::Ident(p) if p == "pub") && matches!(self.peek_at(1), Tok::Attr(_) | Tok::Kw(Kw::Def) | Tok::Kw(Kw::Fn)) {
                self.bump();
                let mut d = self.def_in(Some(&name))?;
                d.public = true;
                methods.push(d);
                mopts.push((methods.len() - 1, vpend.drename.take(), std::mem::take(&mut vpend.dskip)));
                continue;
            }
            if matches!(self.peek(), Tok::Attr(_) | Tok::Kw(Kw::Def) | Tok::Kw(Kw::Fn)) {
                methods.push(self.def_in(Some(&name))?);
                mopts.push((methods.len() - 1, vpend.drename.take(), std::mem::take(&mut vpend.dskip)));
                continue;
            }
            let vsp = self.span();
            let vname = match self.bump().tok {
                Tok::Const(n) => n,
                t => return Err(Diag::new(vsp, format!("expected a variant name (capitalized), found {}", describe(&t)))),
            };
            if variants.iter().any(|(v, _, _)| *v == vname) {
                return Err(Diag::new(vsp, format!("variant `{vname}` is declared twice")));
            }
            let mut fields = vec![];
            let mut dfields: Vec<crate::derive::DField> = vec![];
            if self.is_op("(") && !self.space_before() {
                self.bump();
                while !self.is_op(")") {
                    let mut fo = crate::derive::Opts::default();
                    while let Tok::Attr(a) = self.peek().clone() {
                        let asp = self.bump().span;
                        if crate::derive::is_field_attr(&a) {
                            crate::derive::apply_field_attr(&a, asp, &mut fo)?;
                        } else {
                            crate::derive::apply_json_attr(&a, asp, &mut fo)?;
                        }
                    }
                    let fsp = self.span();
                    // `name: Type`, or just `Type` (fields named 0, 1, ...).
                    let fname = if let (Tok::Ident(n), Tok::Op(":")) = (self.peek().clone(), self.peek_at(1).clone()) {
                        self.bump();
                        self.bump();
                        n
                    } else {
                        fields.len().to_string()
                    };
                    let ty = self.type_expr()?;
                    dfields.push(crate::derive::DField { name: fname.clone(), ty: ty.clone(), opts: fo, span: fsp });
                    fields.push((fname, ty, fsp));
                    if !self.eat_op(",") {
                        break;
                    }
                }
                self.expect_op(")")?;
            }
            dvariants.push(crate::derive::DVariant { name: vname.clone(), fields: dfields, opts: std::mem::take(&mut vpend) });
            variants.push((vname, fields, vsp));
        }
        if !tparams.is_empty() {
            generic_self(&mut methods[first_method..], &tparams);
        }
        let span = start.to(self.prev_span());
        if derives.iter().any(|d| d == "Gob") {
            let job = crate::derive::DeriveJob { type_opts: Default::default(), derive: "Gob".into(), go_name: data_name.clone(), transparent, name: name.clone(), tparams: tparams.iter().map(|t| t.name.clone()).collect(), public: false, span, shape: crate::derive::DShape::Enum(dvariants.clone()) };
            self.gjobs.push(crate::derive_gob::GobJob { job, go_name: data_name.clone() });
        }
        if derives.iter().any(|d| d == "Data") {
            let job = crate::derive::DeriveJob { type_opts: Default::default(), derive: "Data".into(), go_name: data_name.clone(), transparent, name: name.clone(), tparams: tparams.iter().map(|t| t.name.clone()).collect(), public: false, span, shape: crate::derive::DShape::Enum(dvariants.clone()) };
            let ms = data_methods(methods, &mopts);
            self.djobs.push(crate::derive_data::DataJob { job, go_name: data_name.clone(), methods: ms });
        }
        let type_opts = std::mem::take(&mut self.cur_asn1);
        for d in derives.iter().filter(|d| matches!(d.as_str(), "Json" | "Asn1" | "Arbitrary" | "Row")) {
            self.jobs.push(crate::derive::DeriveJob { type_opts: type_opts.clone(), derive: d.clone(), go_name: data_name.clone(), transparent, name: name.clone(), tparams: tparams.iter().map(|t| t.name.clone()).collect(), public: false, span, shape: crate::derive::DShape::Enum(dvariants.clone()) });
        }
        Ok(EnumDef { name, span, error: false, tparams, variants })
    }

    fn struct_def(&mut self, methods: &mut Vec<Def>) -> PResult<StructDef> {
        if let Some(sp) = self.cur_shareable.take() {
            return Err(Diag::new(sp, "`#[shareable]` goes before an interface").note("it says the interface's values may be handed to tasks: no implementor holds unsynchronized storage"));
        }
        let start = self.bump().span;
        let sp = self.span();
        let name = match self.bump().tok {
            Tok::Const(n) => n,
            t => return Err(Diag::new(sp, format!("expected a struct name (capitalized), found {}", describe(&t)))),
        };
        let tparams = self.tparams()?;
        let first_method = methods.len();
        let derives = std::mem::take(&mut self.cur_derives);
        let data_name = self.cur_data_name.take();
        let transparent = std::mem::take(&mut self.cur_transparent);
        self.expect_op("{")?;
        let mut fields: Vec<(String, TypeExpr, Span)> = vec![];
        let mut fopts: Vec<crate::derive::Opts> = vec![];
        let mut pend = crate::derive::Opts::default();
        let mut mopts: Vec<(usize, Option<String>, bool)> = vec![];
        loop {
            self.skip_newlines();
            while self.eat_op(",") {
                self.skip_newlines();
            }
            if self.eat_op("}") {
                break;
            }
            // `#[json("name")]`, `#[json(omit_empty)]`, `#[json(skip)]` on the next field;
            // `#[data("Name")]`, `#[data(skip)]` on the next field or method.
            if let Tok::Attr(a) = self.peek().clone() {
                if crate::derive::is_field_attr(&a) {
                    let asp = self.bump().span;
                    crate::derive::apply_field_attr(&a, asp, &mut pend)?;
                    continue;
                }
            }
            if matches!(self.peek(), Tok::Ident(p) if p == "pub") && matches!(self.peek_at(1), Tok::Attr(_) | Tok::Kw(Kw::Def) | Tok::Kw(Kw::Fn)) {
                self.bump();
                let mut d = self.def_in(Some(&name))?;
                d.public = true;
                methods.push(d);
                mopts.push((methods.len() - 1, pend.drename.take(), std::mem::take(&mut pend.dskip)));
                continue;
            }
            if matches!(self.peek(), Tok::Attr(_) | Tok::Kw(Kw::Def) | Tok::Kw(Kw::Fn)) {
                methods.push(self.def_in(Some(&name))?);
                mopts.push((methods.len() - 1, pend.drename.take(), std::mem::take(&mut pend.dskip)));
                continue;
            }
            let fsp = self.span();
            let fname = match self.bump().tok {
                Tok::Ident(n) => n,
                // A keyword names a field too (Go's `next`): read it as `self.next`.
                Tok::Kw(k) if matches!(self.peek(), Tok::Op(":")) => kw_name(k).to_string(),
                t => return Err(Diag::new(fsp, format!("expected a field name, found {}", describe(&t)))),
            };
            self.expect_op(":")?;
            let ty = self.type_expr()?;
            if fields.iter().any(|(f, _, _)| *f == fname) {
                return Err(Diag::new(fsp, format!("field `{fname}` is declared twice")));
            }
            fields.push((fname, ty, fsp));
            fopts.push(std::mem::take(&mut pend));
        }
        if !tparams.is_empty() {
            generic_self(&mut methods[first_method..], &tparams);
        }
        let span = start.to(self.prev_span());
        if derives.iter().any(|d| d == "Gob") {
            let dfields = fields.iter().zip(fopts.iter()).map(|((n, t, s), o)| crate::derive::DField { name: n.clone(), ty: t.clone(), opts: o.clone(), span: *s }).collect();
            let job = crate::derive::DeriveJob { type_opts: Default::default(), derive: "Gob".into(), go_name: data_name.clone(), transparent, name: name.clone(), tparams: tparams.iter().map(|t| t.name.clone()).collect(), public: false, span, shape: crate::derive::DShape::Struct(dfields) };
            self.gjobs.push(crate::derive_gob::GobJob { job, go_name: data_name.clone() });
        }
        if derives.iter().any(|d| d == "Data") {
            let dfields = fields.iter().zip(fopts.iter()).map(|((n, t, s), o)| crate::derive::DField { name: n.clone(), ty: t.clone(), opts: o.clone(), span: *s }).collect();
            let job = crate::derive::DeriveJob { type_opts: Default::default(), derive: "Data".into(), go_name: data_name.clone(), transparent, name: name.clone(), tparams: tparams.iter().map(|t| t.name.clone()).collect(), public: false, span, shape: crate::derive::DShape::Struct(dfields) };
            let ms = data_methods(methods, &mopts);
            self.djobs.push(crate::derive_data::DataJob { job, go_name: data_name.clone(), methods: ms });
        }
        let dfields: Vec<crate::derive::DField> = fields.iter().zip(fopts).map(|((n, t, s), o)| crate::derive::DField { name: n.clone(), ty: t.clone(), opts: o, span: *s }).collect();
        if derives.iter().any(|d| d == "Xml") {
            let job = crate::derive::DeriveJob { type_opts: Default::default(), derive: "Xml".into(), go_name: data_name.clone(), transparent, name: name.clone(), tparams: tparams.iter().map(|t| t.name.clone()).collect(), public: false, span, shape: crate::derive::DShape::Struct(dfields.clone()) };
            let ms = methods[first_method..].iter().map(|d| d.name.rsplit('.').next().unwrap_or(&d.name).to_string()).collect();
            self.xjobs.push(crate::derive_xml::XmlJob { job, methods: ms });
        }
        let type_opts = std::mem::take(&mut self.cur_asn1);
        for d in derives.iter().filter(|d| matches!(d.as_str(), "Json" | "Asn1" | "Arbitrary" | "Row")) {
            self.jobs.push(crate::derive::DeriveJob { type_opts: type_opts.clone(), derive: d.clone(), go_name: data_name.clone(), transparent, name: name.clone(), tparams: tparams.iter().map(|t| t.name.clone()).collect(), public: false, span, shape: crate::derive::DShape::Struct(dfields.clone()) });
        }
        Ok(StructDef { name, span, tparams, fields })
    }

    fn type_expr(&mut self) -> PResult<TypeExpr> {
        let sp = self.span();
        if self.eat_op("@") {
            let mut name = match self.bump().tok {
                Tok::Const(n) => n,
                Tok::Ident(pkg) if self.is_op(".") => {
                    self.bump();
                    match self.bump().tok {
                        Tok::Const(n) => format!("{pkg}.{n}"),
                        t => return Err(Diag::new(sp, format!("expected a type after `@`, found {}", describe(&t)))),
                    }
                }
                t => return Err(Diag::new(sp, format!("expected a type after `@`, found {}", describe(&t)))),
            };
            // `@Node[T]`: a handle to a generic type's instance.
            let mut args = vec![];
            if self.is_op("[") && !self.space_before() {
                self.bump();
                args.push(self.type_expr()?);
                while self.eat_op(",") {
                    args.push(self.type_expr()?);
                }
                self.expect_op("]")?;
            }
            let base = TypeExpr::Handle(std::mem::take(&mut name), args, sp.to(self.prev_span()));
            if self.is_op("?") && !self.space_before() {
                self.bump();
                return Ok(TypeExpr::Opt(Box::new(base), sp.to(self.prev_span())));
            }
            return Ok(base);
        }
        if self.eat_op("~") {
            let t = self.type_expr()?;
            let errs = if self.is_op("<") && !self.space_before() {
                self.bump();
                let mut es = vec![];
                loop {
                    let esp = self.span();
                    match self.bump().tok {
                        Tok::Const(n) => es.push(n),
                        // `pkg.NumError`: an error type from an imported package.
                        Tok::Ident(pkg) if self.is_op(".") && matches!(self.peek_at(1), Tok::Const(_)) => {
                            self.bump();
                            if let Tok::Const(n) = self.bump().tok {
                                es.push(format!("{pkg}.{n}"));
                            }
                        }
                        t => return Err(Diag::new(esp, format!("expected an error type, found {}", describe(&t)))),
                    }
                    if !self.eat_op("|") {
                        break;
                    }
                }
                self.expect_op(">")?;
                Some(es)
            } else {
                None
            };
            return Ok(TypeExpr::Result(Box::new(t), errs, sp.to(self.prev_span())));
        }
        if self.is_op("(") {
            // `(A, B) -> R`
            self.bump();
            let mut ps = vec![];
            while !self.is_op(")") {
                ps.push(self.type_expr()?);
                if !self.eat_op(",") {
                    break;
                }
            }
            self.expect_op(")")?;
            if !self.is_op("->") && ps.len() >= 2 {
                // `(A, B)`, `(A, B)?`: a tuple.
                let t = TypeExpr::Tuple(ps, sp.to(self.prev_span()));
                if self.is_op("?") && !self.space_before() {
                    self.bump();
                    return Ok(TypeExpr::Opt(Box::new(t), sp.to(self.prev_span())));
                }
                return Ok(t);
            }
            if !self.is_op("->") && ps.len() == 1 {
                // `(T)`, `((A) -> R)?`: a parenthesized type (an optional
                // function type needs the parentheses).
                let t = ps.pop().unwrap();
                if self.is_op("?") && !self.space_before() {
                    self.bump();
                    return Ok(TypeExpr::Opt(Box::new(t), sp.to(self.prev_span())));
                }
                return Ok(t);
            }
            self.expect_op("->")?;
            let r = self.type_expr()?;
            return Ok(TypeExpr::Fn(ps, Box::new(r), sp.to(self.prev_span())));
        }
        let base = if self.eat_op("[") {
            let inner = self.type_expr()?;
            if self.eat_op(";") {
                let n = self.or()?;
                self.expect_op("]")?;
                TypeExpr::Fixed(Box::new(inner), Box::new(n), sp.to(self.prev_span()))
            } else {
                self.expect_op("]")?;
                TypeExpr::Array(Box::new(inner), sp.to(self.prev_span()))
            }
        } else {
            let mut tok = self.bump().tok;
            // `geom.Point`: a type from an imported package.
            if let Tok::Ident(pkg) = &tok {
                if self.is_op(".") && matches!(self.peek_at(1), Tok::Const(_)) {
                    self.bump();
                    if let Tok::Const(n) = self.bump().tok {
                        tok = Tok::Const(format!("{pkg}.{n}"));
                    }
                }
            }
            match tok {
                Tok::Const(n) if self.is_op("[") && !self.space_before() => {
                    self.bump();
                    let mut args = vec![self.type_expr()?];
                    while self.eat_op(",") {
                        args.push(self.type_expr()?);
                    }
                    self.expect_op("]")?;
                    TypeExpr::App(n, args, sp.to(self.prev_span()))
                }
                Tok::Const(n) => TypeExpr::Named(n, sp),
                t => return Err(Diag::new(sp, format!("expected a type, found {}", describe(&t)))),
            }
        };
        if self.is_op("?") && !self.space_before() {
            self.bump();
            return Ok(TypeExpr::Opt(Box::new(base), sp.to(self.prev_span())));
        }
        Ok(base)
    }

    fn braced_stmts(&mut self) -> PResult<Vec<Stmt>> {
        self.expect_op("{")?;
        self.stmts_to_brace()
    }

    /// Statements up to the closing `}` (the `{` already read).
    fn stmts_to_brace(&mut self) -> PResult<Vec<Stmt>> {
        let saved = std::mem::replace(&mut self.in_cond, false);
        let mut out = vec![];
        loop {
            self.skip_newlines();
            if self.eat_op("}") {
                break;
            }
            if matches!(self.peek(), Tok::Eof) {
                return Err(Diag::new(self.span(), "unexpected end of file: missing `}`"));
            }
            out.push(self.stmt()?);
        }
        self.in_cond = saved;
        Ok(out)
    }

    // ---------- statements ----------

    fn stmt(&mut self) -> PResult<Stmt> {
        let start = self.span();
        let kind = match self.peek().clone() {
            Tok::Kw(Kw::While) => {
                self.bump();
                let cond = self.cond()?;
                let body = self.braced_stmts()?;
                return Ok(Stmt { kind: StmtKind::While(cond, body), span: start.to(self.prev_span()) });
            }
            Tok::Kw(Kw::If) | Tok::Kw(Kw::Unless) => {
                let unless = self.is_kw(Kw::Unless);
                self.bump();
                return self.if_rest(start, unless);
            }
            Tok::Kw(Kw::Next) => {
                self.bump();
                StmtKind::Next
            }
            Tok::Kw(Kw::Break) => {
                self.bump();
                StmtKind::Break(if self.at_stmt_end() { None } else { Some(self.expr()?) })
            }
            Tok::Kw(Kw::Return) => {
                self.bump();
                StmtKind::Return(if self.at_stmt_end() {
                    None
                } else {
                    // `return a, b`: a tuple, as Go returns several results.
                    let first = self.expr()?;
                    if self.is_op(",") {
                        let tsp = first.span;
                        let mut items = vec![first];
                        while self.eat_op(",") {
                            items.push(self.expr()?);
                        }
                        Some(self.mk(ExprKind::Tuple(items), tsp.to(self.prev_span())))
                    } else {
                        Some(first)
                    }
                })
            }
            Tok::Kw(Kw::Fail) => {
                self.bump();
                if self.is_op("=") {
                    return Err(reserved(start, Kw::Fail, "a variable"));
                }
                if self.at_stmt_end() {
                    return Err(fail_needs_value(start));
                }
                StmtKind::Fail(self.expr()?)
            }
            Tok::Kw(Kw::Def) | Tok::Kw(Kw::Fn) | Tok::Attr(_) => return Err(Diag::new(start, "methods can only be defined at the top level")),
            Tok::Kw(Kw::Defer) => {
                self.bump();
                if self.is_op("{") {
                    // `defer { stmts }`: run the statements at scope exit (as
                    // `if true { stmts }`; a map literal is never deferred).
                    let sp = self.span();
                    let body = self.braced_stmts()?;
                    let cond = self.mk(ExprKind::Bool(true), sp);
                    StmtKind::Defer(self.mk(ExprKind::If(Box::new(cond), body, vec![]), sp.to(self.prev_span())))
                } else {
                    StmtKind::Defer(self.expr()?)
                }
            }
            Tok::Ident(kw) if kw == "using" && matches!(self.peek_at(1), Tok::Const(_) | Tok::Ident(_)) && !self.is_local("using") => {
                self.bump();
                let sp = self.span();
                let mut name = match self.bump().tok {
                    Tok::Const(n) | Tok::Ident(n) => n,
                    _ => unreachable!(),
                };
                if self.eat_op(".") {
                    match self.bump().tok {
                        Tok::Const(n) => name = format!("{name}.{n}"),
                        t => return Err(Diag::new(sp, format!("expected a refinement name, found {}", describe(&t)))),
                    }
                }
                // At the top level it also applies to the defs below.
                if self.scopes.len() == 1 {
                    self.usings.push(name.clone());
                }
                StmtKind::Using(name)
            }
            Tok::Kw(Kw::For) => {
                self.bump();
                return self.for_rest(start);
            }
            Tok::Kw(Kw::Struct) | Tok::Kw(Kw::Enum) => return Err(Diag::new(start, "types can only be defined at the top level")),
            Tok::Ident(name)
                if matches!(self.peek_at(1), Tok::Op(":"))
                    && (matches!(self.peek_at(2), Tok::Const(_) | Tok::Op("[") | Tok::Op("(") | Tok::Op("~") | Tok::Op("@"))
                        || (matches!(self.peek_at(2), Tok::Ident(_)) && matches!(self.peek_at(3), Tok::Op(".")) && matches!(self.peek_at(4), Tok::Const(_)))) =>
            {
                // `x: T = e`
                let sp = self.bump().span;
                self.bump();
                let ty = self.type_expr()?;
                self.expect_op("=")?;
                self.skip_line_continuation();
                let e = self.expr()?;
                self.declare(&name);
                StmtKind::Decl(name, sp, ty, e)
            }
            Tok::Ident(_) if self.is_multi_assign() => {
                let mut targets = vec![];
                loop {
                    let sp = self.span();
                    if let Tok::Ident(n) = self.bump().tok {
                        targets.push((n, sp));
                    }
                    if !self.eat_op(",") {
                        break;
                    }
                }
                self.expect_op("=")?;
                self.skip_line_continuation();
                let mut values = vec![self.expr()?];
                while self.eat_op(",") {
                    values.push(self.expr()?);
                }
                for (n, _) in &targets {
                    self.declare(n);
                }
                StmtKind::MultiAssign(targets, values)
            }
            _ => {
                // `a[i], a[j] = a[j], a[i]`: places on the left.
                let at = self.pos;
                let first = self.ternary()?;
                if self.is_op(",") && is_place(&first) {
                    let mut targets = vec![first];
                    while self.eat_op(",") {
                        let t = self.ternary()?;
                        if !is_place(&t) {
                            return Err(Diag::new(t.span, "cannot assign to this expression"));
                        }
                        targets.push(t);
                    }
                    self.expect_op("=")?;
                    self.skip_line_continuation();
                    let mut values = vec![self.expr()?];
                    while self.eat_op(",") {
                        values.push(self.expr()?);
                    }
                    StmtKind::PlaceMultiAssign(targets, values)
                } else {
                    self.pos = at;
                    StmtKind::Expr(self.expr()?)
                }
            }
        };
        let mut s = Stmt { kind, span: start.to(self.prev_span()) };
        // Modifiers: `stmt if cond`, `stmt unless cond`.
        if self.is_kw(Kw::If) || self.is_kw(Kw::Unless) {
            let unless = self.is_kw(Kw::Unless);
            self.bump();
            let mut cond = self.expr()?;
            if unless {
                let sp = cond.span;
                cond = self.mk(ExprKind::Not(Box::new(cond)), sp);
            }
            let span = s.span.to(self.prev_span());
            s = Stmt { kind: StmtKind::If(cond, vec![s], vec![]), span };
        }
        if !self.at_stmt_end() {
            return Err(Diag::new(self.span(), format!("unexpected {}", describe(self.peek()))));
        }
        Ok(s)
    }

    fn if_rest(&mut self, start: Span, unless: bool) -> PResult<Stmt> {
        let mut cond = self.cond()?;
        if unless {
            let sp = cond.span;
            cond = self.mk(ExprKind::Not(Box::new(cond)), sp);
        }
        let then = self.braced_stmts()?;
        let mut els = vec![];
        let save = self.pos;
        self.skip_newlines();
        if self.is_kw(Kw::Else) {
            self.bump();
            if self.is_kw(Kw::If) || self.is_kw(Kw::Unless) {
                // `else if`
                let unless = self.is_kw(Kw::Unless);
                let sp = self.bump().span;
                els = vec![self.if_rest(sp, unless)?];
            } else {
                els = self.braced_stmts()?;
            }
        } else if self.is_kw(Kw::Elsif) {
            let sp = self.bump().span;
            els = vec![self.if_rest(sp, false)?];
        } else {
            self.pos = save;
        }
        Ok(Stmt { kind: StmtKind::If(cond, then, els), span: start.to(self.prev_span()) })
    }

    /// `for x in xs { ... }` / `for a, b in pairs { ... }`: sugar for
    /// `xs.each { |x| ... }` (break / next / return behave as in a loop).
    fn for_rest(&mut self, start: Span) -> PResult<Stmt> {
        let mut params = vec![];
        loop {
            let sp = self.span();
            match self.bump().tok {
                Tok::Ident(n) => params.push((n, sp)),
                t => return Err(Diag::new(sp, format!("expected a loop variable, found {}", describe(&t)))),
            }
            if !self.eat_op(",") {
                break;
            }
        }
        if !self.is_kw(Kw::In) {
            return Err(Diag::new(self.span(), format!("expected `in`, found {}", describe(self.peek()))));
        }
        self.bump();
        let coll = self.cond()?;
        self.scopes.push(HashSet::new());
        for (n, _) in &params {
            self.declare(n);
        }
        let bstart = self.span();
        let body = self.braced_stmts()?;
        self.scopes.pop();
        let id = self.id();
        let block = Block { id, params, body, span: bstart.to(self.prev_span()) };
        let sp = start.to(self.prev_span());
        let call = self.mk(ExprKind::Call { recv: Some(Box::new(coll)), name: "each".into(), name_span: start, args: vec![], block: Some(Box::new(block)), block_sym: None }, sp);
        Ok(Stmt { kind: StmtKind::Expr(call), span: sp })
    }

    /// `case [subject] { pat | pat => body ... }`
    fn case_rest(&mut self, start: Span) -> PResult<Expr> {
        let subject = if self.is_op("{") { None } else { Some(Box::new(self.cond()?)) };
        self.expect_op("{")?;
        let saved = std::mem::replace(&mut self.in_cond, false);
        let mut arms = vec![];
        loop {
            self.skip_newlines();
            while self.eat_op(",") {
                self.skip_newlines();
            }
            if self.eat_op("}") {
                break;
            }
            let asp = self.span();
            let mut pats = vec![];
            self.scopes.push(HashSet::new());
            if matches!(self.peek(), Tok::Ident(n) if n == "_") && matches!(self.peek_at(1), Tok::Op("=>")) {
                self.bump();
            } else if subject.is_none() {
                pats.push(Pat::Value(self.expr()?));
            } else {
                loop {
                    // A qualified variant: `PErr.Bad(..)`, `pk.PErr.Bad(..)`. The
                    // name keeps its type part and package (`pk.PErr.Bad`): two
                    // packages' error types may share a name (os.FsError, fs.FsError).
                    let mut k = 0;
                    while matches!(self.peek_at(k), Tok::Ident(_) | Tok::Const(_)) && matches!(self.peek_at(k + 1), Tok::Op(".")) {
                        k += 2;
                    }
                    // (`pkg.CONST` has no type part: it's a value.)
                    let has_type = (0..k).step_by(2).any(|j| matches!(self.peek_at(j), Tok::Const(_)));
                    let qualified = k > 0 && has_type && matches!(self.peek_at(k), Tok::Const(_)) && {
                        let after = self.peek_at(k + 1);
                        (matches!(after, Tok::Op("(")) && !self.space_before_at(k + 1)) || matches!(after, Tok::Op("=>") | Tok::Op("|"))
                    };
                    let mut qual = String::new();
                    let qsp = self.span();
                    // `pkg.Type(v)`: an implementor of an interface from another
                    // package (a type switch, D71); the name keeps its package.
                    let pkg_type = k == 2 && !has_type && matches!(self.peek_at(0), Tok::Ident(_)) && matches!(self.peek_at(2), Tok::Const(_)) && matches!(self.peek_at(3), Tok::Op("(")) && !self.space_before_at(3);
                    if pkg_type {
                        if let Tok::Ident(p) = self.bump().tok {
                            qual.push_str(&p);
                            qual.push('.');
                        }
                        self.bump();
                    }
                    let qualified = qualified || pkg_type;
                    if qualified && !pkg_type {
                        for _ in 0..k / 2 {
                            if let Tok::Const(c) | Tok::Ident(c) = self.bump().tok {
                                qual.push_str(&c);
                                qual.push('.');
                            }
                            self.bump();
                        }
                    }
                    if let Tok::Const(vname) = self.peek().clone() {
                        let vname = format!("{qual}{vname}");
                        let paren = matches!(self.peek_at(1), Tok::Op("(")) && !self.space_before_at(1);
                        if paren || matches!(self.peek_at(1), Tok::Op("=>") | Tok::Op("|")) {
                            let vsp = if qualified { qsp.to(self.bump().span) } else { self.bump().span };
                            let binds = if paren {
                                self.bump();
                                let mut bs = vec![];
                                while !self.is_op(")") {
                                    let bsp = self.span();
                                    match self.bump().tok {
                                        Tok::Ident(n) => {
                                            self.declare(&n);
                                            bs.push((n, bsp));
                                        }
                                        t => return Err(Diag::new(bsp, format!("expected a name to bind (or `_`), found {}", describe(&t)))),
                                    }
                                    if !self.eat_op(",") {
                                        break;
                                    }
                                }
                                self.expect_op(")")?;
                                Some(bs)
                            } else {
                                None
                            };
                            pats.push(Pat::Variant(vname, binds, vsp.to(self.prev_span())));
                            if !self.eat_op("|") {
                                break;
                            }
                            continue;
                        }
                    }
                    let lo = self.shift()?;
                    let pat = if self.is_op("..") || self.is_op("...") {
                        let excl = self.bump().tok == Tok::Op("...");
                        Pat::Range(lo, self.shift()?, excl)
                    } else {
                        Pat::Value(lo)
                    };
                    pats.push(pat);
                    if !self.eat_op("|") {
                        break;
                    }
                }
            }
            self.expect_op("=>")?;
            let body = if self.is_op("{") {
                self.braced_stmts()?
            } else if self.at_jump() {
                // S8: `X => return v`, `X => fail e`, `X => break`, `X => next`.
                vec![self.jump()?]
            } else {
                let e = self.expr()?;
                vec![Stmt { span: e.span, kind: StmtKind::Expr(e) }]
            };
            self.scopes.pop();
            arms.push(CaseArm { pats, body, span: asp.to(self.prev_span()) });
        }
        self.in_cond = saved;
        Ok(self.mk(ExprKind::Case(subject, arms), start.to(self.prev_span())))
    }

    fn cond(&mut self) -> PResult<Expr> {
        let saved = std::mem::replace(&mut self.in_cond, true);
        let e = self.expr();
        self.in_cond = saved;
        e
    }

    fn at_stmt_end(&self) -> bool {
        matches!(self.peek(), Tok::Newline | Tok::Eof | Tok::Op(";") | Tok::Op("}") | Tok::Kw(Kw::If) | Tok::Kw(Kw::Unless))
    }

    fn is_multi_assign(&self) -> bool {
        let mut k = 0;
        loop {
            if !matches!(self.peek_at(k), Tok::Ident(_)) {
                return false;
            }
            match self.peek_at(k + 1) {
                Tok::Op(",") => k += 2,
                Tok::Op("=") => return k > 0,
                _ => return false,
            }
        }
    }

    // ---------- expressions ----------

    pub fn expr(&mut self) -> PResult<Expr> {
        let lhs = self.ternary()?;
        if self.is_op("=") {
            if let Some(c) = const_root(&lhs) {
                return Err(Diag::new(lhs.span, format!("`{c}` is a constant and can't be changed")).note(format!("copy it into a variable to get an array of your own: `xs = {c}`")));
            }
            if let ExprKind::Const(c) = &lhs.kind {
                return Err(Diag::new(lhs.span, format!("`{c}` is declared at the top level and can't be assigned")).note(format!("a package-level `Atomic` changes with `{c}.store(v)`, a `Mutex` with `{c}.lock {{ |v| ... }}` (R11)")));
            }
            if !is_place(&lhs) {
                return Err(Diag::new(lhs.span, "cannot assign to this expression"));
            }
            self.bump();
            if let ExprKind::Name(n) = &lhs.kind {
                let n = n.clone();
                self.declare(&n);
            }
            self.skip_line_continuation();
            let rhs = self.expr()?;
            let sp = lhs.span.to(rhs.span);
            return Ok(self.mk(ExprKind::Assign(Box::new(lhs), Box::new(rhs)), sp));
        }
        for (tok, op) in [
            ("+=", BinOp::Add),
            ("-=", BinOp::Sub),
            ("*=", BinOp::Mul),
            ("/=", BinOp::Div),
            ("%=", BinOp::Rem),
            ("**=", BinOp::Pow),
            ("&=", BinOp::BitAnd),
            ("|=", BinOp::BitOr),
            ("^=", BinOp::BitXor),
            ("&^=", BinOp::AndNot),
            ("<<=", BinOp::Shl),
            (">>=", BinOp::Shr),
            ("+%=", BinOp::AddW),
            ("-%=", BinOp::SubW),
            ("*%=", BinOp::MulW),
        ] {
            if self.is_op(tok) {
                self.bump();
                self.skip_line_continuation();
                let rhs = self.expr()?;
                let sp = lhs.span.to(rhs.span);
                return Ok(self.mk(ExprKind::OpAssign(op, Box::new(lhs), Box::new(rhs)), sp));
            }
        }
        Ok(lhs)
    }

    fn ternary(&mut self) -> PResult<Expr> {
        let c = self.range()?;
        if self.is_op("?") {
            self.bump();
            let a = self.ternary()?;
            self.expect_op(":")?;
            let b = self.ternary()?;
            let sp = c.span.to(b.span);
            return Ok(self.mk(ExprKind::Ternary(Box::new(c), Box::new(a), Box::new(b)), sp));
        }
        Ok(c)
    }

    fn range(&mut self) -> PResult<Expr> {
        let lo = self.or()?;
        for (tok, excl) in [("...", true), ("..", false)] {
            if self.is_op(tok) {
                self.bump();
                let hi = self.or()?;
                let sp = lo.span.to(hi.span);
                return Ok(self.mk(ExprKind::Range(Box::new(lo), Box::new(hi), excl), sp));
            }
        }
        Ok(lo)
    }

    fn binary_level(&mut self, ops: &[(&str, BinOp)], next: fn(&mut Self) -> PResult<Expr>) -> PResult<Expr> {
        let mut l = next(self)?;
        'outer: loop {
            for (tok, op) in ops {
                if self.is_op(tok) {
                    self.bump();
                    self.skip_line_continuation();
                    let r = next(self)?;
                    let sp = l.span.to(r.span);
                    l = self.mk(ExprKind::Binary(*op, Box::new(l), Box::new(r)), sp);
                    continue 'outer;
                }
            }
            return Ok(l);
        }
    }

    fn skip_line_continuation(&mut self) {
        while matches!(self.peek(), Tok::Newline) {
            self.bump();
        }
    }

    fn or(&mut self) -> PResult<Expr> {
        let mut l = self.and()?;
        while self.is_op("||") {
            self.bump();
            self.skip_line_continuation();
            if self.at_jump() {
                // S8: `opt || return v` (`|| fail e`, `|| break`, `|| next`):
                // the optional's value, else the jump. Desugared to
                // `if tmp = opt { tmp } else { jump }`.
                let jump = self.jump()?;
                let sp = l.span.to(jump.span);
                let tmp = format!("{OR_JUMP_TMP}{}", *self.next_id);
                let t1 = self.mk(ExprKind::Name(tmp.clone()), l.span);
                let cond = self.mk(ExprKind::Assign(Box::new(t1), Box::new(l)), sp);
                let t2 = self.mk(ExprKind::Name(tmp), sp);
                let then = vec![Stmt { span: sp, kind: StmtKind::Expr(t2) }];
                return Ok(self.mk(ExprKind::If(Box::new(cond), then, vec![jump]), sp));
            }
            let r = self.and()?;
            let sp = l.span.to(r.span);
            l = self.mk(ExprKind::Binary(BinOp::Or, Box::new(l), Box::new(r)), sp);
        }
        Ok(l)
    }

    /// Does a jump (`return`, `fail`, `break`, `next`) start here?
    fn at_jump(&self) -> bool {
        match self.peek() {
            Tok::Kw(Kw::Return | Kw::Break | Kw::Next | Kw::Fail) => true,
            _ => false,
        }
    }

    /// A jump where an expression or a case arm ends (S8): `return [v]`,
    /// `fail e`, `break [v]`, `next`. No `return a, b` (write a tuple).
    fn jump(&mut self) -> PResult<Stmt> {
        let start = self.span();
        let ends = |p: &Self| p.at_stmt_end() || p.is_op(",") || p.is_op(")") || p.is_op("]");
        let kind = match self.bump().tok {
            Tok::Kw(Kw::Next) => StmtKind::Next,
            Tok::Kw(Kw::Break) => StmtKind::Break(if ends(self) { None } else { Some(self.ternary()?) }),
            Tok::Kw(Kw::Return) => StmtKind::Return(if ends(self) { None } else { Some(self.ternary()?) }),
            _ => {
                if ends(self) {
                    return Err(fail_needs_value(start));
                }
                StmtKind::Fail(self.ternary()?)
            }
        };
        Ok(Stmt { kind, span: start.to(self.prev_span()) })
    }
    fn and(&mut self) -> PResult<Expr> {
        self.binary_level(&[("&&", BinOp::And)], Self::not)
    }
    fn not(&mut self) -> PResult<Expr> {
        if self.is_op("!") {
            let sp = self.bump().span;
            let e = self.not()?;
            let full = sp.to(e.span);
            return Ok(self.mk(ExprKind::Not(Box::new(e)), full));
        }
        let e = self.equality()?;
        // `R is io.Seeker` / `T is like Int`: a compile-time type test (S6).
        if matches!(self.peek(), Tok::Ident(w) if w == "is") && matches!(e.kind, ExprKind::Const(_) | ExprKind::TypeApp(..)) {
            self.bump();
            let lhs = match &e.kind {
                ExprKind::Const(c) => TypeExpr::Named(c.clone(), e.span),
                ExprKind::TypeApp(c, args) => TypeExpr::App(c.clone(), args.clone(), e.span),
                _ => unreachable!(),
            };
            let test = if matches!(self.peek(), Tok::Ident(w) if w == "like") {
                self.bump();
                let lsp = self.span();
                match self.bump().tok {
                    Tok::Const(t) => IsTest::Like(t),
                    t => return Err(Diag::new(lsp, format!("expected a type after `like`, found {}", describe(&t)))),
                }
            } else {
                IsTest::Type(self.type_expr()?)
            };
            let sp = e.span.to(self.prev_span());
            return Ok(self.mk(ExprKind::Is(lhs, test), sp));
        }
        Ok(e)
    }
    fn equality(&mut self) -> PResult<Expr> {
        self.binary_level(&[("==", BinOp::Eq), ("!=", BinOp::Ne)], Self::comparison)
    }
    fn comparison(&mut self) -> PResult<Expr> {
        self.binary_level(&[("<=", BinOp::Le), (">=", BinOp::Ge), ("<", BinOp::Lt), (">", BinOp::Gt)], Self::bitor)
    }
    // Precedence follows Ruby (so `arr << x + 1` pushes `x + 1`), with Go's
    // extra operators slotted in: `| ^` < `& &^` < `<< >>` < `+ -` < `* / %`.
    fn bitor(&mut self) -> PResult<Expr> {
        self.binary_level(&[("|", BinOp::BitOr), ("^", BinOp::BitXor)], Self::bitand)
    }
    fn bitand(&mut self) -> PResult<Expr> {
        self.binary_level(&[("&^", BinOp::AndNot), ("&", BinOp::BitAnd)], Self::shift)
    }
    fn shift(&mut self) -> PResult<Expr> {
        let mut l = self.additive()?;
        loop {
            if self.is_op("<<") {
                // A method call: push for arrays, send for channels, shift for integers.
                let op_span = self.bump().span;
                let r = self.additive()?;
                let sp = l.span.to(r.span);
                l = self.mk(
                    ExprKind::Call { recv: Some(Box::new(l)), name: "<<".into(), name_span: op_span, args: vec![r], block: None, block_sym: None },
                    sp,
                );
            } else if self.is_op(">>") {
                self.bump();
                let r = self.additive()?;
                let sp = l.span.to(r.span);
                l = self.mk(ExprKind::Binary(BinOp::Shr, Box::new(l), Box::new(r)), sp);
            } else {
                return Ok(l);
            }
        }
    }
    fn additive(&mut self) -> PResult<Expr> {
        self.binary_level(&[("+%", BinOp::AddW), ("-%", BinOp::SubW), ("+", BinOp::Add), ("-", BinOp::Sub)], Self::multiplicative)
    }
    fn multiplicative(&mut self) -> PResult<Expr> {
        self.binary_level(&[("*%", BinOp::MulW), ("*", BinOp::Mul), ("/", BinOp::Div), ("%", BinOp::Rem)], Self::unary)
    }
    fn unary(&mut self) -> PResult<Expr> {
        if self.is_op("-") {
            let sp = self.bump().span;
            let e = self.unary()?;
            let full = sp.to(e.span);
            match &e.kind {
                ExprKind::Int(v) if *v != i64::MIN => return Ok(self.mk(ExprKind::Int(-v), full)),
                ExprKind::BigInt(t) => {
                    let n = format!("-{t}");
                    return Ok(match n.parse::<i64>() {
                        Ok(v) => self.mk(ExprKind::Int(v), full),
                        Err(_) => self.mk(ExprKind::BigInt(n), full),
                    });
                }
                ExprKind::Float(v, t) => {
                    let (v, t) = (-v, format!("-{t}"));
                    return Ok(self.mk(ExprKind::Float(v, t), full));
                }
                // `-2i` is the constant (0, -2) (Go), not -(0+2i) = (-0, -2).
                ExprKind::Call { recv: Some(r), name, args, .. } if name == "__imag" && matches!(&r.kind, ExprKind::Const(c) if c == "Complex") && matches!(args.as_slice(), [Expr { kind: ExprKind::Float(..), .. }]) => {
                    let mut e = e.clone();
                    if let ExprKind::Call { args, .. } = &mut e.kind {
                        if let ExprKind::Float(v, t) = &args[0].kind {
                            let (v, t) = (-v, format!("-{t}"));
                            args[0].kind = ExprKind::Float(v, t);
                        }
                    }
                    e.span = full;
                    return Ok(e);
                }
                _ => {}
            }
            return Ok(self.mk(ExprKind::Neg(Box::new(e)), full));
        }
        if self.is_op("^") {
            let sp = self.bump().span;
            let e = self.unary()?;
            let full = sp.to(e.span);
            return Ok(self.mk(ExprKind::BitNot(Box::new(e)), full));
        }
        if self.is_kw(Kw::Try) {
            return Err(Diag::new(self.span(), "`try` is now `~`: `~f(x)`, `x.~m(y)`, `~(a * b)`"));
        }
        if self.is_op("~") {
            // `~f(x)`, `~File.read(p)`, `~(a * b)`, `~r`: the `~` takes the
            // next call (not the whole chain); the chain continues after it.
            let sp = self.bump().span;
            let mut e = self.primary()?;
            // A bare name or constant takes its whole dotted chain (indexing
            // included) up to the first call with arguments: `~p.twice`,
            // `~self.plus(1)`, `~File.read(p)`, `~xs[i]`, `~ps[i].plus(1)`.
            if matches!(e.kind, ExprKind::Name(_) | ExprKind::Const(_) | ExprKind::TypeApp(..)) {
                loop {
                    if self.is_op("[") && !self.space_before() {
                        e = self.index_step(e)?;
                        continue;
                    }
                    if !(self.is_op(".") && !matches!(self.peek_at(1), Tok::Op("~"))) {
                        break;
                    }
                    self.bump();
                    let (step, parens) = self.dot_step(e, ".")?;
                    e = step;
                    if parens || matches!(&e.kind, ExprKind::Call { block: Some(_), .. }) {
                        break;
                    }
                }
            }
            let full = sp.to(e.span);
            let t = self.mk(ExprKind::Try(Box::new(e)), full);
            return self.postfix_from(t);
        }
        if self.is_op("!") {
            return self.not();
        }
        self.power()
    }
    fn power(&mut self) -> PResult<Expr> {
        let base = self.postfix()?;
        if self.is_op("**") {
            self.bump();
            let exp = self.unary()?;
            let sp = base.span.to(exp.span);
            return Ok(self.mk(ExprKind::Binary(BinOp::Pow, Box::new(base), Box::new(exp)), sp));
        }
        Ok(base)
    }

    fn postfix(&mut self) -> PResult<Expr> {
        let e = self.primary()?;
        self.postfix_from(e)
    }

    /// The one parser of a method step, after a `.`, `.~` or `?.` (`after`):
    /// `name`, `name(args)`, a block, and after `.` also `pkg.Type[T, ...]`
    /// (a package's generic type applied). Returns the step and whether it
    /// had parenthesized arguments.
    fn dot_step(&mut self, e: Expr, after: &str) -> PResult<(Expr, bool)> {
        self.skip_line_continuation();
        let name_span = self.span();
        let name = match self.bump().tok {
            Tok::Ident(n) => n,
            Tok::Const(n) if after == "." => n,
            Tok::Kw(k) => kw_name(k).to_string(),
            t => return Err(Diag::new(name_span, format!("expected a method name after `{after}`, found {}", describe(&t)))),
        };
        // `geom.Stack[Int].new`: a package's generic type applied.
        let pkg = match &e.kind {
            ExprKind::Name(a) => Some(a.clone()),
            ExprKind::Call { recv: None, name: a, args, block: None, .. } if args.is_empty() => Some(a.clone()),
            _ => None,
        };
        let mut type_err = None;
        if let Some(pkg) = pkg.filter(|_| after == "." && name.chars().next().is_some_and(|c| c.is_uppercase()) && self.is_op("[") && !self.space_before()) {
            let save = self.pos;
            self.bump();
            match self.type_args_then_dot(false) {
                Ok(Some(targs)) => return Ok((self.mk(ExprKind::TypeApp(format!("{pkg}.{name}"), targs), e.span.to(self.prev_span())), false)),
                Ok(None) => {}
                Err(d) => type_err = Some(d),
            }
            self.pos = save;
        }
        let parens = self.is_op("(") && !self.space_before();
        let (args, block_sym) = if parens { self.call_args()? } else { (vec![], None) };
        let block = self.maybe_block()?;
        let sp = e.span.to(self.prev_span());
        let call = self.mk(ExprKind::Call { recv: Some(Box::new(e)), name, name_span, args, block, block_sym }, sp);
        if let Some(d) = type_err {
            // Not a type after all: an index (`pkg.TABLE[i]`), or else the
            // type's own error, which says more than the index's would.
            return self.index_step(call).map(|x| (x, false)).map_err(|_| d);
        }
        Ok((call, parens))
    }

    /// After `Name[` (cursor past the `[`): type arguments, `]`, and a `.`
    /// (with, if `named`, a name after it): Some(args) if they all fit, None
    /// if the brackets close but no `.` follows, Err on a type that doesn't
    /// parse. The caller rewinds unless it gets Some.
    fn type_args_then_dot(&mut self, named: bool) -> PResult<Option<Vec<TypeExpr>>> {
        let mut targs = vec![];
        loop {
            targs.push(self.type_expr()?);
            if !self.eat_op(",") {
                if !self.eat_op("]") {
                    return Err(Diag::new(self.span(), format!("expected `,` or `]` after a type argument, found {}", describe(self.peek()))));
                }
                let ok = self.is_op(".") && (!named || matches!(self.peek_at(1), Tok::Const(_) | Tok::Ident(_)));
                return Ok(ok.then_some(targs));
            }
        }
    }

    /// A newline followed by a line that starts with `.` or `?.` continues
    /// the expression: `xs\n  .map { it * 2 }`. Moves past the newlines if so.
    fn leading_dot(&mut self) -> bool {
        let mut k = 0;
        while matches!(self.peek_at(k), Tok::Newline) {
            k += 1;
        }
        if k > 0 && matches!(self.peek_at(k), Tok::Op(".") | Tok::Op("?.")) {
            self.pos += k;
            return true;
        }
        false
    }

    fn postfix_from(&mut self, mut e: Expr) -> PResult<Expr> {
        loop {
            self.leading_dot();
            if self.is_op(".") && matches!(self.peek_at(1), Tok::Op("~")) {
                // `x.~m(y)`: propagate this call's error.
                let tsp = self.toks[self.pos + 1].span;
                self.bump();
                self.bump();
                let (call, _) = self.dot_step(e, ".~")?;
                let sp = tsp.to(call.span);
                e = self.mk(ExprKind::Try(Box::new(call)), sp);
                continue;
            }
            if self.is_op("?.") {
                // Optional chaining: the call happens only if the receiver is present.
                self.bump();
                let (call, _) = self.dot_step(e, "?.")?;
                let sp = call.span;
                e = self.mk(ExprKind::OptCall(Box::new(call)), sp);
                continue;
            }
            if self.is_op(".") {
                self.bump();
                e = self.dot_step(e, ".")?.0;
                continue;
            }
            if self.is_op("[") && !self.space_before() {
                e = self.index_step(e)?;
                continue;
            }
            return Ok(e);
        }
    }

    /// `e[i]` / `e[lo..hi]`, at the `[`.
    fn index_step(&mut self, e: Expr) -> PResult<Expr> {
        self.bump();
        // A reslice may leave out either end: `a[2..]`, `a[...n]`.
        let isp = self.span();
        let idx = if self.is_op("..") || self.is_op("...") {
            let excl = self.bump().tok == Tok::Op("...");
            let hi = if self.is_op("]") { None } else { Some(Box::new(self.or()?)) };
            self.mk(ExprKind::SliceRange(None, hi, excl), isp.to(self.prev_span()))
        } else {
            let lo = self.or()?;
            if self.is_op("..") || self.is_op("...") {
                let excl = self.bump().tok == Tok::Op("...");
                let hi = if self.is_op("]") { None } else { Some(Box::new(self.or()?)) };
                self.mk(ExprKind::SliceRange(Some(Box::new(lo)), hi, excl), isp.to(self.prev_span()))
            } else {
                lo
            }
        };
        self.expect_op("]")?;
        let sp = e.span.to(self.prev_span());
        Ok(self.mk(ExprKind::Index(Box::new(e), Box::new(idx)), sp))
    }

    fn call_args(&mut self) -> PResult<(Vec<Expr>, Option<(String, Span)>)> {
        self.expect_op("(")?;
        let saved = std::mem::replace(&mut self.in_cond, false);
        let mut args = vec![];
        let mut sym = None;
        self.skip_newlines();
        while !self.is_op(")") {
            if self.is_op("&:") {
                let sp = self.bump().span;
                let t = self.bump();
                let name = match t.tok {
                    Tok::Ident(n) => n,
                    Tok::Op(o) => o.to_string(),
                    other => return Err(Diag::new(t.span, format!("expected a method name after `&:`, found {}", describe(&other)))),
                };
                sym = Some((name, sp.to(t.span)));
            } else if let (Some(name), Tok::Op(":")) = (match self.peek().clone() {
                Tok::Ident(n) => Some(n),
                Tok::Kw(k) => Some(kw_name(k).to_string()),
                _ => None,
            }, self.peek_at(1).clone()) {
                // `name: value` (a keyword may name a field: `next: n`)
                let nsp = self.bump().span;
                self.bump();
                self.skip_newlines();
                let v = self.expr()?;
                let full = nsp.to(v.span);
                args.push(self.mk(ExprKind::KwArg(name, nsp, Box::new(v)), full));
            } else {
                args.push(self.expr()?);
            }
            self.skip_newlines();
            if !self.eat_op(",") {
                break;
            }
            self.skip_newlines();
        }
        self.expect_op(")")?;
        self.in_cond = saved;
        Ok((args, sym))
    }

    fn maybe_block(&mut self) -> PResult<Option<Box<Block>>> {
        if !self.is_op("{") {
            return Ok(None);
        }
        if self.in_cond && !matches!(self.peek_at(1), Tok::Op("|")) {
            return Ok(None);
        }
        let start = self.bump().span;
        let saved = std::mem::replace(&mut self.in_cond, false);
        self.scopes.push(HashSet::new());
        let mut params = vec![];
        if self.eat_op("|") {
            while !self.is_op("|") {
                let sp = self.span();
                match self.bump().tok {
                    Tok::Ident(n) => {
                        self.declare(&n);
                        params.push((n, sp));
                    }
                    t => return Err(Diag::new(sp, format!("expected a block parameter, found {}", describe(&t)))),
                }
                if !self.eat_op(",") {
                    break;
                }
            }
            self.expect_op("|")?;
        }
        let mut body = vec![];
        loop {
            self.skip_newlines();
            if self.eat_op("}") {
                break;
            }
            if matches!(self.peek(), Tok::Eof) {
                return Err(Diag::new(start, "unterminated block: missing `}`"));
            }
            body.push(self.stmt()?);
        }
        self.scopes.pop();
        self.in_cond = saved;
        let id = self.id();
        Ok(Some(Box::new(Block { id, params, body, span: start.to(self.prev_span()) })))
    }

    fn starts_command_arg(&self) -> bool {
        if !self.space_before() {
            return false;
        }
        match self.peek() {
            Tok::Int(_) | Tok::BigInt(_) | Tok::Float(..) | Tok::Imag(..) | Tok::Str(_) | Tok::Interp(_) | Tok::Cmd(_) | Tok::Ident(_) | Tok::Const(_) | Tok::Sym(_) => true,
            Tok::Kw(Kw::Case) => true,
            Tok::Kw(Kw::True | Kw::False | Kw::Nil | Kw::None | Kw::Try) => true,
            Tok::Op("(") | Tok::Op("[") | Tok::Op("~") | Tok::Op("->") => true,
            // `puts -x` (Ruby): a minus (or `!`) right before its operand starts an argument.
            Tok::Op("-") | Tok::Op("^") | Tok::Op("!") => !self.toks[(self.pos + 1).min(self.toks.len() - 1)].space_before,
            _ => false,
        }
    }

    fn primary(&mut self) -> PResult<Expr> {
        let t = self.bump();
        let sp = t.span;
        Ok(match t.tok {
            Tok::Int(v) => self.mk(ExprKind::Int(v), sp),
            Tok::BigInt(t) => self.mk(ExprKind::BigInt(t), sp),
            Tok::Float(v, t) => self.mk(ExprKind::Float(v, t), sp),
            // `2i`: `Complex.__imag(2.0)`, a Complex with real part +0 (Go).
            Tok::Imag(v, t) => {
                let k = self.mk(ExprKind::Float(v, t), sp);
                let recv = self.mk(ExprKind::Const("Complex".into()), sp);
                self.mk(ExprKind::Call { recv: Some(Box::new(recv)), name: "__imag".into(), name_span: sp, args: vec![k], block: None, block_sym: None }, sp)
            }
            Tok::Str(s) => self.mk(ExprKind::Str(s), sp),
            Tok::Interp(pieces) => self.interp(pieces, sp)?,
            Tok::Cmd(cparts) => {
                // `os/exec.from_literal(groups, kinds)`: a group per word ([w]),
                // per `#{*xs}` (xs) and per operator ([""]).
                // The first thing the literal doesn't support, in source order.
                let mut bads: Vec<&CmdPart> = cparts.iter().filter(|p| matches!(p, CmdPart::Bad(..))).collect();
                bads.sort_by_key(|p| if let CmdPart::Bad(_, _, s) = p { s.lo } else { 0 });
                if let Some(CmdPart::Bad(msg, note, bsp)) = bads.first() {
                    let d = Diag::new(*bsp, msg.clone());
                    return Err(if note.is_empty() { d } else { d.note(note.clone()) });
                }
                let mut groups = vec![];
                let mut kinds = vec![];
                for p in cparts {
                    let (g, k) = match p {
                        CmdPart::Word(pieces, wsp) => {
                            let w = if let [IPiece::Lit(s)] = pieces.as_slice() { self.mk(ExprKind::Str(s.clone()), wsp) } else { self.interp(pieces, wsp)? };
                            (self.mk(ExprKind::Array(vec![w]), wsp), 0)
                        }
                        CmdPart::Splice(code, base, ssp) => (self.code_expr(&code, base, ssp, "`#{*...}`")?, 10),
                        CmdPart::Op(k, osp) => {
                            let e = self.mk(ExprKind::Str(String::new()), osp);
                            (self.mk(ExprKind::Array(vec![e]), osp), k)
                        }
                        CmdPart::Bad(..) => unreachable!(),
                    };
                    groups.push(g);
                    kinds.push(self.mk(ExprKind::Int(k), sp));
                }
                // The file's own `import "os/exec"` unless a local hides its
                // name; else the parser adds one (see `parse`).
                let pkg = match self.cmd_pkg.clone().filter(|a| !self.is_local(a)) {
                    Some(a) => a,
                    None => {
                        self.cmd_own = self.cmd_own.or(Some(sp));
                        CMD_PKG.into()
                    }
                };
                let recv = self.mk(ExprKind::Name(pkg), sp);
                let ga = self.mk(ExprKind::Array(groups), sp);
                let ka = self.mk(ExprKind::Array(kinds), sp);
                self.mk(ExprKind::Call { recv: Some(Box::new(recv)), name: "from_literal".into(), name_span: sp, args: vec![ga, ka], block: None, block_sym: None }, sp)
            }
            Tok::Kw(Kw::Case) => return self.case_rest(sp),
            Tok::Kw(Kw::If) | Tok::Kw(Kw::Unless) => {
                let unless = t.tok == Tok::Kw(Kw::Unless);
                let st = self.if_rest(sp, unless)?;
                let StmtKind::If(c, a, b) = st.kind else { unreachable!() };
                self.mk(ExprKind::If(Box::new(c), a, b), st.span)
            }
            Tok::Sym(s) => self.mk(ExprKind::Sym(s), sp),
            Tok::Kw(Kw::True) => self.mk(ExprKind::Bool(true), sp),
            Tok::Kw(Kw::False) => self.mk(ExprKind::Bool(false), sp),
            Tok::Kw(Kw::Nil) => self.mk(ExprKind::Nil, sp),
            Tok::Kw(Kw::None) => self.mk(ExprKind::None, sp),
            Tok::Const(c) if self.is_op("[") && !self.space_before() => {
                // `Stack[Int].new`: a generic type applied, if what follows fits.
                let save = self.pos;
                self.bump();
                match self.type_args_then_dot(true) {
                    Ok(Some(args)) => self.mk(ExprKind::TypeApp(c, args), sp.to(self.prev_span())),
                    Ok(None) => {
                        self.pos = save;
                        self.mk(ExprKind::Const(c), sp)
                    }
                    Err(d) => {
                        // Not a type after all: an index (`TABLE[i]`), or else
                        // the type's own error (`Stack[Int;].new`).
                        self.pos = save;
                        let k = self.mk(ExprKind::Const(c), sp);
                        self.index_step(k).map_err(|_| d)?
                    }
                }
            }
            Tok::Const(c) => self.mk(ExprKind::Const(c), sp),
            Tok::Op("(") => {
                let saved = std::mem::replace(&mut self.in_cond, false);
                self.skip_newlines();
                let mut e = self.expr()?;
                self.skip_newlines();
                // `(a, b)`: a tuple.
                if self.is_op(",") {
                    let mut items = vec![e];
                    while self.eat_op(",") {
                        self.skip_newlines();
                        if self.is_op(")") {
                            break;
                        }
                        items.push(self.expr()?);
                        self.skip_newlines();
                    }
                    self.expect_op(")")?;
                    self.in_cond = saved;
                    return Ok(self.mk(ExprKind::Tuple(items), sp.to(self.prev_span())));
                }
                self.expect_op(")")?;
                self.in_cond = saved;
                // Parenthesized expressions keep their inner id, but the span
                // covers the parens (used when quoting source).
                e.span = sp.to(self.prev_span());
                e
            }
            Tok::Kw(Kw::Spawn) if !self.is_op("=") => {
                // `spawn { ... }`, or `spawn f(x)` = `spawn { f(x) }`
                if self.is_op("{") {
                    let bsp = self.span();
                    let body = self.braced_stmts()?;
                    let id = self.id();
                    let block = Block { id, params: vec![], body, span: bsp.to(self.prev_span()) };
                    self.mk(ExprKind::Spawn(Box::new(block)), sp.to(self.prev_span()))
                } else {
                    let e = self.expr()?;
                    let id = self.id();
                    let esp = e.span;
                    let block = Block { id, params: vec![], body: vec![Stmt { span: esp, kind: StmtKind::Expr(e) }], span: esp };
                    self.mk(ExprKind::Spawn(Box::new(block)), sp.to(esp))
                }
            }
            Tok::Ident(kw) if kw == "select" && !self.is_local("select") && self.is_op("{") => {
                self.bump();
                let mut arms = vec![];
                let mut default = None;
                loop {
                    self.skip_newlines();
                    if self.eat_op("}") {
                        break;
                    }
                    let asp = self.span();
                    self.scopes.push(HashSet::new());
                    let op = if self.is_kw(Kw::Else) {
                        self.bump();
                        None
                    } else {
                        match self.bump().tok {
                            Tok::Ident(w) if w == "when" => {}
                            t => return Err(Diag::new(asp, format!("expected `when` or `else` in `select`, found {}", describe(&t)))),
                        }
                        // `when v = ch.recv` / `when ch.recv` / `when ch.send(x)` / `when ch << x`
                        let bind = if matches!(self.peek(), Tok::Ident(_)) && matches!(self.peek_at(1), Tok::Op("=")) {
                            let bsp = self.span();
                            let Tok::Ident(n) = self.bump().tok else { unreachable!() };
                            self.bump();
                            Some((n, bsp))
                        } else {
                            None
                        };
                        let e = self.expr()?;
                        let op = match e.kind {
                            ExprKind::Call { recv: Some(ch), name, args, .. } if name == "recv" && args.is_empty() => SelOp::Recv(bind.clone(), *ch),
                            ExprKind::Call { recv: Some(ch), name, mut args, .. } if (name == "send" || name == "<<") && args.len() == 1 && bind.is_none() => SelOp::Send(*ch, args.pop().unwrap()),
                            _ => return Err(Diag::new(e.span, "a `select` case is `when v = ch.recv`, `when ch.recv` or `when ch.send(x)`")),
                        };
                        if let Some((n, _)) = &bind {
                            self.declare(n);
                        }
                        Some(op)
                    };
                    self.expect_op("=>")?;
                    let body = if self.is_op("{") {
                        self.braced_stmts()?
                    } else {
                        let e = self.expr()?;
                        vec![Stmt { span: e.span, kind: StmtKind::Expr(e) }]
                    };
                    self.scopes.pop();
                    match op {
                        Some(op) => arms.push(SelArm { op, body, span: asp.to(self.prev_span()) }),
                        None if default.is_none() => default = Some(body),
                        None => return Err(Diag::new(asp, "a `select` has one `else`")),
                    }
                }
                self.mk(ExprKind::Select(arms, default), sp.to(self.prev_span()))
            }
            Tok::Op("->") => {
                // `->(x: Int, y) -> Int { ... }` / `-> { ... }`
                self.scopes.push(HashSet::new());
                let mut params = vec![];
                if self.eat_op("(") {
                    while !self.is_op(")") {
                        let psp = self.span();
                        let pname = match self.bump().tok {
                            Tok::Ident(n) => n,
                            t => return Err(Diag::new(psp, format!("expected a parameter name, found {}", describe(&t)))),
                        };
                        let ty = if self.eat_op(":") { Some(self.type_expr()?) } else { None };
                        self.declare(&pname);
                        params.push(Param { name: pname, ty, span: psp, default: None });
                        if !self.eat_op(",") {
                            break;
                        }
                    }
                    self.expect_op(")")?;
                }
                let ret = if self.eat_op("->") { Some(self.type_expr()?) } else { None };
                let bsp = self.span();
                let body = self.braced_stmts()?;
                self.scopes.pop();
                let id = self.id();
                let block = Block { id, params: params.iter().map(|p| (p.name.clone(), p.span)).collect(), body, span: bsp.to(self.prev_span()) };
                self.mk(ExprKind::Lambda(params, ret, Box::new(block)), sp.to(self.prev_span()))
            }
            Tok::Op("{") => {
                let mut pairs = vec![];
                let saved = std::mem::replace(&mut self.in_cond, false);
                self.skip_newlines();
                while !self.is_op("}") {
                    let k = if let (Tok::Ident(name), Tok::Op(":")) = (self.peek().clone(), self.peek_at(1).clone()) {
                        let nsp = self.bump().span;
                        self.bump();
                        self.mk(ExprKind::Str(name), nsp)
                    } else {
                        let k = self.expr()?;
                        self.skip_newlines();
                        self.expect_op("=>")?;
                        k
                    };
                    self.skip_newlines();
                    let v = self.expr()?;
                    pairs.push((k, v));
                    self.skip_newlines();
                    if !self.eat_op(",") {
                        break;
                    }
                    self.skip_newlines();
                }
                self.skip_newlines();
                self.expect_op("}")?;
                self.in_cond = saved;
                self.mk(ExprKind::MapLit(pairs), sp.to(self.prev_span()))
            }
            Tok::Op("[") => {
                let mut items = vec![];
                self.skip_newlines();
                if !self.is_op("]") {
                    let first = self.expr()?;
                    if self.eat_op(";") {
                        // `[v; n]`
                        let n = self.or()?;
                        self.expect_op("]")?;
                        return Ok(self.mk(ExprKind::ArrayRepeat(Box::new(first), Box::new(n)), sp.to(self.prev_span())));
                    }
                    items.push(first);
                    self.skip_newlines();
                    if self.eat_op(",") {
                        self.skip_newlines();
                    } else {
                        self.expect_op("]")?;
                        return Ok(self.mk(ExprKind::Array(items), sp.to(self.prev_span())));
                    }
                }
                while !self.is_op("]") {
                    items.push(self.expr()?);
                    self.skip_newlines();
                    if !self.eat_op(",") {
                        break;
                    }
                    self.skip_newlines();
                }
                self.expect_op("]")?;
                self.mk(ExprKind::Array(items), sp.to(self.prev_span()))
            }
            Tok::Kw(k @ (Kw::Fail | Kw::Spawn)) if self.is_op("=") => return Err(reserved(sp, k, "a variable")),
            Tok::Kw(Kw::Fail) if self.at_stmt_end() || self.is_op(")") || self.is_op(",") => return Err(fail_needs_value(sp)),
            Tok::Kw(k @ (Kw::Return | Kw::Break | Kw::Next | Kw::Fail)) => return Err(jump_here(sp, kw_name(k))),
            Tok::Ident(name) => {
                if self.is_local(&name) {
                    // `f(x)` on a local: calling a lambda.
                    if self.is_op("(") && !self.space_before() {
                        let (args, block_sym) = self.call_args()?;
                        let full = sp.to(self.prev_span());
                        return Ok(self.mk(ExprKind::Call { recv: None, name, name_span: sp, args, block: None, block_sym }, full));
                    }
                    return Ok(self.mk(ExprKind::Name(name), sp));
                }
                // Call forms: f(args), f { block }, f arg.
                if self.is_op("(") && !self.space_before() {
                    let (args, block_sym) = self.call_args()?;
                    let block = self.maybe_block()?;
                    let full = sp.to(self.prev_span());
                    return Ok(self.mk(ExprKind::Call { recv: None, name, name_span: sp, args, block, block_sym }, full));
                }
                if self.is_op("{") {
                    let block = self.maybe_block()?;
                    if block.is_some() {
                        let full = sp.to(self.prev_span());
                        return Ok(self.mk(ExprKind::Call { recv: None, name, name_span: sp, args: vec![], block, block_sym: None }, full));
                    }
                }
                if self.starts_command_arg() && name != "it" {
                    let mut args = vec![self.expr()?];
                    while self.eat_op(",") {
                        args.push(self.expr()?);
                    }
                    let full = sp.to(self.prev_span());
                    return Ok(self.mk(ExprKind::Call { recv: None, name, name_span: sp, args, block: None, block_sym: None }, full));
                }
                self.mk(ExprKind::Name(name), sp)
            }
            other => return Err(Diag::new(sp, format!("expected an expression, found {}", describe(&other)))),
        })
    }
}

/// An assignable place: a name, `place[i]`, or `place.field`.
/// `C[i]...`: an element of constant `C`.
fn const_root(e: &Expr) -> Option<&str> {
    match &e.kind {
        ExprKind::Index(a, _) => match &a.kind {
            ExprKind::Const(c) => Some(c),
            _ => const_root(a),
        },
        _ => None,
    }
}

pub fn is_place(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Name(_) => true,
        ExprKind::Index(a, _) => is_place(a),
        ExprKind::Call { recv: Some(r), args, block: None, block_sym: None, .. } if args.is_empty() => is_place(r),
        _ => false,
    }
}

/// Prefix of the temporaries `opt || jump` binds (check.rs names them in errors).
pub const OR_JUMP_TMP: &str = "__or_jump";

/// A jump where a value is wanted.
fn jump_here(sp: Span, kw: &str) -> Diag {
    Diag::new(sp, format!("`{kw}` is a statement, not a value"))
        .note(format!("as part of an expression it can only follow `||` (`v = opt || {kw}`) or be a whole `case` arm (`X => {kw}`) (S8)"))
}

/// `fail` with nothing after it.
fn fail_needs_value(sp: Span) -> Diag {
    Diag::new(sp, "`fail` needs an error value: `fail ParseErr.Bad(pos: 0)`, `fail e`")
}

/// A keyword used as a name it can't be (a local, a parameter, a function).
fn reserved(sp: Span, k: Kw, what: &str) -> Diag {
    Diag::new(sp, format!("`{}` is a keyword and can't name {what}", kw_name(k))).note("only a method may be named after a keyword (`def fail` inside a struct, `t.fail`)")
}

/// A keyword's spelling, for keywords used as method names (`e.next`, `t.fail`).
fn kw_name(k: Kw) -> &'static str {
    match k {
        Kw::Def => "def",
        Kw::If => "if",
        Kw::Unless => "unless",
        Kw::Else => "else",
        Kw::Elsif => "elsif",
        Kw::While => "while",
        Kw::Next => "next",
        Kw::Break => "break",
        Kw::Return => "return",
        Kw::True => "true",
        Kw::False => "false",
        Kw::Nil => "nil",
        Kw::Try => "try",
        Kw::Require => "require",
        Kw::Struct => "struct",
        Kw::Enum => "enum",
        Kw::Interface => "interface",
        Kw::Fn => "fn",
        Kw::For => "for",
        Kw::In => "in",
        Kw::Case => "case",
        Kw::Defer => "defer",
        Kw::Fail => "fail",
        Kw::Spawn => "spawn",
        Kw::None => "none",
    }
}

pub fn describe(t: &Tok) -> String {
    match t {
        Tok::Int(v) => format!("`{v}`"),
        Tok::BigInt(t) => format!("`{t}`"),
        Tok::Float(_, t) => format!("`{t}`"),
        Tok::Imag(_, t) => format!("`{t}i`"),
        Tok::Str(_) | Tok::Interp(_) => "a string".into(),
        Tok::Cmd(_) => "a command literal".into(),
        Tok::Ident(n) | Tok::Const(n) => format!("`{n}`"),
        Tok::Sym(s) => format!("`:{s}`"),
        Tok::Attr(a) => format!("`#[{a}]`"),
        Tok::Directive(n, _) => format!("`#![{n}]`"),
        Tok::Kw(k) => format!("the keyword `{}`", kw_name(*k)),
        Tok::Op(o) => format!("`{o}`"),
        Tok::Newline => "end of line".into(),
        Tok::Eof => "end of file".into(),
    }
}


/// Is `#[...]`'s text an `embed(...)` attribute?
fn is_embed_attr(a: &str) -> bool {
    a.trim_start().strip_prefix("embed").is_some_and(|r| r.trim_start().starts_with('('))
}

/// The patterns of `embed("a.txt", "static/*.html")`: string literals
/// (double-quoted with Go's simple escapes, or backquoted), comma-separated.
fn embed_patterns(a: &str) -> Result<Vec<String>, String> {
    let usage = "write `#[embed(\"file.txt\")]` (one or more quoted patterns, comma-separated)";
    let inner = a.trim().strip_prefix("embed").map(str::trim).and_then(|r| r.strip_prefix('(')).and_then(|r| r.trim_end().strip_suffix(')')).ok_or(usage)?;
    let mut out = vec![];
    let mut cs = inner.trim().chars().peekable();
    loop {
        while cs.peek().is_some_and(|c| c.is_whitespace()) {
            cs.next();
        }
        let Some(q) = cs.next() else { break };
        if q != '"' && q != '`' {
            return Err(usage.into());
        }
        let mut p = String::new();
        loop {
            match cs.next() {
                None => return Err("unterminated pattern in `#[embed(...)]`".into()),
                Some(c) if c == q => break,
                Some('\\') if q == '"' => match cs.next() {
                    Some('n') => p.push('\n'),
                    Some('t') => p.push('\t'),
                    Some(c) => p.push(c),
                    None => return Err("unterminated pattern in `#[embed(...)]`".into()),
                },
                Some(c) => p.push(c),
            }
        }
        if p.is_empty() {
            return Err("an empty pattern in `#[embed(...)]`".into());
        }
        out.push(p);
        while cs.peek().is_some_and(|c| c.is_whitespace()) {
            cs.next();
        }
        match cs.next() {
            None => break,
            Some(',') => {}
            _ => return Err(usage.into()),
        }
    }
    if out.is_empty() {
        return Err(usage.into());
    }
    Ok(out)
}

/// The methods a derive(Data) sees: each recorded method with its
/// `#[data(...)]` options.
fn data_methods(methods: &[Def], mopts: &[(usize, Option<String>, bool)]) -> Vec<crate::derive_data::TMethod> {
    mopts
        .iter()
        .filter(|(i, _, _)| methods[*i].tparams.is_empty())
        .map(|(i, r, sk)| {
            let d = &methods[*i];
            let params = d.params.iter().map(|p| (p.name.clone(), p.ty.clone().unwrap_or(TypeExpr::Named("Unit".into(), p.span)))).collect();
            crate::derive_data::TMethod { name: d.name.clone(), params, ret: d.ret.clone(), fallible: d.fallible, public: d.public, rename: r.clone(), skip: *sk }
        })
        .collect()
}
