//! Typed tree produced by the checker: one `TFunc` per function instance
//! (monomorphized), every expression typed, every call resolved.

pub use crate::ast::IntKind;
use crate::ast::{BinOp, Overflow};
use num_bigint::BigInt;
use num_rational::BigRational;
use crate::diag::Span;

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Ty {
    /// I64, the default integer.
    Int,
    /// Every other integer type (never I64).
    IntK(IntKind),
    Float,
    Bool,
    Str,
    Unit,
    Array(Box<Ty>),
    Tuple(Vec<Ty>),
    /// A user struct (a value): its name and fields, in order.
    Struct(String, Vec<(String, Ty)>),
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
            Ty::IntK(k) => k.name().into(),
            Ty::Float => "Float".into(),
            Ty::Struct(n, _) => n.clone(),
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
    /// The integer kind of an integer type.
    pub fn int_kind(&self) -> Option<IntKind> {
        match self {
            Ty::Int => Some(IntKind::I64),
            Ty::IntK(k) => Some(*k),
            _ => None,
        }
    }
    pub fn of_kind(k: IntKind) -> Ty {
        if k == IntKind::I64 { Ty::Int } else { Ty::IntK(k) }
    }
    pub fn field(&self, name: &str) -> Option<(usize, Ty)> {
        match self {
            Ty::Struct(_, fs) => fs.iter().position(|(f, _)| f == name).map(|k| (k, fs[k].1.clone())),
            _ => None,
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
    /// Int → Float
    ToF,
    /// Float → Int (truncates; fails on NaN, infinities and out-of-range values)
    FloatToI,
    /// Integer or Float → integer kind: `to_u8` (checked) or `as_u8` (wraps, as Go's `uint8(x)`)
    Conv(IntKind, bool),
    FloatAbs,
    FloatToS,
    /// Math.sqrt
    Sqrt,
    /// `Name.new(fields...)`
    StructNew,
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
    /// A typed integer literal (bit pattern for U64).
    Int(i64),
    Float(f64),
    /// An untyped constant (Go): exact until its use gives it a type.
    Const(ConstVal),
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
    /// `place = v`, or `place op= v`: a local, then index and field steps.
    PlaceAssign(LocalId, Vec<TStep>, Option<BinOp>, Box<TExpr>),
    /// `format("...", args)`: pieces checked against the arguments.
    Format(Vec<FmtPiece>, Vec<TExpr>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConstVal {
    Int(BigInt),
    Float(BigRational),
}

#[derive(Clone, Debug)]
pub enum TStep {
    Index(TExpr),
    Field(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub enum FmtPiece {
    Lit(String),
    /// `%d`: argument k (Int)
    Int(usize),
    /// `%v`, `%s`, `%t`: argument k as Go's fmt shows it
    Str(usize),
    /// `%f`, `%.Nf`: argument k (Float or Int), N decimals
    Fixed(usize, u32),
    /// `%x` `%X` `%o` `%b`: argument k in a base
    Base(usize, u32, bool),
    /// `%c`: argument k as a character (a Rune)
    Char(usize),
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
