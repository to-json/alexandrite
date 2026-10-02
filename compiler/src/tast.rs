//! Typed tree produced by the checker: one `TFunc` per function instance
//! (monomorphized), every expression typed, every call resolved.

pub use std::collections::HashMap;
use crate::ast::IntKind;
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
    /// `[T]`: a slice (a window onto shared storage).
    Array(Box<Ty>),
    /// `[T; N]`: a fixed-length array, a value (copied on assignment).
    Fixed(Box<Ty>, u64),
    /// `enum`: its name and variants (each with its fields). A value: the
    /// tag, then every variant's fields side by side.
    Enum(String, Vec<(String, Vec<(String, Ty)>)>),
    /// Any error value: one of the program's `error` types (and the
    /// builtin ones), with where it happened and any `wrap` context.
    Error,
    /// `~T` held as a value: a T or an Error.
    Result(Box<Ty>),
    /// A spawned task whose body produces this (a `~T` if it uses `~`).
    Task(Box<Ty>),
    /// A channel of T.
    Chan(Box<Ty>),
    /// A function value (`(A, B) -> R`): one of the program's lambda
    /// literals of this type, with its captured values.
    Fn(Vec<Ty>, Box<Ty>),
    /// An interface value: one of the types converted to it (closed world:
    /// the program's implementors, see `TProgram::ifaces`).
    Iface(String),
    /// `Map[K, V]`: ordered (insertion order), shared like a Go map.
    Map(Box<Ty>, Box<Ty>),
    Tuple(Vec<Ty>),
    /// `T?`: a T or nothing (no nil anywhere else).
    Opt(Box<Ty>),
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
            Ty::Struct(n, _) | Ty::Enum(n, _) | Ty::Iface(n) => n.clone(),
            Ty::Opt(t) => format!("{}?", t.show()),
            Ty::Bool => "Bool".into(),
            Ty::Str => "Str".into(),
            Ty::Unit => "nil".into(),
            Ty::Array(t) => format!("[{}]", t.show()),
            Ty::Fixed(t, n) => format!("[{}; {n}]", t.show()),
            Ty::Map(k, v) => format!("Map[{}, {}]", k.show(), v.show()),
            Ty::Error => "Error".into(),
            Ty::Result(t) => format!("~{}", t.show()),
            Ty::Task(t) => format!("Task[{}]", t.show()),
            Ty::Chan(t) => format!("Chan[{}]", t.show()),
            Ty::Fn(ps, r) => format!("({}) -> {}", ps.iter().map(Ty::show).collect::<Vec<_>>().join(", "), r.show()),
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
            Ty::Array(t) | Ty::Fixed(t, _) | Ty::Seq(t, _) | Ty::Gen(t) | Ty::Yielder(t) | Ty::Opt(t) => t.has_var(),
            Ty::Tuple(ts) => ts.iter().any(Ty::has_var),
            Ty::Map(k, v) => k.has_var() || v.has_var(),
            Ty::Fn(ps, r) => ps.iter().any(Ty::has_var) || r.has_var(),
            Ty::Result(t) | Ty::Task(t) | Ty::Chan(t) => t.has_var(),
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
    /// The name of a struct or enum (methods live under it).
    pub fn type_name(&self) -> Option<&str> {
        match self {
            // A generic instance (`Stack[Int]`) has its type's methods.
            Ty::Struct(n, _) | Ty::Enum(n, _) => Some(n.split('[').next().unwrap_or(n)),
            _ => None,
        }
    }
    /// An enum's flattened slots: variant `k`'s first field is at slot
    /// `enum_slot(k)` of the value (slot 0 is the tag).
    pub fn enum_slot(&self, k: usize) -> usize {
        let Ty::Enum(_, vs) = self else { panic!("not an enum") };
        1 + vs[..k].iter().map(|(_, fs)| fs.len()).sum::<usize>()
    }
    /// The element type of a slice or fixed array.
    pub fn arr_elem(&self) -> Option<Ty> {
        match self {
            Ty::Array(t) | Ty::Fixed(t, _) => Some((**t).clone()),
            _ => None,
        }
    }
    /// Copied on assignment: fixed arrays, and structs/tuples holding one.
    pub fn is_value_array(&self) -> bool {
        match self {
            Ty::Fixed(..) => true,
            Ty::Struct(_, fs) => fs.iter().any(|(_, t)| t.is_value_array()),
            Ty::Tuple(ts) => ts.iter().any(Ty::is_value_array),
            Ty::Enum(_, vs) => vs.iter().any(|(_, fs)| fs.iter().any(|(_, t)| t.is_value_array())),
            Ty::Opt(t) => t.is_value_array(),
            _ => false,
        }
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
    /// Introduced by an assignment in the source (unused ones warn).
    pub user: bool,
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
    /// T? → Bool
    OptPresent,
    /// T? → T, unchecked (only after a presence test)
    OptGet,
    /// T? → T, panics on none
    Unwrap,
    ArrayNew,
    /// `xs.dup`: a copy with its own storage.
    Dup,
    /// `Str.from_bytes(xs)`.
    FromBytes,
    /// `copy(dst, src)`: Go's copy, returns the count.
    CopyInto,
    /// `s.runes`: the code points of a Str (U+FFFD for invalid bytes).
    Runes,
    /// Maps: `{k => v, ...}` (args: k1, v1, k2, v2, ...), `m[k]` (V?),
    /// `m.fetch(k, d)`, `m[k] = v`, `delete` (V?), `key?`, `size`, `keys`, `values`.
    /// An enum value's variant index.
    EnumTag,
    /// An error enum value as an `Error` (k = its index in `TProgram::errors`).
    ToError(usize),
    /// `e.message` (with any wrap context).
    ErrMessage,
    /// `e.wrap(ctx)`.
    ErrWrap,
    /// Is `Error` value of error type `k`? / its value as that type.
    ErrIs(usize),
    ErrAs(usize),
    /// Results: `ok` (T?), `err` (Error?), `ok?`, `unwrap`, `unwrap_or(d)`,
    /// `rescue { |e| v }`.
    ResOk,
    ResErr,
    ResIsOk,
    ResUnwrap,
    ResUnwrapOr,
    ResRescue,
    /// `spawn { }` (the block); args are its captured locals.
    Spawn,
    /// `t.wait` → ~T.
    TaskWait,
    /// `Chan[T].new(cap)`; `send`/`<<`, `recv` (T?), `close`, `size`.
    ChanNew,
    ChanSend,
    ChanRecv,
    ChanClose,
    ChanLen,
    /// A lambda literal (the block); args are its captured locals.
    Lambda,
    /// Call a function value: recv is the function, args its arguments.
    FnCall,
    /// Wrap a concrete value as implementor `k` of its interface type.
    ToIface(usize),
    /// Call method `k` (declaration order) of an interface value.
    IfaceCall(usize),
    /// Build variant `k` of an enum: args are every slot after the tag.
    VariantNew(usize),
    /// `find { pred }` → T? (a select stage, then this terminal).
    Find,
    MapNew,
    MapGet,
    MapGetOr,
    MapSet,
    MapDel,
    MapHas,
    MapSize,
    MapKeys,
    MapValues,
    FileRead,
    EnumNew,
    Loop,
}

impl M {
    pub fn is_stage(self) -> bool {
        use M::*;
        matches!(self, Select | Reject | Map | FlatMap | TakeWhile | Drop | Take | EachWithIndex | Lazy | EachIndex | EachCons | Chars | Bytes | Runes)
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
    /// `a[lo..hi]` / `a[lo...hi]` on a slice, fixed array or Str (ends optional).
    Slice(Box<TExpr>, Option<Box<TExpr>>, Option<Box<TExpr>>, bool),
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
    /// Statements whose value is the last one's (a `case` arm, a desugaring).
    Seq(Vec<TStmt>),
    /// The absent value of a T?.
    None,
    /// A present T?.
    Some(Box<TExpr>),
    /// `select { when ... }`: the arms, then the `else` body if any.
    Select(Vec<TSelArm>, Option<Vec<TStmt>>),
}

#[derive(Clone, Debug)]
pub enum TSelArm {
    /// `when v = ch.recv`: binds the local (a T?) for the body.
    Recv { ch: TExpr, bind: Option<LocalId>, body: Vec<TStmt> },
    Send { ch: TExpr, val: TExpr, body: Vec<TStmt> },
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
    /// `fail e` (an `Error` value).
    Fail(TExpr, Span),
    /// Run when the enclosing block exits.
    Defer(TExpr),
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
    /// Lambda literals in this function: (block span start, fn type, captured locals).
    pub lambdas: Vec<(u32, Ty, Vec<LocalId>)>,
    /// The error types it can fail with ("Error" = any).
    pub errs: Vec<String>,
}

pub struct TProgram {
    pub funcs: Vec<TFunc>,
    pub main: FuncId,
    /// Each interface's implementors, in tag order, with the instance of
    /// each of its methods (in declaration order).
    pub ifaces: HashMap<String, Vec<(Ty, Vec<FuncId>)>>,
    /// `to_s` instances of types that are printed (by `Ty::show`), so
    /// printing a slice of them uses each element's `to_s` (Go's Stringer).
    pub stringers: HashMap<String, FuncId>,
    /// Every error type (enums), in tag order; builtins first.
    pub errors: Vec<Ty>,
    /// Error types with a `message` method: index → its instance.
    pub messages: HashMap<usize, FuncId>,
    /// Unused locals and imports (errors under `--strict`).
    pub warnings: Vec<crate::diag::Diag>,
}
