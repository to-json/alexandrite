//! Typed tree produced by the checker: one `TFunc` per function instance
//! (monomorphized), every expression typed, every call resolved.

use crate::ast::{BinOp, Overflow};
use crate::diag::Span;

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Ty {
    Int,
    Bool,
    Str,
    Unit,
    Array(Box<Ty>),
    Tuple(Vec<Ty>),
    /// Range[Int]
    Range,
    /// An unmaterialized pipeline: elements of type T. `true` = lazy.
    Seq(Box<Ty>, bool),
    /// Enumerator[T]
    Gen(Box<Ty>),
    /// The `y` inside `Enumerator.new { |y| ... }`.
    Yielder(Box<Ty>),
    Var(u32),
    Never,
}

impl Ty {
    pub fn arr(t: Ty) -> Ty {
        Ty::Array(Box::new(t))
    }
    pub fn seq(t: Ty, lazy: bool) -> Ty {
        Ty::Seq(Box::new(t), lazy)
    }
    pub fn show(&self) -> String {
        match self {
            Ty::Int => "Int".into(),
            Ty::Bool => "Bool".into(),
            Ty::Str => "Str".into(),
            Ty::Unit => "nil".into(),
            Ty::Array(t) => format!("Array[{}]", t.show()),
            Ty::Tuple(ts) => format!("({})", ts.iter().map(Ty::show).collect::<Vec<_>>().join(", ")),
            Ty::Range => "Range[Int]".into(),
            Ty::Seq(t, true) => format!("Lazy[{}]", t.show()),
            Ty::Seq(t, false) => format!("Enumerable[{}]", t.show()),
            Ty::Gen(t) => format!("Enumerator[{}]", t.show()),
            Ty::Yielder(t) => format!("Yielder[{}]", t.show()),
            Ty::Var(_) => "_".into(),
            Ty::Never => "!".into(),
        }
    }
    pub fn has_var(&self) -> bool {
        match self {
            Ty::Var(_) => true,
            Ty::Array(t) | Ty::Seq(t, _) | Ty::Gen(t) | Ty::Yielder(t) => t.has_var(),
            Ty::Tuple(ts) => ts.iter().any(Ty::has_var),
            _ => false,
        }
    }
}

pub type LocalId = usize;
pub type FuncId = usize;

#[derive(Clone, Debug)]
pub struct Local {
    pub name: String,
    pub ty: Ty,
    /// Number of assignments after the first (0 = immutable binding).
    pub reassigned: u32,
    /// Ever mutated in place (`<<`, index assignment).
    pub mutated: bool,
    /// Ever grown with `<<` (its length isn't fixed).
    pub pushed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum M {
    // stages
    Select,
    Reject,
    Map,
    FlatMap,
    TakeWhile,
    Drop,
    Take,
    EachWithIndex,
    Lazy,
    EachIndex,
    EachCons,
    Chars,
    Bytes,
    // terminals
    Sum,
    Max,
    Min,
    MaxBy,
    MinBy,
    First,
    Last,
    ToA,
    Each,
    Reduce,
    All,
    Any,
    Count,
    Include,
    Sort,
    Pmap,
    // scalars and strings
    ToS,
    ToI,
    Size,
    Reverse,
    Delete,
    Split,
    Even,
    Odd,
    Digits,
    Step,
    Push,
    Yield,
    TupleGet(usize),
    IntSqrt,
    ArrayNew,
    FileRead,
    EnumNew,
    Loop,
}

impl M {
    pub fn is_stage(self) -> bool {
        use M::*;
        matches!(self, Select | Reject | Map | FlatMap | TakeWhile | Drop | Take | EachWithIndex | Lazy | EachIndex | EachCons | Chars | Bytes)
    }
}

#[derive(Clone, Debug)]
pub struct TExpr {
    pub kind: TK,
    pub ty: Ty,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum TK {
    Int(i64),
    Str(String),
    Bool(bool),
    Unit,
    Local(LocalId),
    Assign(LocalId, Box<TExpr>),
    IndexAssign(LocalId, Box<TExpr>, Box<TExpr>),
    Bin(BinOp, Box<TExpr>, Box<TExpr>),
    Neg(Box<TExpr>),
    Not(Box<TExpr>),
    Ternary(Box<TExpr>, Box<TExpr>, Box<TExpr>),
    Range(Box<TExpr>, Box<TExpr>, bool),
    Index(Box<TExpr>, Box<TExpr>),
    Call(FuncId, Vec<TExpr>),
    /// Builtin method: receiver, args, block.
    M(M, Option<Box<TExpr>>, Vec<TExpr>, Option<Box<TBlock>>),
    Try(Box<TExpr>),
    Puts(Box<TExpr>),
    Array(Vec<TExpr>),
}

#[derive(Clone, Debug)]
pub struct TBlock {
    pub params: Vec<LocalId>,
    /// Several params over a tuple element: destructure.
    pub destructure: bool,
    pub body: Vec<TStmt>,
    /// No I/O and no impure calls (fusion is legal).
    pub pure: bool,
    pub span: Span,
    /// Locals declared inside this block: ids in [own.0, own.1).
    pub own: (usize, usize),
}

#[derive(Clone, Debug)]
pub enum TStmt {
    Expr(TExpr),
    MultiAssign(Vec<LocalId>, Vec<TExpr>),
    While(TExpr, Vec<TStmt>),
    If(TExpr, Vec<TStmt>, Vec<TStmt>),
    Next(Span),
    Break(Option<TExpr>, Span),
    Return(Option<TExpr>, Span),
}

#[derive(Clone, Debug)]
pub struct TFunc {
    /// Unique, C-safe name for this instance.
    pub cname: String,
    pub src_name: String,
    pub params: Vec<LocalId>,
    pub locals: Vec<Local>,
    pub ret: Ty,
    pub fallible: bool,
    pub pure: bool,
    /// Does I/O, directly or through calls (inferred).
    pub io: bool,
    pub body: Vec<TStmt>,
    pub overflow: Overflow,
    pub span: Span,
    /// Defined in a separately compiled library: emit an extern prototype.
    pub external: bool,
    pub is_main: bool,
}

pub struct TProgram {
    pub funcs: Vec<TFunc>,
    pub main: FuncId,
}
