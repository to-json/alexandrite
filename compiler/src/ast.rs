//! Syntax tree. Every node that can be typed carries a unique `id`.

use crate::diag::Span;

pub type NodeId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overflow {
    Abort,
    Wrap,
    Promote,
}

#[derive(Debug)]
pub struct Module {
    pub file: u32,
    pub overflow: Overflow,
    pub requires: Vec<(String, Span)>,
    pub defs: Vec<Def>,
    pub structs: Vec<StructDef>,
    /// Top-level statements, run in order (the implicit `main`).
    pub main: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub struct Def {
    pub name: String,
    pub span: Span,
    pub name_span: Span,
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    pub fallible: bool,
    pub pure: bool,
    pub body: Vec<Stmt>,
}

/// `struct Name { field: Type, ... }`: a value type.
#[derive(Debug, Clone)]
pub struct StructDef {
    pub name: String,
    pub span: Span,
    pub fields: Vec<(String, TypeExpr, Span)>,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub ty: Option<TypeExpr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum TypeExpr {
    Named(String, Span),
    Array(Box<TypeExpr>, Span),
}

#[derive(Debug, Clone)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum StmtKind {
    Expr(Expr),
    /// `a, b = x, y` (also the single-target case when written with commas).
    MultiAssign(Vec<(String, Span)>, Vec<Expr>),
    While(Expr, Vec<Stmt>),
    If(Expr, Vec<Stmt>, Vec<Stmt>),
    Next,
    Break(Option<Expr>),
    Return(Option<Expr>),
}

#[derive(Debug, Clone)]
pub struct Block {
    pub id: NodeId,
    /// Declared params; empty with `uses_it` means the implicit `it`.
    pub params: Vec<(String, Span)>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Expr {
    pub id: NodeId,
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

impl BinOp {
    pub fn text(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            BinOp::Pow => "**",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "&&",
            BinOp::Or => "||",
        }
    }
    pub fn is_arith(self) -> bool {
        matches!(self, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem | BinOp::Pow)
    }
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Nil,
    Sym(String),
    /// A bare identifier: a local, or a call to a zero-argument function.
    Name(String),
    Const(String),
    Call {
        recv: Option<Box<Expr>>,
        name: String,
        name_span: Span,
        args: Vec<Expr>,
        block: Option<Box<Block>>,
        /// `&:sym`
        block_sym: Option<(String, Span)>,
    },
    Index(Box<Expr>, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Range(Box<Expr>, Box<Expr>, bool),
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    /// `x = e`, `a[i] = e`, `p.f = e`, `a[i].f = e`: the target is a Name, an
    /// Index, or a field (a Call with no arguments) over a place.
    Assign(Box<Expr>, Box<Expr>),
    /// `x += e` etc.: desugared by the parser to Assign(x, Binary(op, x, e)),
    /// but remembered for the counter rule.
    OpAssign(BinOp, Box<Expr>, Box<Expr>),
    Try(Box<Expr>),
    Array(Vec<Expr>),
    /// `name: value` in an argument list (`Body.new(x: 1.0)`).
    KwArg(String, Span, Box<Expr>),
}
