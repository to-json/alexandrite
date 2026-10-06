//! Typed tree produced by the checker: one `TFunc` per function instance
//! (monomorphized), every expression typed, every call resolved.

pub use std::collections::HashMap;
use crate::ast::IntKind;
use crate::ast::{BinOp, Overflow};
use num_bigint::BigInt;
use num_rational::BigRational;
use crate::diag::Span;
use std::rc::Rc;

/// A type: a structural tree. A struct's or enum's field list is shared
/// (`Rc`), so cloning a type is cheap however large its fields are: the
/// checker copies types into every expression, and a struct like
/// crypto/tls.Conn expands to ~11,000 nodes (port-issues #160).
#[derive(Clone, Debug)]
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
    Enum(String, Rc<Vec<(String, Vec<(String, Ty)>)>>),
    /// Any error value: one of the program's `error` types (and the
    /// builtin ones), with where it happened and any `wrap` context.
    Error,
    /// `~T` held as a value: a T or an Error.
    Result(Box<Ty>),
    /// A spawned task whose body produces this (a `~T` if it uses `~`).
    Task(Box<Ty>),
    /// A channel of T.
    Chan(Box<Ty>),
    /// `Pool[T]`: values addressed by handles (shared, like a map).
    Pool(Box<Ty>),
    /// `@T`: a handle into a pool of T (named by T's type name).
    Handle(String),
    /// `Mutex[T]`: a T only reached under its lock (shared between tasks).
    Mutex(Box<Ty>),
    /// `Atomic[T]`: shared between tasks. An Int or a Bool is a runtime
    /// cell; any other T sits behind a lock (`atomic_boxed`, R11).
    Atomic(Box<Ty>),
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
    /// `Ptr`: an opaque C pointer (pointer-sized; no deref in alexandrite).
    Ptr,
    /// A user struct (a value): its name and fields, in order.
    Struct(String, Rc<Vec<(String, Ty)>>),
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
    /// A struct or enum named where it is being defined, inside its own
    /// definition (R12): `struct Node { kids: [Node] }` is
    /// `Struct("Node", [("kids", Array(Rec("Node")))])`. Equal to the
    /// struct or enum of that name; `Ty::unrec` gives its definition.
    Rec(String),
}

thread_local! {
    /// The definitions of the struct and enum types that contain
    /// themselves (through a slice, a map, an optional, a closure...), by
    /// name, for `Ty::Rec` (R12).
    pub static RECS: std::cell::RefCell<HashMap<String, Ty>> = std::cell::RefCell::new(HashMap::new());
    /// The `Rec` names being unfolded by `with_rec` now.
    static UNFOLDING: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(vec![]) };
}

/// The key of `TProgram::iface_eqs` for `==` on Error values (`M::ErrEq`):
/// each error type's `__eq` instance in tag order (`usize::MAX` for a type
/// whose values can't be compared: never equal).
pub const ERR_EQ: &str = "#Error";

/// Record the definition of a type that refers to itself.
pub fn define_rec(name: &str, t: Ty) {
    RECS.with(|r| r.borrow_mut().insert(name.to_string(), t));
}

/// Is `name` a struct or enum that contains itself?
pub fn is_rec(name: &str) -> bool {
    RECS.with(|r| r.borrow().contains_key(name))
}

/// The definition of a named struct or enum (one that contains itself).
pub fn rec_def(name: &str) -> Option<Ty> {
    RECS.with(|r| r.borrow().get(name).cloned())
}

/// Walk into a recursive type's definition at most once per walk: `f`
/// gets the definition of `name`, unless `name` is already being walked
/// (or unknown); then the walk gets `cycle` (a coinductive answer: what
/// the rest of the walk decides).
pub fn with_rec<T>(name: &str, cycle: T, f: impl FnOnce(&Ty) -> T) -> T {
    if UNFOLDING.with(|u| u.borrow().iter().any(|x| x == name)) {
        return cycle;
    }
    let Some(def) = rec_def(name) else { return cycle };
    UNFOLDING.with(|u| u.borrow_mut().push(name.to_string()));
    let r = f(&def);
    UNFOLDING.with(|u| u.borrow_mut().pop());
    r
}

/// Does `t` hold (inline: in its fields, tuple slots, optionals) a value
/// of a type that contains itself? Such a value in an optional field may
/// be boxed (R12).
pub fn mentions_rec(t: &Ty) -> bool {
    match t {
        Ty::Rec(_) => true,
        Ty::Struct(n, _) | Ty::Enum(n, _) if is_rec(n) => true,
        Ty::Struct(_, fs) => fs.iter().any(|(_, t)| mentions_rec(t)),
        Ty::Enum(_, vs) => vs.iter().any(|(_, fs)| fs.iter().any(|(_, t)| mentions_rec(t))),
        Ty::Tuple(ts) => ts.iter().any(mentions_rec),
        Ty::Opt(t) | Ty::Fixed(t, _) | Ty::Result(t) => mentions_rec(t),
        _ => false,
    }
}

impl PartialEq for Ty {
    fn eq(&self, o: &Ty) -> bool {
        use Ty::*;
        match (self, o) {
            (Rec(a), Rec(b)) => a == b,
            (Rec(a), Struct(b, _) | Enum(b, _)) | (Struct(b, _) | Enum(b, _), Rec(a)) => a == b,
            (Struct(a, x), Struct(b, y)) => a == b && x == y,
            (Enum(a, x), Enum(b, y)) => a == b && x == y,
            (Int, Int) | (Float, Float) | (Bool, Bool) | (Str, Str) | (Unit, Unit) | (Error, Error) | (Ptr, Ptr) | (Range, Range) | (Never, Never) => true,
            (IntK(a), IntK(b)) => a == b,
            (Array(a), Array(b)) | (Result(a), Result(b)) | (Task(a), Task(b)) | (Chan(a), Chan(b)) | (Pool(a), Pool(b)) | (Mutex(a), Mutex(b)) | (Atomic(a), Atomic(b)) | (Opt(a), Opt(b)) | (Gen(a), Gen(b)) | (Yielder(a), Yielder(b)) => a == b,
            (Fixed(a, n), Fixed(b, m)) => n == m && a == b,
            (Handle(a), Handle(b)) | (Iface(a), Iface(b)) => a == b,
            (Fn(a, r), Fn(b, s)) => a == b && r == s,
            (Map(a, x), Map(b, y)) => a == b && x == y,
            (Tuple(a), Tuple(b)) => a == b,
            (Seq(a, l), Seq(b, m)) => l == m && a == b,
            (Var(a), Var(b)) => a == b,
            _ => false,
        }
    }
}
impl Eq for Ty {}

impl std::hash::Hash for Ty {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        use Ty::*;
        match self {
            // Equal to each other by name.
            Struct(n, _) | Enum(n, _) | Rec(n) => {
                0u8.hash(h);
                n.hash(h);
            }
            t => {
                std::mem::discriminant(t).hash(h);
                match t {
                    IntK(k) => k.hash(h),
                    Array(a) | Result(a) | Task(a) | Chan(a) | Pool(a) | Mutex(a) | Atomic(a) | Opt(a) | Gen(a) | Yielder(a) => a.hash(h),
                    Fixed(a, n) => {
                        a.hash(h);
                        n.hash(h);
                    }
                    Handle(a) | Iface(a) => a.hash(h),
                    Fn(a, r) => {
                        a.hash(h);
                        r.hash(h);
                    }
                    Map(a, b) => {
                        a.hash(h);
                        b.hash(h);
                    }
                    Tuple(a) => a.hash(h),
                    Seq(a, l) => {
                        a.hash(h);
                        l.hash(h);
                    }
                    Var(v) => v.hash(h),
                    _ => {}
                }
            }
        }
    }
}

impl Ty {
    /// A `Rec` as the struct or enum it names; anything else as it is.
    pub fn unrec(&self) -> Ty {
        match self {
            Ty::Rec(n) => rec_def(n).unwrap_or_else(|| self.clone()),
            t => t.clone(),
        }
    }
    /// The value type of an `Atomic[self]` that isn't a runtime cell (not
    /// an Int or a Bool): it is kept behind a lock, laid out like a Mutex.
    pub fn atomic_boxed(&self) -> bool {
        !matches!(self, Ty::Int | Ty::Bool)
    }
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
            Ty::Ptr => "Ptr".into(),
            Ty::Struct(n, _) | Ty::Enum(n, _) | Ty::Iface(n) | Ty::Rec(n) => n.clone(),
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
            Ty::Pool(t) => format!("Pool[{}]", t.show()),
            Ty::Handle(n) => format!("@{n}"),
            Ty::Mutex(t) => format!("Mutex[{}]", t.show()),
            Ty::Atomic(t) => format!("Atomic[{}]", t.show()),
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
            Ty::Result(t) | Ty::Task(t) | Ty::Chan(t) | Ty::Pool(t) | Ty::Mutex(t) | Ty::Atomic(t) => t.has_var(),
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
            Ty::Struct(n, _) | Ty::Enum(n, _) | Ty::Rec(n) => Some(n.split('[').next().unwrap_or(n)),
            _ => None,
        }
    }
    /// An enum's flattened slots: variant `k`'s first field is at slot
    /// `enum_slot(k)` of the value (slot 0 is the tag).
    pub fn enum_slot(&self, k: usize) -> usize {
        if let Ty::Rec(_) = self {
            return self.unrec().enum_slot(k);
        }
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
            Ty::Rec(n) => with_rec(n, false, Ty::is_value_array),
            _ => false,
        }
    }
    pub fn field(&self, name: &str) -> Option<(usize, Ty)> {
        match self {
            Ty::Rec(_) => self.unrec().field(name),
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
    /// A Ruby string convenience (see strgen.rs; the code is its StrFn).
    StrHelper(u8),
    /// `s.byteindex(sub, from)`: Ruby's String#byteindex (Rt::StrIndex).
    ByteIndex,
    // stages
    Select,
    Reject,
    Map,
    FlatMap,
    TakeWhile,
    Drop,
    /// `step(n)`: every n-th element, from the first (Ruby's Range#step).
    StepBy,
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
    /// `u.__mulhi(v)` (U64): the high word of the 128-bit product (math/bits, math/big).
    UMulHi,
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
    /// A libm function (`x.__sin`; the receiver and args are Floats).
    Math(crate::lir::MathFn),
    /// Float -> U64 bit pattern (`x.__bits`).
    FloatBits,
    /// U64 -> Float (`u.__from_bits`).
    FloatFromBits,
    /// `x.__ffmt_f(n)` / `x.__ffmt_e(n)`: `%.nf` / `%.ne` of a Float (the fmt engine).
    FloatFmtF,
    FloatFmtE,
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
    /// Array constant k, read in place (`TProgram::globals`). Only ever
    /// indexed down to elements without storage, so it is never changed.
    Global(usize),
    /// Set global k (a package-level `Atomic` / `Mutex`, R11) to args[0],
    /// once, at program start.
    SetGlobal(usize),
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
    /// Does an interface value hold implementor `k`? / its value as that type
    /// (`case shape { Circle(c) => }`, Go's type switch; D71).
    IfaceIs(usize),
    IfaceAs(usize),
    /// Results: `ok` (T?), `err` (Error?), `ok?`, `unwrap`, `unwrap_or(d)`,
    /// `rescue { |e| v }`.
    ResOk,
    ResErr,
    ResIsOk,
    ResUnwrap,
    ResUnwrapOr,
    ResRescue,
    /// Pools: `Pool[T].new`, `add(v)` → @T, `p[h]`, `p[h] = v`, `remove(h)` → T?, `size`.
    PoolNew,
    PoolAdd,
    PoolGet,
    /// `p.get(h)`: the value, or none for a removed (or foreign, out of range) handle.
    PoolLookup,
    PoolSet,
    PoolRemove,
    PoolSize,
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
    /// `Mutex.new(v)`; `m.lock { |v| ... }` (the block runs holding the
    /// lock; `v` is the value, changed in place; the result is copied out).
    MutexNew,
    Lock,
    /// `m.poisoned?`: a task panicked holding the lock. `m.clear_poison!`.
    MutexPoisoned,
    MutexClearPoison,
    /// `(a, b)`: a tuple of the args.
    TupleNew,
    /// `xs.join(sep)` (args[0] is sep).
    Join,
    /// `fmt.print*`: write a Str (args[0]) to stdout, no newline added.
    PrintStr,
    /// `Atomic.new(v)`; `load`, `store(v)`, `add(n)` (the new value),
    /// `swap(v)` (the old one), `compare_and_swap(old, new)` (Bool).
    AtomicNew,
    AtomicLoad,
    AtomicStore,
    AtomicAdd,
    AtomicSwap,
    AtomicCas,
    /// A lambda literal (the block); args are its captured locals.
    Lambda,
    /// Call a function value: recv is the function, args its arguments.
    FnCall,
    /// Wrap a concrete value as implementor `k` of its interface type.
    ToIface(usize),
    /// Call method `k` (declaration order) of an interface value.
    IfaceCall(usize),
    /// `v.as(T)`: the interface value as implementor `k` (a `T?`; Go's
    /// `t, ok := v.(T)`).
    IfaceOpt(usize),
    /// `a == b` on interface values (recv a, args [b]): the same
    /// implementor, and `__eq` of the two values (`TProgram::iface_eqs`).
    IfaceEq,
    /// `a == b` on Error values (recv a, args [b]): the same error type,
    /// and its `__eq` of the two values (`TProgram::iface_eqs[ERR_EQ]`).
    ErrEq,
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
    /// `Time.now_ns`: a monotonic clock in nanoseconds.
    NowNs,
    /// `Ptr.null`.
    PtrNull,
    /// `Str.from_cstr(p)`: a copy of the NUL-terminated string at `p`.
    StrFromCstr,
    /// `Str.from_ptr(p, n)`: a copy of the `n` bytes at `p`.
    StrFromPtr,
    /// `C.errno`: errno as saved right after the last `extern def` call on this thread.
    CErrno,
    /// `C.strerror(n)`.
    CStrerror,
    /// `Test.begin_capture` / `Test.end_capture`: capture what `puts` prints.
    CapBegin,
    CapEnd,
    /// `Test.exit(code)`.
    Exit,
    EnumNew,
    Loop,
}

impl M {
    pub fn is_stage(self) -> bool {
        use M::*;
        matches!(self, Select | Reject | Map | FlatMap | TakeWhile | Drop | Take | StepBy | EachWithIndex | Lazy | EachIndex | EachCons | Chars | Bytes | Runes)
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
    /// A Str literal of arbitrary bytes (an `#[embed]`ded file that isn't UTF-8).
    Bytes(Vec<u8>),
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
    /// Panic with this Str message (`assert`, `assert_eq`).
    Panic(Box<TExpr>),
    Array(Vec<TExpr>),
    /// `place = v`, or `place op= v`: a local, then index and field steps.
    PlaceAssign(LocalId, Vec<TStep>, Option<BinOp>, Box<TExpr>),
    /// `place.m!(args)`: the call (whose receiver argument is `Local(view)`)
    /// runs with `view` a one-element slice that *is* the place (a local,
    /// then index and field steps): writes through it land in the place.
    /// See docs/notes/bang-calls.md.
    Bang(LocalId, Vec<TStep>, LocalId, Box<TExpr>),
    /// `format("...", args)`: pieces checked against the arguments.
    Format(Vec<FmtPiece>, Vec<TExpr>),
    /// Statements whose value is the last one's (a `case` arm, a desugaring).
    Seq(Vec<TStmt>),
    /// The absent value of a T?.
    None,
    /// The all-zero value of the type (an interface value: its first
    /// implementor's zero).
    Zero,
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
    /// `%q`: argument k (a Str) double-quoted, Go-escaped
    Quote(usize),
    /// `%e` `%E`: argument k (a number) in exponent form, N decimals
    Exp(usize, u32, bool),
    /// A piece with flags and a width: `%-8s`, `%05d`, `%+d`, `% d`.
    Padded { inner: Box<FmtPiece>, width: u32, left: bool, zero: bool, plus: bool, space: bool },
    /// An integer's precision, `%.3d`: at least n digits (zero-padded after the sign); `%.0d` of 0 is empty.
    Digits { inner: Box<FmtPiece>, n: u32 },
}
impl FmtPiece {
    /// The argument this piece shows.
    pub fn arg(&self) -> Option<usize> {
        match self {
            FmtPiece::Lit(_) => None,
            FmtPiece::Int(k) | FmtPiece::Str(k) | FmtPiece::Fixed(k, _) | FmtPiece::Base(k, ..) | FmtPiece::Char(k) | FmtPiece::Quote(k) | FmtPiece::Exp(k, ..) => Some(*k),
            FmtPiece::Padded { inner, .. } | FmtPiece::Digits { inner, .. } => inner.arg(),
        }
    }
    /// Whether it shows its argument as a Float.
    pub fn wants_float(&self) -> bool {
        match self {
            FmtPiece::Fixed(..) | FmtPiece::Exp(..) => true,
            FmtPiece::Padded { inner, .. } | FmtPiece::Digits { inner, .. } => inner.wants_float(),
            _ => false,
        }
    }
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
    /// The AST block's id (0 for blocks the checker makes): a lambda's
    /// identity, which a span isn't (derived code shares one span).
    pub id: u32,
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
    /// An `extern def`: the C symbol it calls (the function has no body).
    pub ffi: Option<String>,
    pub is_main: bool,
    /// Lambda literals in this function: (block span start, fn type, captured locals).
    pub lambdas: Vec<(u64, Ty, Vec<LocalId>)>,
    /// Per lambda (by block span start): its parameters and the range of
    /// locals its body declares (parameters included).
    pub lambda_info: Vec<(u64, Vec<LocalId>, (usize, usize))>,
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
    /// Array constants read in place, by `M::Global` index: each one's
    /// value, a literal. Built before the program's first statement.
    pub globals: Vec<TExpr>,
    /// The globals that are package-level values (R11), set by
    /// `M::SetGlobal` at the start of main rather than from a literal.
    pub vars: Vec<usize>,
    /// For each interface whose values are compared with `==`: the
    /// `__eq` instance of each implementor, in tag order.
    pub iface_eqs: HashMap<String, Vec<FuncId>>,
}
