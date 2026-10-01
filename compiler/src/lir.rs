//! Low-level IR shared by the C backend and the Rust oracle: structured
//! statements, explicit loops with labels, explicit checks.

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum LTy {
    I64,
    /// Int in a `#![overflow(promote)]` file: small or bignum.
    PInt,
    Bool,
    Str,
    Unit,
    Arr(Box<LTy>),
    Tup(Vec<LTy>),
    Range,
    Gen(Box<LTy>),
}

pub type V = usize;
pub type Label = usize;

#[derive(Clone, Debug)]
pub struct LVar {
    pub name: String,
    pub ty: LTy,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
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

/// What happens when an Int operation overflows.
#[derive(Clone, Debug, PartialEq)]
pub enum Ovf {
    /// Abort with a message naming the location (default mode).
    Panic(String),
    /// Two's-complement wrap (`#![overflow(wrap)]`).
    Wrap,
    /// Proven not to overflow: plain machine arithmetic.
    Unchecked,
}

#[derive(Clone, Debug)]
pub enum LE {
    Var(V),
    I(i64),
    B(bool),
    S(String),
    /// A source location string (C: `const char *`).
    Loc(String),
    Unit,
    Tup(LTy, Vec<LE>),
    Field(Box<LE>, usize),
    /// Integer arithmetic on I64.
    Arith(Op, Box<LE>, Box<LE>, Ovf),
    /// Arithmetic on PInt (promote mode); comparisons too.
    PArith(Op, Box<LE>, Box<LE>),
    /// Comparisons and boolean ops (no overflow possible), any scalar type.
    Cmp(Op, Box<LE>, Box<LE>, LTy),
    Neg(Box<LE>, Ovf),
    Not(Box<LE>),
    Cond(Box<LE>, Box<LE>, Box<LE>),
    /// Call a non-fallible user function.
    Call(String, Vec<LE>),
    /// Runtime helper (named per backend).
    Rt(Rt, Vec<LE>),
    Index { arr: Box<LE>, idx: Box<LE>, check: Option<String> },
    Len(Box<LE>),
    /// Array literal / sized constructor.
    ArrLit(LTy, Vec<LE>),
    ArrNew(LTy, Box<LE>, Box<LE>, String),
    ArrWithCap(LTy, Box<LE>),
    /// View of `len` elements of an array starting at `start` (no copy).
    Slice(LTy, Box<LE>, Box<LE>, Box<LE>),
    Range(Box<LE>, Box<LE>, bool),
    RangeField(Box<LE>, u8),
    /// Create generator `id`, capturing these values.
    GenNew(usize, Vec<LE>),
    /// Convert I64 to PInt.
    ToP(Box<LE>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rt {
    IntToS,
    PIntToS,
    StrRev,
    StrDelete,
    StrSplit,
    StrToI,
    StrChar,
    StrByte,
    StrLen,
    Isqrt,
    Digits,
    PDigits,
    SatAdd,
    ArrCopy,
    Even,
    PEven,
    PToI64,
}

#[derive(Clone, Debug)]
pub enum ErrPath {
    /// In a fallible function: return the error.
    Return,
    /// At the top level: print the error and exit 1.
    Die,
}

#[derive(Clone, Debug)]
pub enum LS {
    Set(V, LE),
    SetIndex { arr: V, idx: LE, val: LE, check: Option<String> },
    Push(V, LE),
    Eval(LE),
    If(LE, Vec<LS>, Vec<LS>),
    Loop(Label, Vec<LS>),
    Break(Label),
    Continue(Label),
    Return(Option<LE>),
    /// `dst = a op b`, failing to the error path on overflow (`try`).
    TryArith { dst: V, op: Op, a: LE, b: LE, loc: String, path: ErrPath },
    /// `dst = f(args)` for a fallible user function.
    TryCall { dst: Option<V>, f: String, args: Vec<LE>, path: ErrPath },
    /// `dst = File.read(path)`.
    TryRead { dst: V, path_arg: LE, loc: String, path: ErrPath },
    /// Pull the next element of a generator into `dst`, or break `label`.
    NextOrBreak { source: LE, dst: V, label: Label },
    Yield(LE),
    /// `dst = arr.pmap { worker }`.
    Pmap { dst: V, arr: LE, worker: usize, path: ErrPath },
    Puts(LE, LTy),
    Panic(String, String),
    SortInPlace(V, LTy),
}

#[derive(Clone, Debug)]
pub struct LFunc {
    pub name: String,
    pub params: Vec<V>,
    pub vars: Vec<LVar>,
    pub ret: LTy,
    pub fallible: bool,
    pub body: Vec<LS>,
    pub external: bool,
    pub is_main: bool,
    pub labels: usize,
}

/// A generator: a resumable body with every variable hoisted into its state.
#[derive(Clone, Debug)]
pub struct LGen {
    pub id: usize,
    pub elem: LTy,
    /// Variables of `func` that are initialized from the creator's values.
    pub captures: Vec<V>,
    pub func: LFunc,
}

/// A `pmap` worker: one element in (param 0), one value out.
#[derive(Clone, Debug)]
pub struct LWorker {
    pub id: usize,
    pub input: LTy,
    pub func: LFunc,
}

#[derive(Clone, Debug, Default)]
pub struct LProgram {
    pub funcs: Vec<LFunc>,
    pub gens: Vec<LGen>,
    pub workers: Vec<LWorker>,
    /// Uses bignums: link the bignum runtime.
    pub uses_pint: bool,
}
