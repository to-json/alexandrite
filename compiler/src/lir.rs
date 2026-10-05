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
    /// A handle to a spawned task that produces a value of this type
    /// (pointer-sized; copies refer to the same task).
    Task(Box<LTy>),
    /// A handle to a channel carrying values of this type (pointer-sized;
    /// copies refer to the same channel; safe to use from any task).
    Chan(Box<LTy>),
    /// A lock (pointer-sized; copies refer to the same lock; safe to use
    /// from any task). Taking a held lock blocks the task, not the thread.
    Lock,
    /// A shared I64 cell read and written atomically (pointer-sized; copies
    /// refer to the same cell; sequentially consistent).
    Atomic,
    /// A memory region (pointer-sized handle). Every allocation the runtime
    /// makes (arrays, strings, map storage, growth) goes into the thread's
    /// *current* region. See `LS::RegionEnter` and friends.
    Region,
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
    /// A Str literal of arbitrary bytes (not UTF-8: `#[embed]`).
    SB(Vec<u8>),
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
    /// Call the C function `LProgram::externs[i]` (an `extern def`). Flushes
    /// stdout first (so output interleaves in program order) and saves errno
    /// right after the call (read back by `Rt::Errno`).
    Ffi(usize, Vec<LE>),
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
    /// A new channel of `LTy` values with buffer capacity `cap` (an I64 >= 0).
    /// Capacity 0 is unbuffered: a send completes only when a receiver takes
    /// the value (Go's rendezvous).
    ChanNew(LTy, Box<LE>),
    /// The number of values buffered in a channel right now (I64).
    ChanLen(Box<LE>),
    /// A new, unheld lock.
    LockNew,
    /// A task handle that names no task (an LTy::Task's zero value; never waited on).
    NullTask(LTy),
    /// A new atomic cell holding this I64.
    AtomicNew(Box<LE>),
    /// The cell's value (I64). Lowering binds it right away: reads and
    /// writes of atomics keep their order.
    AtomicLoad(Box<LE>),
    /// Atomically: `Add` adds and gives the new value; `Swap` stores and
    /// gives the old one (I64). Bound right away, as `AtomicLoad` is.
    AtomicRmw(AtomicOp, Box<LE>, Box<LE>),
    /// If the cell holds `old`, store `new`; whether it did (Bool).
    AtomicCas(Box<LE>, Box<LE>, Box<LE>),
    /// This thread's program region (never freed; what everything used before regions).
    RegionProgram,
    /// The region holding the storage of this value (an `Arr` or a `Str`:
    /// the region whose chunk or large block contains its data pointer,
    /// interior pointers included). For anything else (an empty array, a
    /// string literal, memory from another thread) this thread's program
    /// region. Used to store values into a caller's container: they go
    /// where the container's storage lives.
    RegionOf(Box<LE>),
    /// A new *child* region of `parent` (not made current): freed together
    /// with its parent (when the parent is exited or freed), or earlier by
    /// `LS::RegionFree`. Used for a container that owns its contents (R3).
    RegionNew(Box<LE>),
    /// Bytes allocated in a region so far (chunks in use + large blocks),
    /// I64. Used to decide when a container's region is worth compacting.
    RegionBytes(Box<LE>),
    /// Global `k`'s value (`LProgram::globals`), shared by every task.
    Global(usize),
}

/// The C-side type of an `extern def` parameter or result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiTy {
    /// An integer of this kind (`I64` = `int64_t`; narrower kinds are the exact C types).
    Int(IntKind),
    F64,
    Bool,
    /// Parameter only: a NUL-terminated copy (`const char *`), valid for the call.
    Str,
    /// Parameter only: a `[Byte]` as a pointer to its first element.
    Bytes,
    /// An opaque pointer (`void *`); I64 in registers.
    Ptr,
    Unit,
}

impl FfiTy {
    /// The type of the value in a LIR register.
    pub fn lty(self) -> LTy {
        match self {
            FfiTy::Int(IntKind::I64) | FfiTy::Ptr => LTy::I64,
            FfiTy::Int(k) => LTy::IntK(k),
            FfiTy::F64 => LTy::F64,
            FfiTy::Bool => LTy::Bool,
            FfiTy::Str => LTy::Str,
            FfiTy::Bytes => LTy::Arr(Box::new(LTy::IntK(IntKind::U8))),
            FfiTy::Unit => LTy::Unit,
        }
    }
}

/// A C function an `extern def` calls.
#[derive(Clone, Debug, PartialEq)]
pub struct FfiSig {
    /// The link name (without the platform's leading underscore).
    pub sym: String,
    pub params: Vec<FfiTy>,
    pub ret: FfiTy,
}

/// A case of `LS::Select`.
#[derive(Clone, Debug)]
pub enum SelCase {
    /// Send `val` on `ch` (a closed channel panics, as `ChanSend` does).
    Send { ch: LE, val: LE },
    /// Receive from `ch` into `val`; `ok` is false (and `val` untouched)
    /// if the channel is closed and drained.
    Recv { ch: LE, ok: V, val: V },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rt {
    IntToS,
    PIntToS,
    StrRev,
    StrDelete,
    StrSplit,
    StrToI,
    /// (s, sub, from): the first byte offset >= from where sub occurs, or -1.
    StrIndex,
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
    /// Str#to_i in promote mode: any number of digits.
    PFromStr,
    /// Int → Float
    IntToF,
    /// Float → Int: (x, loc); truncates, fails on NaN/inf/out of range.
    FToI,
    FSqrt,
    FAbs,
    /// A libm function on Floats (see `MathFn`).
    Math(MathFn),
    /// Float -> U64: the IEEE bit pattern.
    FBits,
    /// U64 -> Float: reinterpret the bit pattern.
    FFromBits,
    /// Float#to_s, as Ruby prints it.
    FToS,
    /// (x, digits): fixed notation, `%.Nf`.
    FFmt,
    /// Concatenate all argument strings.
    StrCat,
    /// (arr: [Str], sep): the elements with `sep` between them.
    StrJoin,
    /// (s, width, flags): pad to `width` runes; flags 1 = on the right
    /// (`%-5s`), 2 = with zeros after any sign (`%05d`).
    StrPad,
    /// (s): Go's `%q` / strconv.Quote: double-quoted, escaped.
    StrQuote,
    /// (x, digits, upper): `%e` / `%E`, as Go prints it (`1.234560e+03`, `+Inf`).
    FFmtE,
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
    /// (): monotonic clock, nanoseconds (I64).
    NowNs,
    /// (): the current region (Region).
    RegionCur,
    /// (): a mark in the current region (Region): the frame of a light call.
    RegionMark,
    /// (): the second half of a mark: the current region's newest large block.
    RegionMarkLarges,
    /// (mark, larges): free what the current region got since the mark (Unit).
    RegionReset,
    /// (): errno as saved right after the last `Ffi` call on this thread (I64).
    Errno,
    /// (n): `strerror(n)` copied into a Str.
    Strerror,
    /// (p): a Str copy of the NUL-terminated string at pointer `p`.
    StrFromCstr,
    /// (p, n): a Str copy of the `n` bytes at pointer `p`.
    StrFromPtr,
    /// (): start capturing everything `puts` prints (I64, always 0). Process-wide.
    CapBegin,
    /// (): stop capturing; the Str of what was printed since `CapBegin`.
    CapEnd,
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
    /// Whether the signed product a * b overflows (Bool).
    MulOvf,
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
    /// `dst` = the worker applied to each element. With `err`, the worker
    /// returns a Result: `dst` gets the ok values and `err` the Result of the
    /// lowest failing index (its field 0 true when nothing failed).
    Pmap { dst: V, arr: LE, worker: usize, err: Option<V> },
    Puts(LE, LTy),
    /// Write a Str to stdout as it is (no newline added).
    Print(LE),
    Panic(String, String),
    /// Panic with a computed message (a Str, without the `alexandrite: `
    /// prefix, which the runtime adds). Task-aware like `Panic`.
    PanicStr(LE),
    /// Flush stdout and exit the process with this status (an I64).
    Exit(LE),
    /// `saved` := the current region; `region` := a fresh empty region,
    /// which becomes current. Should be cheap (reuse freed chunks).
    RegionEnter { region: V, saved: V },
    /// Free everything allocated in `region` (all of it at once), then make
    /// `saved` current again. Nothing allocated in `region` is used after.
    RegionExit { region: V, saved: V },
    /// `saved` := the current region; `region` (any live region handle)
    /// becomes current: allocations go there until `RegionRestore`.
    RegionUse { region: LE, saved: V },
    /// Make `saved` (from a `RegionUse`) current again.
    RegionRestore(V),
    /// Free a child region (from `RegionNew`) now, everything in it at once,
    /// and detach it from its parent. Nothing in it is used afterwards; it
    /// is not current when freed.
    RegionFree(LE),
    /// Start a task running `workers[worker]` on `env` (a value of the
    /// worker's `input` type, copied); `dst` (an `LTy::Task`) gets its handle.
    /// A panic inside the task ends only that task (see `Wait`); a panic on
    /// the main thread still aborts the process.
    Spawn { dst: V, worker: usize, env: LE },
    /// Block until `task` finishes. If it returned, `ok` = true and `val` =
    /// its result; if it panicked, `ok` = false and `msg` = the panic message
    /// (`val` untouched). Waiting again gives the same answer.
    Wait { task: LE, ok: V, val: V, msg: V },
    /// Send `val` on `ch`: blocks while the buffer is full (unbuffered: until
    /// a receiver takes it). Sending on a closed channel panics at `loc`.
    ChanSend { ch: LE, val: LE, loc: String },
    /// Receive from `ch`: blocks until a value arrives (`ok` = true, `val` =
    /// it) or the channel is closed and drained (`ok` = false, `val` untouched).
    ChanRecv { ch: LE, ok: V, val: V },
    /// Close `ch`; receivers drain what's buffered, then see `ok` = false.
    /// Closing twice panics at `loc`.
    ChanClose { ch: LE, loc: String },
    /// Take the lock, blocking (the task) while another holder has it.
    Lock(LE),
    /// Release the lock (held by this task) and wake a waiter.
    Unlock(LE),
    /// Store into an atomic cell.
    AtomicStore(LE, LE),
    /// Run one ready case, chosen fairly (random among the ready ones);
    /// `dst` = its index. If none is ready: with `default`, run none and set
    /// `dst` = `cases.len()`; otherwise block until one is. The case
    /// expressions are evaluated once, before waiting, in order.
    Select { cases: Vec<SelCase>, default: bool, dst: V },
    /// Flush stdout, print the Str and a newline to stderr, exit 1.
    Die(LE),
    SortInPlace(V, LTy),
    /// Set global `k`. Only at the start of main, before any other code
    /// (or task) runs: globals are read-only after that.
    SetGlobal(usize, LE),
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
    /// The C functions of `extern def`s (what `LE::Ffi` indexes).
    pub externs: Vec<FfiSig>,
    /// Program-wide variables (array constants), by `LE::Global` index.
    pub globals: Vec<LTy>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtomicOp {
    Add,
    Swap,
}

// ---- math (std/math): C library functions on Floats ----

macro_rules! math_fns {
    ($($v:ident $name:literal $arity:literal $c:literal;)*) => {
        /// A libm function on F64 arguments, F64 result. The alexandrite side
        /// reaches it as a `__name` method on Float (see `check.rs`); the
        /// `math` package wraps those.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[repr(u8)]
        pub enum MathFn { $($v),* }
        impl MathFn {
            pub const ALL: &'static [MathFn] = &[$(MathFn::$v),*];
            /// The method name after the `__` prefix.
            pub fn name(self) -> &'static str { match self { $(MathFn::$v => $name),* } }
            /// Number of Float arguments (receiver included).
            pub fn arity(self) -> usize { match self { $(MathFn::$v => $arity),* } }
            /// The C library function.
            pub fn c_name(self) -> &'static str { match self { $(MathFn::$v => $c),* } }
            pub fn by_name(n: &str) -> Option<MathFn> { match n { $($name => Some(MathFn::$v),)* _ => None } }
            pub fn from_id(i: u8) -> Option<MathFn> { Self::ALL.get(i as usize).copied() }
        }
    };
}

math_fns! {
    Sin "sin" 1 "sin"; Cos "cos" 1 "cos"; Tan "tan" 1 "tan";
    Asin "asin" 1 "asin"; Acos "acos" 1 "acos"; Atan "atan" 1 "atan"; Atan2 "atan2" 2 "atan2";
    Sinh "sinh" 1 "sinh"; Cosh "cosh" 1 "cosh"; Tanh "tanh" 1 "tanh";
    Asinh "asinh" 1 "asinh"; Acosh "acosh" 1 "acosh"; Atanh "atanh" 1 "atanh";
    Exp "exp" 1 "exp"; Exp2 "exp2" 1 "exp2"; Expm1 "expm1" 1 "expm1";
    Log "log" 1 "log"; Log2 "log2" 1 "log2"; Log10 "log10" 1 "log10"; Log1p "log1p" 1 "log1p";
    Pow "pow" 2 "pow"; Cbrt "cbrt" 1 "cbrt"; Hypot "hypot" 2 "hypot";
    Floor "floor" 1 "floor"; Ceil "ceil" 1 "ceil"; Trunc "trunc" 1 "trunc";
    Round "round" 1 "round"; RoundEven "round_even" 1 "rint";
    Fmod "fmod" 2 "fmod"; Remainder "remainder" 2 "remainder"; Fma "fma" 3 "fma";
    Nextafter "nextafter" 2 "nextafter"; Copysign "copysign" 2 "copysign";
    Erf "erf" 1 "erf"; Erfc "erfc" 1 "erfc"; Gamma "gamma" 1 "tgamma"; Lgamma "lgamma" 1 "lgamma";
}
