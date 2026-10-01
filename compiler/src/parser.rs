//! Recursive-descent parser. Tracks local variable names the way Ruby's
//! parser does, so `name arg` (a command call) and `name` (a local) can be
//! told apart.

use crate::ast::*;
use crate::diag::{Diag, Span};
use crate::lexer::{Kw, Tok, Token};
use std::collections::HashSet;

pub struct Parser<'a> {
    toks: &'a [Token],
    pos: usize,
    next_id: &'a mut NodeId,
    scopes: Vec<HashSet<String>>,
    /// Inside an `if`/`while` condition: `{` is a block only if `|` follows.
    in_cond: bool,
}

type PResult<T> = Result<T, Diag>;

pub fn parse(file: u32, toks: &[Token], next_id: &mut NodeId) -> PResult<Module> {
    let mut p = Parser { toks, pos: 0, next_id, scopes: vec![HashSet::new()], in_cond: false };
    p.module(file)
}

impl<'a> Parser<'a> {
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
        let mut m = Module { file, overflow: Overflow::Abort, requires: vec![], defs: vec![], main: vec![] };
        self.skip_newlines();
        while let Tok::Directive(name, arg) = self.peek().clone() {
            let sp = self.bump().span;
            match (name.as_str(), arg.as_str()) {
                ("overflow", "abort") => m.overflow = Overflow::Abort,
                ("overflow", "wrap") => m.overflow = Overflow::Wrap,
                ("overflow", "promote") => m.overflow = Overflow::Promote,
                _ => return Err(Diag::new(sp, format!("unknown directive `#![{name}({arg})]`")).note("known: #![overflow(abort | wrap | promote)]")),
            }
            self.skip_newlines();
        }
        loop {
            self.skip_newlines();
            match self.peek().clone() {
                Tok::Eof => break,
                Tok::Kw(Kw::Require) => {
                    self.bump();
                    let sp = self.span();
                    match self.bump().tok {
                        Tok::Str(s) => m.requires.push((s, sp)),
                        _ => return Err(Diag::new(sp, "`require` takes a string path")),
                    }
                }
                Tok::Attr(_) | Tok::Kw(Kw::Def) => m.defs.push(self.def()?),
                Tok::Directive(..) => return Err(Diag::new(self.span(), "directives must come first in the file")),
                _ => {
                    let s = self.stmt()?;
                    m.main.push(s);
                }
            }
        }
        Ok(m)
    }

    fn def(&mut self) -> PResult<Def> {
        let start = self.span();
        let mut pure = false;
        while let Tok::Attr(a) = self.peek().clone() {
            let sp = self.bump().span;
            match a.as_str() {
                "pure" => pure = true,
                _ => return Err(Diag::new(sp, format!("unknown attribute `#[{a}]`"))),
            }
            self.skip_newlines();
        }
        if !self.is_kw(Kw::Def) {
            return Err(Diag::new(self.span(), "expected `def` after attribute"));
        }
        self.bump();
        let name_span = self.span();
        let name = match self.bump().tok {
            Tok::Ident(n) => n,
            t => return Err(Diag::new(name_span, format!("expected a method name, found {}", describe(&t)))),
        };
        self.scopes.push(HashSet::new());
        let mut params = vec![];
        if self.is_op("(") && !self.space_before() {
            self.bump();
            while !self.is_op(")") {
                let sp = self.span();
                let pname = match self.bump().tok {
                    Tok::Ident(n) => n,
                    t => return Err(Diag::new(sp, format!("expected a parameter name, found {}", describe(&t)))),
                };
                let ty = if self.eat_op(":") { Some(self.type_expr()?) } else { None };
                self.declare(&pname);
                params.push(Param { name: pname, ty, span: sp });
                if !self.eat_op(",") {
                    break;
                }
            }
            self.expect_op(")")?;
        }
        let (mut ret, mut fallible) = (None, false);
        if self.eat_op("->") {
            ret = Some(self.type_expr()?);
            if self.eat_op("!") {
                fallible = true;
            }
        }
        let body = self.braced_stmts()?;
        self.scopes.pop();
        Ok(Def { name, span: start.to(self.prev_span()), name_span, params, ret, fallible, pure, body })
    }

    fn type_expr(&mut self) -> PResult<TypeExpr> {
        let sp = self.span();
        if self.eat_op("[") {
            let inner = self.type_expr()?;
            self.expect_op("]")?;
            return Ok(TypeExpr::Array(Box::new(inner), sp.to(self.prev_span())));
        }
        match self.bump().tok {
            Tok::Const(n) => Ok(TypeExpr::Named(n, sp)),
            t => Err(Diag::new(sp, format!("expected a type, found {}", describe(&t)))),
        }
    }

    fn braced_stmts(&mut self) -> PResult<Vec<Stmt>> {
        self.expect_op("{")?;
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
                StmtKind::Return(if self.at_stmt_end() { None } else { Some(self.expr()?) })
            }
            Tok::Kw(Kw::Def) | Tok::Attr(_) => return Err(Diag::new(start, "methods can only be defined at the top level")),
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
                let mut values = vec![self.expr()?];
                while self.eat_op(",") {
                    values.push(self.expr()?);
                }
                for (n, _) in &targets {
                    self.declare(n);
                }
                StmtKind::MultiAssign(targets, values)
            }
            _ => StmtKind::Expr(self.expr()?),
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
            els = self.braced_stmts()?;
        } else if self.is_kw(Kw::Elsif) {
            let sp = self.bump().span;
            els = vec![self.if_rest(sp, false)?];
        } else {
            self.pos = save;
        }
        Ok(Stmt { kind: StmtKind::If(cond, then, els), span: start.to(self.prev_span()) })
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
            if !matches!(lhs.kind, ExprKind::Name(_) | ExprKind::Index(..)) {
                return Err(Diag::new(lhs.span, "cannot assign to this expression"));
            }
            self.bump();
            if let ExprKind::Name(n) = &lhs.kind {
                let n = n.clone();
                self.declare(&n);
            }
            let rhs = self.expr()?;
            let sp = lhs.span.to(rhs.span);
            return Ok(self.mk(ExprKind::Assign(Box::new(lhs), Box::new(rhs)), sp));
        }
        for (tok, op) in [("+=", BinOp::Add), ("-=", BinOp::Sub), ("*=", BinOp::Mul)] {
            if self.is_op(tok) {
                self.bump();
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
        self.binary_level(&[("||", BinOp::Or)], Self::and)
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
        self.equality()
    }
    fn equality(&mut self) -> PResult<Expr> {
        self.binary_level(&[("==", BinOp::Eq), ("!=", BinOp::Ne)], Self::comparison)
    }
    fn comparison(&mut self) -> PResult<Expr> {
        self.binary_level(&[("<=", BinOp::Le), (">=", BinOp::Ge), ("<", BinOp::Lt), (">", BinOp::Gt)], Self::shift)
    }
    fn shift(&mut self) -> PResult<Expr> {
        let mut l = self.additive()?;
        while self.is_op("<<") {
            let op_span = self.bump().span;
            let r = self.additive()?;
            let sp = l.span.to(r.span);
            l = self.mk(
                ExprKind::Call { recv: Some(Box::new(l)), name: "<<".into(), name_span: op_span, args: vec![r], block: None, block_sym: None },
                sp,
            );
        }
        Ok(l)
    }
    fn additive(&mut self) -> PResult<Expr> {
        self.binary_level(&[("+", BinOp::Add), ("-", BinOp::Sub)], Self::multiplicative)
    }
    fn multiplicative(&mut self) -> PResult<Expr> {
        self.binary_level(&[("*", BinOp::Mul), ("/", BinOp::Div), ("%", BinOp::Rem)], Self::unary)
    }
    fn unary(&mut self) -> PResult<Expr> {
        if self.is_op("-") {
            let sp = self.bump().span;
            let e = self.unary()?;
            let full = sp.to(e.span);
            if let ExprKind::Int(v) = e.kind {
                return Ok(self.mk(ExprKind::Int(-v), full));
            }
            return Ok(self.mk(ExprKind::Neg(Box::new(e)), full));
        }
        if self.is_kw(Kw::Try) {
            let sp = self.bump().span;
            let e = self.unary()?;
            let full = sp.to(e.span);
            return Ok(self.mk(ExprKind::Try(Box::new(e)), full));
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
        let mut e = self.primary()?;
        loop {
            if self.is_op(".") {
                self.bump();
                self.skip_line_continuation();
                let name_span = self.span();
                let name = match self.bump().tok {
                    Tok::Ident(n) => n,
                    Tok::Const(n) if n == "new" => n,
                    t => return Err(Diag::new(name_span, format!("expected a method name after `.`, found {}", describe(&t)))),
                };
                let (args, block_sym) = if self.is_op("(") && !self.space_before() { self.call_args()? } else { (vec![], None) };
                let block = self.maybe_block()?;
                let sp = e.span.to(self.prev_span());
                e = self.mk(ExprKind::Call { recv: Some(Box::new(e)), name, name_span, args, block, block_sym }, sp);
                continue;
            }
            if self.is_op("[") && !self.space_before() {
                self.bump();
                let idx = self.expr()?;
                self.expect_op("]")?;
                let sp = e.span.to(self.prev_span());
                e = self.mk(ExprKind::Index(Box::new(e), Box::new(idx)), sp);
                continue;
            }
            return Ok(e);
        }
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
            Tok::Int(_) | Tok::Str(_) | Tok::Ident(_) | Tok::Const(_) | Tok::Sym(_) => true,
            Tok::Kw(Kw::True | Kw::False | Kw::Nil | Kw::Try) => true,
            Tok::Op("(") | Tok::Op("[") => true,
            _ => false,
        }
    }

    fn primary(&mut self) -> PResult<Expr> {
        let t = self.bump();
        let sp = t.span;
        Ok(match t.tok {
            Tok::Int(v) => self.mk(ExprKind::Int(v), sp),
            Tok::Str(s) => self.mk(ExprKind::Str(s), sp),
            Tok::Sym(s) => self.mk(ExprKind::Sym(s), sp),
            Tok::Kw(Kw::True) => self.mk(ExprKind::Bool(true), sp),
            Tok::Kw(Kw::False) => self.mk(ExprKind::Bool(false), sp),
            Tok::Kw(Kw::Nil) => self.mk(ExprKind::Nil, sp),
            Tok::Const(c) => self.mk(ExprKind::Const(c), sp),
            Tok::Op("(") => {
                let saved = std::mem::replace(&mut self.in_cond, false);
                self.skip_newlines();
                let mut e = self.expr()?;
                self.skip_newlines();
                self.expect_op(")")?;
                self.in_cond = saved;
                // Parenthesized expressions keep their inner id, but the span
                // covers the parens (used when quoting source).
                e.span = sp.to(self.prev_span());
                e
            }
            Tok::Op("[") => {
                let mut items = vec![];
                self.skip_newlines();
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
            Tok::Ident(name) => {
                if self.is_local(&name) {
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

pub fn describe(t: &Tok) -> String {
    match t {
        Tok::Int(v) => format!("`{v}`"),
        Tok::Str(_) => "a string".into(),
        Tok::Ident(n) | Tok::Const(n) => format!("`{n}`"),
        Tok::Sym(s) => format!("`:{s}`"),
        Tok::Attr(a) => format!("`#[{a}]`"),
        Tok::Directive(n, _) => format!("`#![{n}]`"),
        Tok::Kw(k) => format!("`{}`", format!("{k:?}").to_lowercase()),
        Tok::Op(o) => format!("`{o}`"),
        Tok::Newline => "end of line".into(),
        Tok::Eof => "end of file".into(),
    }
}
