//! Syntax tree. Every node that can be typed carries a unique `id`.

use crate::diag::Span;

pub type NodeId = u32;

/// Go's integer types. `Int` is I64; Byte = U8, Rune = I32.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntKind {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
}

impl IntKind {
    pub fn bits(self) -> u32 {
        match self {
            IntKind::I8 | IntKind::U8 => 8,
            IntKind::I16 | IntKind::U16 => 16,
            IntKind::I32 | IntKind::U32 => 32,
            IntKind::I64 | IntKind::U64 => 64,
        }
    }
    pub fn signed(self) -> bool {
        matches!(self, IntKind::I8 | IntKind::I16 | IntKind::I32 | IntKind::I64)
    }
    pub fn min(self) -> i128 {
        if self.signed() { -(1i128 << (self.bits() - 1)) } else { 0 }
    }
    pub fn max(self) -> i128 {
        if self.signed() { (1i128 << (self.bits() - 1)) - 1 } else { (1i128 << self.bits()) - 1 }
    }
    pub fn name(self) -> &'static str {
        match self {
            IntKind::I8 => "I8",
            IntKind::I16 => "I16",
            IntKind::I32 => "I32",
            IntKind::I64 => "Int",
            IntKind::U8 => "U8",
            IntKind::U16 => "U16",
            IntKind::U32 => "U32",
            IntKind::U64 => "U64",
        }
    }
    pub fn from_name(n: &str) -> Option<IntKind> {
        Some(match n {
            "I8" => IntKind::I8,
            "I16" => IntKind::I16,
            "I32" | "Rune" => IntKind::I32,
            "I64" | "Int" => IntKind::I64,
            "U8" | "Byte" => IntKind::U8,
            "U16" => IntKind::U16,
            "U32" => IntKind::U32,
            "U64" => IntKind::U64,
            _ => return None,
        })
    }
    /// Suffix of the conversion methods: `to_u8`, `as_i32`.
    pub fn method(self) -> &'static str {
        match self {
            IntKind::I8 => "i8",
            IntKind::I16 => "i16",
            IntKind::I32 => "i32",
            IntKind::I64 => "i64",
            IntKind::U8 => "u8",
            IntKind::U16 => "u16",
            IntKind::U32 => "u32",
            IntKind::U64 => "u64",
        }
    }
}

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
    pub enums: Vec<EnumDef>,
    pub ifaces: Vec<IfaceDef>,
    /// `NAME = expr` at the top level: compile-time constants (Go's exact rules).
    pub consts: Vec<ConstDef>,
    /// Top-level statements, run in order (the implicit `main`).
    pub main: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub struct Def {
    pub name: String,
    pub span: Span,
    /// `def f[T, U: Shape]`: explicit type parameters (bounds checked per instance).
    pub tparams: Vec<TParam>,
    pub name_span: Span,
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    pub fallible: bool,
    /// A fallible def's error set: None = inferred (`~T`), names otherwise
    /// (`~T<ParseError | IoError>`; `Error` = open).
    pub errs: Option<Vec<String>>,
    pub pure: bool,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub struct ConstDef {
    pub name: String,
    pub span: Span,
    pub ty: Option<TypeExpr>,
    pub value: Expr,
}

/// `struct Name { field: Type, ... }`: a value type.
#[derive(Debug, Clone)]
pub struct StructDef {
    pub name: String,
    pub span: Span,
    pub tparams: Vec<TParam>,
    pub fields: Vec<(String, TypeExpr, Span)>,
}

/// A type parameter: `T`, `T: Shape` (an interface), `T: like Int`.
#[derive(Debug, Clone)]
pub struct TParam {
    pub name: String,
    pub bound: Option<Bound>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Bound {
    /// Implements this interface.
    Iface(String),
    /// `like Int`: any integer type; `like Float`: Float.
    Like(String),
}

/// `enum Name { Variant(field: T, ...), Bare, ... }`: a sum type (a value).
#[derive(Debug, Clone)]
pub struct EnumDef {
    pub name: String,
    pub span: Span,
    /// Declared with `error`: its values are errors (they convert to `Error`).
    pub error: bool,
    pub tparams: Vec<TParam>,
    pub variants: Vec<(String, Vec<(String, TypeExpr, Span)>, Span)>,
}

/// `interface Name { def m(x: T) -> R; def d -> R { default } }`: satisfied
/// structurally. Required methods have no body; defaults are defs named
/// `Name.m` whose `self` is generic (the concrete type).
#[derive(Debug, Clone)]
pub struct IfaceDef {
    pub name: String,
    pub span: Span,
    /// Every method's signature (defaults too): name, params (after self), ret, has a default.
    pub methods: Vec<(String, Vec<Param>, Option<TypeExpr>, bool, Span)>,
}

/// Methods are defs named `Type.name` whose first parameter is `self`.
/// In a `!` method `self` is a one-element slice holding the receiver, so
/// writes reach the caller (Go's pointer receiver).
pub fn method_name(owner: &str, m: &str) -> String {
    format!("{owner}.{m}")
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
    /// `T?`
    Opt(Box<TypeExpr>, Span),
    /// `[T; N]`: a fixed-size array (a value). N is a literal or a constant.
    Fixed(Box<TypeExpr>, Box<Expr>, Span),
    /// `Name[T, ...]`: a generic type applied (`Map[Str, Int]`).
    App(String, Vec<TypeExpr>, Span),
    /// `~T`, `~T<E | F>`: a fallible T (a Result value when held).
    Result(Box<TypeExpr>, Option<Vec<String>>, Span),
    /// `(A, B) -> R`: a function value (a lambda).
    Fn(Vec<TypeExpr>, Box<TypeExpr>, Span),
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
    /// `x: T = e`
    Decl(String, Span, TypeExpr, Expr),
    While(Expr, Vec<Stmt>),
    If(Expr, Vec<Stmt>, Vec<Stmt>),
    Next,
    Break(Option<Expr>),
    Return(Option<Expr>),
    /// `fail e`: return error `e` from a fallible def.
    Fail(Expr),
    /// `defer e`: run when the enclosing block exits.
    Defer(Expr),
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
    BitAnd,
    BitOr,
    BitXor,
    /// `&^` (Go's and-not)
    AndNot,
    Shl,
    Shr,
    /// `+%` `-%` `*%`: wrapping arithmetic
    AddW,
    SubW,
    MulW,
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
            BinOp::BitAnd => "&",
            BinOp::BitOr => "|",
            BinOp::BitXor => "^",
            BinOp::AndNot => "&^",
            BinOp::Shl => "<<",
            BinOp::Shr => ">>",
            BinOp::AddW => "+%",
            BinOp::SubW => "-%",
            BinOp::MulW => "*%",
        }
    }
    /// Integer operations that never fail.
    pub fn is_bitwise(self) -> bool {
        matches!(self, BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::AndNot | BinOp::Shl | BinOp::Shr | BinOp::AddW | BinOp::SubW | BinOp::MulW)
    }
    pub fn is_arith(self) -> bool {
        matches!(self, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem | BinOp::Pow)
    }
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    Int(i64),
    /// An integer literal outside i64 (only meaningful as a constant).
    BigInt(String),
    /// The value, and the literal's text (constants fold exactly).
    Float(f64, String),
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
    /// `^x`: bitwise complement (Go)
    BitNot(Box<Expr>),
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
    /// `"a #{x} b"`
    Interp(Vec<InterpPart>),
    /// `case subject { pat => body ... }`, or `case { cond => body ... }`
    Case(Option<Box<Expr>>, Vec<CaseArm>),
    /// `if` used as a value: `x = if c { a } else { b }`
    If(Box<Expr>, Vec<Stmt>, Vec<Stmt>),
    /// `none`
    None,
    /// `[v; n]`: n copies of v (a fixed-size array)
    ArrayRepeat(Box<Expr>, Box<Expr>),
    /// `->(x: T) -> R { body }`: a lambda (escapes; captures copies).
    Lambda(Vec<Param>, Option<TypeExpr>, Box<Block>),
    /// `Stack[Int]` before `.new`: a generic type with explicit arguments.
    TypeApp(String, Vec<TypeExpr>),
    /// `{k => v, ...}` / `{name: v}` (a Str key) / `{}`
    MapLit(Vec<(Expr, Expr)>),
    /// The index of a reslice: `a[lo...hi]`, `a[lo..]`, `a[...hi]`
    SliceRange(Option<Box<Expr>>, Option<Box<Expr>>, bool),
    /// `x?.m(...)`: the inner Call's receiver is the optional value.
    OptCall(Box<Expr>),
}

#[derive(Debug, Clone)]
pub enum InterpPart {
    Lit(String),
    Expr(Expr),
}

#[derive(Debug, Clone)]
pub struct CaseArm {
    /// Alternatives (`1 | 2`); empty means `_`.
    pub pats: Vec<Pat>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Pat {
    /// A value compared with `==` (literal, constant), or with a guard-less
    /// `case { cond => }` the condition itself.
    Value(Expr),
    /// `lo..hi` / `lo...hi`
    Range(Expr, Expr, bool),
    /// `Circle(r, _)` / `Empty`: an enum variant, binding its fields. Without
    /// parens it may also be a constant (the checker decides).
    Variant(String, Option<Vec<(String, Span)>>, Span),
}
