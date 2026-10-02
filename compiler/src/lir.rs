//! Low-level IR shared by the C backend and the Rust oracle: structured
//! statements, explicit loops with labels, explicit checks.

pub use crate::ast::IntKind;

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum LTy {
    I64,
    /// A Go integer type other than I64. In registers it is an i64 holding
    /// the value (sign- or zero-extended; U64 as its bit pattern); in arrays
    /// and structs it takes its own width.
    IntK(IntKind),
    /// Float: an IEEE double.
    F64,
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

impl LTy {
    /// The element type of an array type.
    pub fn arr_elem_lty(self) -> LTy {
        match self {
            LTy::Arr(t) => *t,
            t => panic!("not an array: {t:?}"),
        }
    }
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
    F(f64),
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
    /// Float arithmetic (IEEE: never fails).
    FArith(Op, Box<LE>, Box<LE>),
    /// A primitive operation on i64 registers (see `Prim`).
    Prim(Prim, Vec<LE>),
    FNeg(Box<LE>),
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
    /// `x.to_s.size` without building the string.
    NDigits,
    PNDigits,
    Isqrt,
    Digits,
    PDigits,
    SatAdd,
    ArrCopy,
    Even,
    PEven,
    PToI64,
    /// Int → Float
    IntToF,
    /// Float → Int: (x, loc); truncates, fails on NaN/inf/out of range.
    FToI,
    FSqrt,
    FAbs,
    /// Float#to_s, as Ruby prints it.
    FToS,
    /// (x, digits): fixed notation, `%.Nf`.
    FFmt,
    /// Concatenate all argument strings.
    StrCat,
    /// A U64 (bit pattern in an i64) in decimal.
    U64ToS,
    /// (v, base, upper, unsigned): `%x` `%X` `%o` `%b`.
    IntFmt,
    /// Float → U64: (x, loc); fails on NaN, negatives and out of range.
    FToU64,
    /// A Rune as a one-character Str (`%c`).
    RuneToS,
    /// A Str holding a copy of a [U8]'s bytes.
    StrFromBytes,
    /// (path): 0 if the file can be read, 1 if it doesn't exist, 2 for any other error.
    FileStatus,
    /// (path): the file's contents; empty on failure (check `FileStatus` first).
    FileRead,
}

/// Primitive integer operations on i64 registers. The lowering builds Go's
/// integer semantics (widths, overflow checks, shifts) out of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prim {
    And,
    Or,
    Xor,
    /// a & !b
    AndNot,
    Not,
    /// Shift counts are always in 0..64 here.
    Shl,
    ShrS,
    ShrU,
    /// Unsigned comparisons (as u64).
    ULt,
    ULe,
    UDiv,
    URem,
    /// High 64 bits of the unsigned 128-bit product.
    UMulHi,
    /// Truncate to a narrow kind's width, then sign- or zero-extend back.
    Wrap(IntKind),
    /// u64 → f64
    UToF,
}

/// A step of an assignable place.
#[derive(Clone, Debug)]
pub enum Step {
    Index(LE, Option<String>),
    Field(usize),
}

#[derive(Clone, Debug)]
pub enum LS {
    Set(V, LE),
    SetIndex { arr: V, idx: LE, val: LE, check: Option<String> },
    /// `var[i].f... = val`: a write through index and field steps.
    SetPlace { var: V, steps: Vec<Step>, val: LE },
    Push(V, LE),
    Eval(LE),
    If(LE, Vec<LS>, Vec<LS>),
    Loop(Label, Vec<LS>),
    Break(Label),
    Continue(Label),
    Return(Option<LE>),
    /// Pull the next element of a generator into `dst`, or break `label`.
    NextOrBreak { source: LE, dst: V, label: Label },
    Yield(LE),
    /// `dst = arr.pmap { worker }`.
    Pmap { dst: V, arr: LE, worker: usize },
    Puts(LE, LTy),
    Panic(String, String),
    /// Flush stdout, print the Str and a newline to stderr, exit 1.
    Die(LE),
    SortInPlace(V, LTy),
}

#[derive(Clone, Debug)]
pub struct LFunc {
    pub name: String,
    pub params: Vec<V>,
    pub vars: Vec<LVar>,
    pub ret: LTy,
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
    /// Map instantiations generated so far (see mapgen), by K/V.
    pub maps: Vec<String>,
}
