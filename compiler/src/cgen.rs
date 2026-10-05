//! LIR → C (the primary backend).

use crate::lir::*;
use std::collections::HashSet;
use std::fmt::Write;

/// The C name of a type. Names spell the layout (`Tup2_I64_Str`); a long
/// one (a struct with dozens of fields, nested) becomes `Ty_<hash>`, so a
/// big program's C doesn't grow lines of a megabyte (crypto/tls's tests
/// made 200 MB of C that clang took over half an hour on).
pub fn ty_name(t: &LTy) -> String {
    let n = ty_name_full(t);
    if n.len() <= 96 {
        return n;
    }
    // FNV-1a, 64 bits: stable across runs and backends.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in n.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("Ty_{h:016x}")
}

fn ty_name_full(t: &LTy) -> String {
    match t {
        LTy::Region => "Region".into(),
        LTy::Task(t) => format!("Task_{}", ty_name(t)),
        LTy::Chan(t) => format!("Chan_{}", ty_name(t)),
        LTy::Lock => "Lock".into(),
        LTy::Atomic => "Atomic".into(),
        LTy::I64 => "I64".into(),
        LTy::IntK(k) => k.name().into(),
        LTy::F64 => "F64".into(),
        LTy::PInt => "PInt".into(),
        LTy::Bool => "Bool".into(),
        LTy::Str => "Str".into(),
        LTy::Unit => "Unit".into(),
        LTy::Arr(t) => format!("Arr_{}", ty_name(t)),
        // The arity keeps nested tuples apart: Tup(Tup(A), B) and
        // Tup(Tup(A, B)) would both be Tup_Tup_A_B.
        LTy::Tup(ts) => format!("Tup{}_{}", ts.len(), ts.iter().map(ty_name).collect::<Vec<_>>().join("_")),
        LTy::Range => "Range".into(),
        LTy::Gen(t) => format!("Gen_{}", ty_name(t)),
    }
}

/// Is this value all-zero bytes in C, or equivalent to them (so a
/// designated initialiser may leave it out)? An empty string is: a zeroed
/// AlxStr (null pointer, length 0) already stands for "" in every
/// zero-initialised local.
fn c_zero(e: &LE) -> bool {
    match e {
        LE::I(0) | LE::B(false) | LE::Unit => true,
        LE::S(s) => s.is_empty(),
        LE::F(f) => *f == 0.0 && f.is_sign_positive(),
        LE::ArrWithCap(_, n) => matches!(**n, LE::I(0)),
        LE::Tup(_, vs) => vs.iter().all(c_zero),
        _ => false,
    }
}

pub fn cty(t: &LTy) -> String {
    match t {
        LTy::Region => "AlxRegion *".into(),
        LTy::Task(_) => "AlxTask *".into(),
        LTy::Chan(_) => "AlxChan *".into(),
        LTy::Lock => "AlxLock *".into(),
        LTy::Atomic => "int64_t *".into(),
        LTy::I64 | LTy::IntK(_) => "int64_t".into(),
        LTy::F64 => "double".into(),
        LTy::PInt => "AlxPInt".into(),
        LTy::Bool => "bool".into(),
        LTy::Str => "AlxStr".into(),
        LTy::Unit => "int8_t".into(),
        LTy::Arr(_) | LTy::Tup(_) => ty_name(t),
        LTy::Range => "AlxRange".into(),
        LTy::Gen(_) => "AlxGen *".into(),
    }
}

/// The C type in memory (array elements, struct fields): integers take
/// their own width there; in registers they are all int64_t.
pub fn cty_mem(t: &LTy) -> String {
    match t {
        LTy::IntK(k) => match k {
            IntKind::I8 => "int8_t",
            IntKind::I16 => "int16_t",
            IntKind::I32 => "int32_t",
            IntKind::U8 => "uint8_t",
            IntKind::U16 => "uint16_t",
            IntKind::U32 => "uint32_t",
            IntKind::I64 | IntKind::U64 => "int64_t",
        }
        .into(),
        t => cty(t),
    }
}

fn zero(t: &LTy) -> String {
    match t {
        LTy::I64 | LTy::IntK(_) | LTy::Unit => "0".into(),
        LTy::F64 => "0.0".into(),
        LTy::Bool => "false".into(),
        LTy::Gen(_) | LTy::Task(_) | LTy::Chan(_) => "NULL".into(),
        // `{}`, not `{0}`: the first member may be an empty aggregate (a
        // struct with no fields, as a generic `T` can be).
        t => format!("({}){{}}", cty(t)),
    }
}

const BUILTIN_ARRS: [&str; 4] = ["Arr_I64", "Arr_Str", "Arr_PInt", "Arr_Bool"];

struct Gen<'a> {
    out: String,
    types_done: HashSet<String>,
    typedefs: String,
    p: &'a LProgram,
}

thread_local! {
    /// Types of tuple literals, declared after the bodies are emitted.
    static LITERAL_TYPES: std::cell::RefCell<Vec<LTy>> = const { std::cell::RefCell::new(vec![]) };
}

pub fn emit(p: &LProgram) -> String {
    let mut g = Gen { out: String::new(), types_done: HashSet::new(), typedefs: String::new(), p };
    // Collect every type used.
    let all_funcs: Vec<&LFunc> = p.funcs.iter().chain(p.gens.iter().map(|x| &x.func)).chain(p.workers.iter().map(|w| &w.func)).collect();
    for f in &all_funcs {
        for v in &f.vars {
            g.need_type(&v.ty);
        }
        g.need_type(&f.ret);
    }
    for gn in &p.gens {
        g.need_type(&gn.elem);
    }
    for t in &p.globals {
        g.need_type(t);
    }
    // Prototypes.
    let mut protos = String::new();
    for (i, x) in p.externs.iter().enumerate() {
        let params: Vec<&str> = x.params.iter().map(|t| ffi_cty(*t)).collect();
        let _ = writeln!(protos, "extern {} alx_ffi_{i}({}) __asm__(ALX_SYM({:?}));", ffi_cty(x.ret), if params.is_empty() { "void".into() } else { params.join(", ") }, x.sym);
    }
    for (k, t) in p.globals.iter().enumerate() {
        let _ = writeln!(protos, "static {} alx_global{k};", cty(t));
    }
    for f in &p.funcs {
        let _ = writeln!(protos, "{};", proto(f));
    }
    for gn in &p.gens {
        let caps: Vec<String> = gn.captures.iter().map(|v| format!("{} c{v}", cty(&gn.func.vars[*v].ty))).collect();
        let _ = writeln!(protos, "static AlxGen *gen{}_new({});", gn.id, if caps.is_empty() { "void".into() } else { caps.join(", ") });
    }
    for w in &p.workers {
        let _ = writeln!(protos, "static void worker{}(const void *in_, void *out_);", w.id);
    }
    // Bodies.
    for gn in &p.gens {
        g.generator(gn);
    }
    for w in &p.workers {
        g.worker(w);
    }
    for f in &p.funcs {
        if !f.external {
            g.func(f);
        }
    }
    for t in LITERAL_TYPES.with(|l| std::mem::take(&mut *l.borrow_mut())) {
        g.need_type(&t);
    }
    let main = p.funcs.iter().find(|f| f.is_main).map(|f| f.name.clone()).unwrap_or_default();
    let mut s = String::new();
    s.push_str("/* Generated by alx. */\n#include \"alx.h\"\n\n");
    s.push_str(&g.typedefs);
    s.push('\n');
    s.push_str(&protos);
    s.push('\n');
    s.push_str(&g.out);
    if !main.is_empty() {
        let _ = writeln!(s, "int main(int argc, char **argv) {{\n    alx_set_args(argc, argv);\n    alx_init();\n    {main}();\n    fflush(stdout);\n    return 0;\n}}");
    }
    s
}

/// The C type of an `extern def` parameter or result.
fn ffi_cty(t: FfiTy) -> &'static str {
    match t {
        FfiTy::Int(k) => match k {
            IntKind::I8 => "int8_t",
            IntKind::I16 => "int16_t",
            IntKind::I32 => "int32_t",
            IntKind::I64 => "int64_t",
            IntKind::U8 => "uint8_t",
            IntKind::U16 => "uint16_t",
            IntKind::U32 => "uint32_t",
            IntKind::U64 => "uint64_t",
        },
        FfiTy::F64 => "double",
        FfiTy::Bool => "bool",
        FfiTy::Str => "const char *",
        FfiTy::Bytes => "uint8_t *",
        FfiTy::Ptr => "void *",
        FfiTy::Unit => "void",
    }
}

fn proto(f: &LFunc) -> String {
    proto_named(f, false)
}

/// The prototype; with `copies`, aggregate parameters get the names
/// `a<v>` and the body starts by copying them into the usual locals (see
/// `Gen::func`).
fn proto_named(f: &LFunc, copies: bool) -> String {
    let params: Vec<String> = f
        .params
        .iter()
        .map(|v| {
            let t = &f.vars[*v].ty;
            if copies && aggregate(t) {
                format!("{} a{v}", cty(t))
            } else {
                format!("{} {}", cty(t), vname(f, *v))
            }
        })
        .collect();
    let linkage = if f.external || f.name.starts_with("alx_lib_") { "" } else { "static " };
    if f.is_main {
        return format!("static void {}(void)", f.name);
    }
    let ret = if f.ret == LTy::Unit { "void".to_string() } else { cty(&f.ret) };
    format!("{linkage}{ret} {}({})", f.name, if params.is_empty() { "void".into() } else { params.join(", ") })
}

/// A C struct type (passed by value as an aggregate).
fn aggregate(t: &LTy) -> bool {
    matches!(t, LTy::Arr(_) | LTy::Tup(_) | LTy::Str)
}

fn vname(f: &LFunc, v: V) -> String {
    format!("v{v}_{}", f.vars[v].name)
}

fn c_str(s: &str) -> String {
    c_bytes(s.as_bytes())
}

fn c_bytes(s: &[u8]) -> String {
    let mut o = String::from("\"");
    for &b in s {
        match b {
            b'"' => o.push_str("\\\""),
            b'\\' => o.push_str("\\\\"),
            b'\n' => o.push_str("\\n"),
            b'\t' => o.push_str("\\t"),
            0x20..=0x7e => o.push(b as char),
            _ => {
                let _ = write!(o, "\\{b:03o}");
            }
        }
    }
    o.push('"');
    o
}

/// How the current function returns errors / values.
#[derive(Clone, Copy, PartialEq)]
enum Ctx {
    Plain,
    Main,
    Gen,
    Worker,
}

struct FnEmit<'f> {
    p: &'f LProgram,
    f: &'f LFunc,
    ctx: Ctx,
    /// In a generator: variables are fields of `g`.
    yields: usize,
    out: String,
    ind: usize,
}

impl Gen<'_> {
    fn need_type(&mut self, t: &LTy) {
        match t {
            LTy::Arr(inner) => {
                self.need_type(inner);
                let n = ty_name(t);
                if !BUILTIN_ARRS.contains(&n.as_str()) && self.types_done.insert(n.clone()) {
                    let _ = writeln!(self.typedefs, "ALX_ARR({}, {n})", cty_mem(inner));
                }
            }
            LTy::Tup(ts) => {
                for x in ts {
                    self.need_type(x);
                }
                let n = ty_name(t);
                if self.types_done.insert(n.clone()) {
                    let fields: Vec<String> = ts.iter().enumerate().map(|(i, x)| format!("{} f{i};", cty_mem(x))).collect();
                    let _ = writeln!(self.typedefs, "typedef struct {{ {} }} {n};", fields.join(" "));
                }
            }
            LTy::Gen(inner) | LTy::Task(inner) | LTy::Chan(inner) => self.need_type(inner),
            _ => {}
        }
    }

    fn func(&mut self, f: &LFunc) {
        let ctx = if f.is_main {
            Ctx::Main
        } else {
            Ctx::Plain
        };
        let mut e = FnEmit { p: self.p, f, ctx, yields: 0, out: String::new(), ind: 1 };
        let _ = writeln!(self.out, "{} {{", proto_named(f, true));
        for (i, v) in f.vars.iter().enumerate() {
            if !f.params.contains(&i) {
                let _ = writeln!(e.out, "    {} {} = {};", cty(&v.ty), vname(f, i), zero(&v.ty));
            } else if aggregate(&v.ty) {
                // A by-value struct parameter (a slice, a Str, a tuple) is
                // passed in memory the callee doesn't own on most ABIs, so
                // the C compiler must assume stores through its data
                // pointer may change it, and reloads it in loops. A local
                // copy (whose address is never taken) lives in registers.
                let _ = writeln!(e.out, "    {} {} = a{i};", cty(&v.ty), vname(f, i));
            }
        }
        e.block(&f.body);
        if f.ret != LTy::Unit && !matches!(f.body.last(), Some(LS::Return(_))) {
            let _ = writeln!(e.out, "    alx_panic(\"function ended without a value\", \"{}\");", f.name);
        }
        self.out.push_str(&e.out);
        self.out.push_str("}\n\n");
    }

    fn worker(&mut self, w: &LWorker) {
        let f = &w.func;
        let mut e = FnEmit { p: self.p, f, ctx: Ctx::Worker, yields: 0, out: String::new(), ind: 1 };
        let _ = writeln!(self.out, "static void worker{}(const void *in_, void *out_) {{", w.id);
        for (i, v) in f.vars.iter().enumerate() {
            if f.params.contains(&i) {
                let _ = writeln!(e.out, "    {} {} = *(const {} *)in_;", cty(&v.ty), vname(f, i), cty_mem(&v.ty));
            } else {
                let _ = writeln!(e.out, "    {} {} = {};", cty(&v.ty), vname(f, i), zero(&v.ty));
            }
        }
        e.block(&f.body);
        self.out.push_str(&e.out);
        self.out.push_str("}\n\n");
    }

    fn generator(&mut self, gn: &LGen) {
        let f = &gn.func;
        let id = gn.id;
        let _ = writeln!(self.out, "typedef struct {{\n    AlxGen base;\n    int state;");
        for (i, v) in f.vars.iter().enumerate() {
            let _ = writeln!(self.out, "    {} {};", cty(&v.ty), vname(f, i));
        }
        let _ = writeln!(self.out, "}} Gen{id};\n");
        let mut e = FnEmit { p: self.p, f, ctx: Ctx::Gen, yields: 0, out: String::new(), ind: 1 };
        e.block(&f.body);
        let n = e.yields;
        let elem = cty(&gn.elem);
        let _ = writeln!(self.out, "static bool gen{id}_next(AlxGen *g_, void *out_) {{\n    Gen{id} *g = (Gen{id} *)g_;\n    {elem} *out = out_;\n    (void)out;");
        // The state lives in C locals while running (array stores can't
        // alias them), saved back to the struct at each yield.
        for (i, v) in f.vars.iter().enumerate() {
            let n = vname(f, i);
            let _ = writeln!(self.out, "    {} {n} = g->{n};", cty(&v.ty));
        }
        let _ = writeln!(self.out, "    switch (g->state) {{\n    case 0: goto start;");
        for k in 1..=n {
            let _ = writeln!(self.out, "    case {k}: goto y{k};");
        }
        let _ = writeln!(self.out, "    default: return false;\n    }}\nstart:;");
        self.out.push_str(&e.out);
        let _ = writeln!(self.out, "    g->state = -1;\n    return false;\n}}\n");
        let caps: Vec<String> = gn.captures.iter().map(|v| format!("{} c{v}", cty(&f.vars[*v].ty))).collect();
        let _ = writeln!(self.out, "static AlxGen *gen{id}_new({}) {{\n    Gen{id} *g = alx_alloc(sizeof *g);\n    memset(g, 0, sizeof *g);\n    g->base.next = gen{id}_next;", if caps.is_empty() { "void".into() } else { caps.join(", ") });
        for v in &gn.captures {
            let _ = writeln!(self.out, "    g->{} = c{v};", vname(f, *v));
        }
        let _ = writeln!(self.out, "    return &g->base;\n}}\n");
        let _ = self.p;
    }
}

impl FnEmit<'_> {
    fn line(&mut self, s: &str) {
        for _ in 0..self.ind {
            self.out.push_str("    ");
        }
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn v(&self, v: V) -> String {
        vname(self.f, v)
    }

    /// The static type of a (channel-valued) expression.
    fn ty_of(&self, e: &LE) -> Option<LTy> {
        match e {
            LE::Var(v) => Some(self.f.vars[*v].ty.clone()),
            LE::ChanNew(t, _) => Some(LTy::Chan(Box::new(t.clone()))),
            LE::Field(x, i) => match self.ty_of(x)? {
                LTy::Tup(ts) => ts.get(*i).cloned(),
                _ => None,
            },
            LE::Index { arr, .. } => match self.ty_of(arr)? {
                LTy::Arr(t) => Some(*t),
                _ => None,
            },
            LE::Cond(_, a, _) => self.ty_of(a),
            LE::Call(f, _) => self.p.funcs.iter().find(|x| &x.name == f).map(|x| x.ret.clone()),
            _ => None,
        }
    }

    /// The in-memory C element type of a channel expression.
    fn chan_elem(&self, ch: &LE) -> String {
        match self.ty_of(ch) {
            Some(LTy::Chan(t)) => cty_mem(&t),
            t => panic!("cgen: channel expression of unknown type ({t:?}): {ch:?}"),
        }
    }

    fn block(&mut self, ss: &[LS]) {
        for s in ss {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &LS) {
        match s {
            LS::SetGlobal(k, v) => {
                let v = self.e(v);
                self.line(&format!("alx_global{k} = {v};"));
            }
            LS::RegionFree(r) => {
                let r = self.e(r);
                self.line(&format!("alx_region_free({r});"));
            }
            LS::RegionEnter { region, saved } => {
                let (r, sv) = (self.v(*region), self.v(*saved));
                self.line(&format!("{sv} = alx_region_cur(); {r} = alx_region_enter();"));
            }
            LS::RegionExit { region, saved } => {
                let (r, sv) = (self.v(*region), self.v(*saved));
                self.line(&format!("alx_region_exit({r}, {sv});"));
            }
            LS::RegionUse { region, saved } => {
                let (r, sv) = (self.e(region), self.v(*saved));
                self.line(&format!("{sv} = alx_region_use({r});"));
            }
            LS::RegionRestore(saved) => {
                let sv = self.v(*saved);
                self.line(&format!("alx_region_set({sv});"));
            }
            LS::Spawn { dst, worker, env } => {
                let w = &self.p.workers[*worker];
                let (it, ot) = (cty_mem(&w.input), cty_mem(&w.func.ret));
                let (x, d) = (self.e(env), self.v(*dst));
                self.line(&format!("{{ {it} env_ = {x}; {d} = alx_spawn(worker{}, &env_, sizeof env_, sizeof({ot})); }}", w.id));
            }
            LS::Wait { task, ok, val, msg } => {
                let vt = cty_mem(&self.f.vars[*val].ty);
                let (t, o, v, m) = (self.e(task), self.v(*ok), self.v(*val), self.v(*msg));
                self.line(&format!("{{ {vt} out_ = {v}; {o} = alx_task_wait({t}, &out_, &{m}); {v} = out_; }}"));
            }
            LS::ChanSend { ch, val, loc } => {
                let vt = self.chan_elem(ch);
                let (c, x) = (self.e(ch), self.e(val));
                self.line(&format!("{{ AlxChan *ch_ = {c}; {vt} val_ = {x}; alx_chan_send(ch_, &val_, {}); }}", c_str(loc)));
            }
            LS::ChanRecv { ch, ok, val } => {
                let vt = cty_mem(&self.f.vars[*val].ty);
                let (c, o, v) = (self.e(ch), self.v(*ok), self.v(*val));
                self.line(&format!("{{ {vt} out_ = {v}; {o} = alx_chan_recv({c}, &out_); {v} = out_; }}"));
            }
            LS::ChanClose { ch, loc } => {
                let c = self.e(ch);
                self.line(&format!("alx_chan_close({c}, {});", c_str(loc)));
            }
            LS::Lock(l, loc) => {
                let l = self.e(l);
                self.line(&format!("alx_lock({l}, {});", c_str(loc)));
            }
            LS::LockClearPoison(l) => {
                let l = self.e(l);
                self.line(&format!("alx_lock_clear_poison({l});"));
            }
            LS::Unlock(l) => {
                let l = self.e(l);
                self.line(&format!("alx_unlock({l});"));
            }
            LS::AtomicStore(a, v) => {
                let (a, v) = (self.e(a), self.e(v));
                self.line(&format!("__atomic_store_n({a}, {v}, __ATOMIC_SEQ_CST);"));
            }
            LS::Select { cases, default, dst } => {
                self.line("{");
                self.ind += 1;
                // Evaluate every case expression once, in order.
                let mut inits = vec![];
                for (i, c) in cases.iter().enumerate() {
                    match c {
                        SelCase::Send { ch, val } => {
                            let (cx, vx) = (self.e(ch), self.e(val));
                            let vt = self.chan_elem(ch);
                            self.line(&format!("AlxChan *c{i}_ = {cx}; {vt} s{i}_ = {vx};"));
                            inits.push(format!("{{ c{i}_, &s{i}_, 1, 0 }}"));
                        }
                        SelCase::Recv { ch, ok, val } => {
                            let vt = cty_mem(&self.f.vars[*val].ty);
                            let (cx, o, v) = (self.e(ch), self.v(*ok), self.v(*val));
                            self.line(&format!("AlxChan *c{i}_ = {cx}; {vt} r{i}_ = {v};"));
                            inits.push(format!("{{ c{i}_, &r{i}_, 0, {o} }}"));
                        }
                    }
                }
                let n = cases.len();
                let d = self.v(*dst);
                if n == 0 {
                    self.line(&format!("{d} = alx_select(NULL, 0, {default}, \"select\");"));
                } else {
                    self.line(&format!("AlxSelCase sc_[{n}] = {{ {} }};", inits.join(", ")));
                    self.line(&format!("{d} = alx_select(sc_, {n}, {default}, \"select\");"));
                }
                for (i, c) in cases.iter().enumerate() {
                    if let SelCase::Recv { ok, val, .. } = c {
                        let (o, v) = (self.v(*ok), self.v(*val));
                        self.line(&format!("{o} = sc_[{i}].ok != 0; {v} = r{i}_;"));
                    }
                }
                self.ind -= 1;
                self.line("}");
            }
            LS::Set(v, e) => {
                let x = self.e(e);
                let v = self.v(*v);
                self.line(&format!("{v} = {x};"));
            }
            LS::SetIndex { arr, idx, val, check } => {
                let (a, i, x) = (self.v(*arr), self.e(idx), self.e(val));
                match check {
                    Some(loc) => self.line(&format!("ALX_IDX_SET({a}, {i}, {}) = {x};", c_str(loc))),
                    None => self.line(&format!("{a}.ptr[{i}] = {x};")),
                }
            }
            LS::SetPlace { var, steps, val } => {
                let mut lv = self.v(*var);
                for st in steps {
                    lv = match st {
                        Step::Index(i, Some(loc)) => format!("ALX_IDX_SET({lv}, {}, {})", self.e(i), c_str(loc)),
                        Step::Index(i, None) => format!("({lv}).ptr[{}]", self.e(i)),
                        Step::Field(k) => format!("({lv}).f{k}"),
                    };
                }
                let x = self.e(val);
                self.line(&format!("{lv} = {x};"));
            }
            LS::Push(v, e) => {
                let ty = ty_name(&self.f.vars[*v].ty);
                let x = self.e(e);
                let v = self.v(*v);
                self.line(&format!("{ty}_push(&{v}, {x});"));
            }
            // A discarded tuple: its parts (its C type may not be declared).
            LS::Eval(LE::Tup(_, vs)) => {
                for v in vs {
                    let x = self.e(v);
                    self.line(&format!("(void)({x});"));
                }
            }
            LS::Eval(e) => {
                let x = self.e(e);
                self.line(&format!("(void)({x});"));
            }
            LS::If(c, a, b) => {
                let c = self.e(c);
                self.line(&format!("if ({c}) {{"));
                self.ind += 1;
                self.block(a);
                self.ind -= 1;
                if b.is_empty() {
                    self.line("}");
                } else {
                    self.line("} else {");
                    self.ind += 1;
                    self.block(b);
                    self.ind -= 1;
                    self.line("}");
                }
            }
            LS::Loop(l, body) => {
                self.line("for (;;) {");
                self.ind += 1;
                self.block(body);
                self.line(&format!("cont_{l}:;"));
                self.ind -= 1;
                self.line("}");
                self.line(&format!("brk_{l}:;"));
            }
            LS::Break(l) => self.line(&format!("goto brk_{l};")),
            LS::Continue(l) => self.line(&format!("goto cont_{l};")),
            LS::Return(v) => match (self.ctx, v) {
                (Ctx::Worker, Some(v)) => {
                    let x = self.e(v);
                    let t = cty_mem(&self.f.ret);
                    self.line(&format!("{{ *({t} *)out_ = {x}; return; }}"));
                }
                (Ctx::Plain, Some(v)) => {
                    let x = self.e(v);
                    self.line(&format!("return {x};"));
                }
                (Ctx::Gen, _) => self.line("{ g->state = -1; return false; }"),
                _ => self.line("return;"),
            },
            LS::NextOrBreak { source, dst, label } => {
                let (g, d) = (self.e(source), self.v(*dst));
                self.line(&format!("if (!({g})->next(({g}), &{d})) goto brk_{label};"));
            }
            LS::Yield(e) => {
                self.yields += 1;
                let k = self.yields;
                let x = self.e(e);
                self.line(&format!("*out = {x};"));
                self.line(&format!("g->state = {k};"));
                for i in 0..self.f.vars.len() {
                    let n = vname(self.f, i);
                    self.line(&format!("g->{n} = {n};"));
                }
                self.line("return true;");
                self.out.push_str(&format!("y{k}:;\n"));
            }
            LS::Pmap { dst, arr, worker, err } => {
                let d = self.v(*dst);
                let dt = self.f.vars[*dst].ty.clone();
                let LTy::Arr(out_t) = &dt else { unreachable!() };
                let a = self.e(arr);
                self.line("{");
                self.ind += 1;
                self.line(&format!("__typeof__({a}) in_ = {a};"));
                self.line(&format!("{d} = {}_cap(in_.len);", ty_name(&dt)));
                self.line(&format!("{d}.len = in_.len;"));
                match err {
                    None => self.line(&format!("alx_pmap(in_.ptr, in_.len, sizeof *in_.ptr, {d}.ptr, sizeof({}), worker{worker});", cty_mem(out_t))),
                    Some(e) => {
                        let ev = self.v(*e);
                        let rt = cty_mem(&self.f.vars[*e].ty);
                        self.line(&format!(
                            "alx_pmap_try(in_.ptr, in_.len, sizeof *in_.ptr, {d}.ptr, sizeof({}), worker{worker}, sizeof({rt}), offsetof({rt}, f1), &{ev});",
                            cty_mem(out_t)
                        ));
                    }
                }
                self.ind -= 1;
                self.line("}");
            }
            LS::Print(e) => {
                let x = self.e(e);
                self.line(&format!("alx_print_str({x});"));
            }
            LS::Puts(e, t) => {
                let x = self.e(e);
                let f = match t {
                    LTy::I64 => "alx_puts_i64",
                    LTy::IntK(IntKind::U64) => "alx_puts_u64",
                    LTy::IntK(_) => "alx_puts_i64",
                    LTy::F64 => "alx_puts_f64",
                    LTy::Str => "alx_puts_str",
                    LTy::Bool => "alx_puts_bool",
                    LTy::PInt => "alx_puts_pint",
                    LTy::Unit => {
                        self.line(&format!("(void)({x}); puts(\"\");"));
                        return;
                    }
                    _ => {
                        self.line(&format!("(void)({x}); alx_panic(\"`puts` of this type isn't supported yet\", \"puts\");"));
                        return;
                    }
                };
                self.line(&format!("{f}({x});"));
            }
            LS::Die(e) => {
                let x = self.e(e);
                self.line(&format!("alx_die_str({x});"));
            }
            LS::Panic(msg, loc) => self.line(&format!("alx_panic({}, {});", c_str(msg), c_str(loc))),
            LS::Exit(e) => {
                let x = self.e(e);
                self.line(&format!("exit((int)({x}));"));
            }
            LS::PanicStr(e) => {
                let x = self.e(e);
                self.line(&format!("alx_panic_str({x});"));
            }
            LS::SortInPlace(v, el) => {
                let f = match el {
                    LTy::I64 => "alx_sort_i64",
                    LTy::Str => "alx_sort_str",
                    _ => {
                        self.line("alx_panic(\"`sort` of this element type isn't supported yet\", \"sort\");");
                        return;
                    }
                };
                let v = self.v(*v);
                self.line(&format!("{f}(&{v});"));
            }
        }
    }

    fn e(&self, e: &LE) -> String {
        match e {
            LE::Global(k) => format!("alx_global{k}"),
            LE::RegionNew(p) => format!("alx_region_new_child({})", self.e(p)),
            LE::RegionBytes(r) => format!("alx_region_bytes({})", self.e(r)),
            LE::RegionOf(x) => format!("alx_region_of(({}).ptr)", self.e(x)),
            LE::RegionProgram => "alx_region_program()".into(),
            LE::ChanNew(t, cap) => format!("alx_chan_new({}, sizeof({}))", self.e(cap), cty_mem(t)),
            LE::ChanLen(c) => format!("alx_chan_len({})", self.e(c)),
            LE::LockPoisoned(l) => format!("alx_lock_poisoned({})", self.e(l)),
            LE::LockNew => "alx_lock_new()".into(),
            LE::NullTask(_) => "NULL".into(),
            LE::AtomicNew(v) => format!("alx_atomic_new({})", self.e(v)),
            LE::AtomicLoad(a) => format!("__atomic_load_n({}, __ATOMIC_SEQ_CST)", self.e(a)),
            LE::AtomicRmw(AtomicOp::Add, a, v) => format!("(__atomic_add_fetch({}, {}, __ATOMIC_SEQ_CST))", self.e(a), self.e(v)),
            LE::AtomicRmw(AtomicOp::Swap, a, v) => format!("(__atomic_exchange_n({}, {}, __ATOMIC_SEQ_CST))", self.e(a), self.e(v)),
            LE::AtomicCas(a, o, n) => format!("({{ int64_t exp_ = {}; __atomic_compare_exchange_n({}, &exp_, {}, false, __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST); }})", self.e(o), self.e(a), self.e(n)),
            LE::Var(v) => self.v(*v),
            LE::I(i) => {
                if *i == i64::MIN {
                    "INT64_MIN".into()
                } else {
                    format!("INT64_C({i})")
                }
            }
            LE::F(v) => c_double(*v),
            LE::FArith(op, a, b) => {
                let sym = match op {
                    Op::Add => "+",
                    Op::Sub => "-",
                    Op::Mul => "*",
                    _ => "/",
                };
                format!("({} {sym} {})", self.e(a), self.e(b))
            }
            LE::FNeg(x) => format!("(-{})", self.e(x)),
            LE::Prim(p, args) => {
                let a: Vec<String> = args.iter().map(|x| self.e(x)).collect();
                let u = |x: &str| format!("((uint64_t)({x}))");
                match p {
                    Prim::And => format!("({} & {})", a[0], a[1]),
                    Prim::Or => format!("({} | {})", a[0], a[1]),
                    Prim::Xor => format!("({} ^ {})", a[0], a[1]),
                    Prim::AndNot => format!("({} & ~{})", a[0], a[1]),
                    Prim::Not => format!("(~{})", a[0]),
                    Prim::Shl => format!("((int64_t)({} << ({})))", u(&a[0]), a[1]),
                    Prim::ShrS => format!("({} >> ({}))", a[0], a[1]),
                    Prim::ShrU => format!("((int64_t)({} >> ({})))", u(&a[0]), a[1]),
                    Prim::MulOvf => format!("alx_mul_ovf({}, {})", a[0], a[1]),
                    Prim::ULt => format!("({} < {})", u(&a[0]), u(&a[1])),
                    Prim::ULe => format!("({} <= {})", u(&a[0]), u(&a[1])),
                    Prim::UDiv => format!("((int64_t)({} / {}))", u(&a[0]), u(&a[1])),
                    Prim::URem => format!("((int64_t)({} % {}))", u(&a[0]), u(&a[1])),
                    Prim::UMulHi => format!("((int64_t)(((unsigned __int128){} * {}) >> 64))", u(&a[0]), u(&a[1])),
                    Prim::Wrap(k) => format!("((int64_t)({})({}))", cty_mem(&LTy::IntK(*k)), a[0]),
                    Prim::UToF => format!("((double){})", u(&a[0])),
                }
            }
            LE::B(b) => if *b { "true" } else { "false" }.into(),
            LE::S(s) => format!("alx_str_lit({}, {})", c_str(s), s.len()),
            LE::SB(b) => format!("alx_str_lit({}, {})", c_bytes(b), b.len()),
            LE::Loc(s) => c_str(s),
            LE::Unit => "0".into(),
            LE::Tup(t, vs) => {
                // A literal's type may be held by no variable (`Zero[T].new.v`).
                LITERAL_TYPES.with(|l| l.borrow_mut().push(t.clone()));
                // Fields whose value is all-zero bytes are left to C's zero
                // fill: a closure or sum value names one live variant, and
                // spelling every other variant's zero payload made lines of
                // tens of kilobytes (crypto/tls's builders).
                if vs.iter().any(c_zero) {
                    let parts: Vec<String> = vs.iter().enumerate().filter(|(_, x)| !c_zero(x)).map(|(k, x)| format!(".f{k} = {}", self.e(x))).collect();
                    return format!("(({}){{{}}})", cty(t), parts.join(", "));
                }
                format!("(({}){{{}}})", cty(t), vs.iter().map(|x| self.e(x)).collect::<Vec<_>>().join(", "))
            }
            LE::Field(x, i) => format!("({}).f{i}", self.e(x)),
            LE::Arith(op, a, b, ovf) => {
                let (a, b) = (self.e(a), self.e(b));
                match ovf {
                    Ovf::Panic(loc) => {
                        let f = match op {
                            Op::Add => "alx_add",
                            Op::Sub => "alx_sub",
                            Op::Mul => "alx_mul",
                            Op::Div => "alx_div",
                            Op::Rem => "alx_rem",
                            Op::Pow => "alx_pow",
                            _ => unreachable!(),
                        };
                        format!("{f}({a}, {b}, {})", c_str(loc))
                    }
                    Ovf::Wrap => match op {
                        Op::Add => format!("alx_wadd({a}, {b})"),
                        Op::Sub => format!("alx_wsub({a}, {b})"),
                        Op::Mul => format!("alx_wmul({a}, {b})"),
                        Op::Div => format!("alx_div({a}, {b}, \"wrap\")"),
                        Op::Rem => format!("alx_rem({a}, {b}, \"wrap\")"),
                        _ => format!("alx_pow({a}, {b}, \"wrap\")"),
                    },
                    Ovf::Unchecked => match op {
                        Op::Add => format!("({a} + {b})"),
                        Op::Sub => format!("({a} - {b})"),
                        Op::Mul => format!("({a} * {b})"),
                        // Proven: divisor is a nonzero constant other than -1.
                        Op::Div => format!("alx_div({a}, {b}, \"proven\")"),
                        Op::Rem => format!("alx_rem({a}, {b}, \"proven\")"),
                        _ => format!("alx_pow({a}, {b}, \"proven\")"),
                    },
                }
            }
            LE::PArith(op, a, b) => {
                let (a, b) = (self.e(a), self.e(b));
                match op {
                    Op::Add => format!("alx_p_add({a}, {b})"),
                    Op::Sub => format!("alx_p_sub({a}, {b})"),
                    Op::Mul => format!("alx_p_mul({a}, {b})"),
                    Op::Div => format!("alx_p_div({a}, {b}, \"div\")"),
                    Op::Rem => format!("alx_p_rem({a}, {b}, \"rem\")"),
                    Op::Pow => format!("alx_p_pow({a}, {b}, \"pow\")"),
                    Op::Eq => format!("(alx_p_cmp({a}, {b}) == 0)"),
                    Op::Ne => format!("(alx_p_cmp({a}, {b}) != 0)"),
                    Op::Lt => format!("(alx_p_cmp({a}, {b}) < 0)"),
                    Op::Le => format!("(alx_p_cmp({a}, {b}) <= 0)"),
                    Op::Gt => format!("(alx_p_cmp({a}, {b}) > 0)"),
                    Op::Ge => format!("(alx_p_cmp({a}, {b}) >= 0)"),
                    _ => unreachable!(),
                }
            }
            LE::Cmp(op @ (Op::Eq | Op::Ne), a, b, LTy::Str) if palindrome_test(a, b).is_some() => {
                let x = self.e(palindrome_test(a, b).unwrap());
                let neg = if *op == Op::Ne { "!" } else { "" };
                format!("({neg}alx_str_is_pal({x}))")
            }
            LE::Cmp(op, a, b, t) => {
                let (a, b) = (self.e(a), self.e(b));
                let sym = match op {
                    Op::Eq => "==",
                    Op::Ne => "!=",
                    Op::Lt => "<",
                    Op::Le => "<=",
                    Op::Gt => ">",
                    Op::Ge => ">=",
                    Op::And => "&&",
                    Op::Or => "||",
                    _ => unreachable!(),
                };
                match t {
                    LTy::Str => match op {
                        Op::Eq => format!("alx_str_eq({a}, {b})"),
                        Op::Ne => format!("(!alx_str_eq({a}, {b}))"),
                        _ => format!("(alx_str_cmp({a}, {b}) {sym} 0)"),
                    },
                    LTy::PInt => format!("(alx_p_cmp({a}, {b}) {sym} 0)"),
                    _ => format!("({a} {sym} {b})"),
                }
            }
            LE::Neg(x, ovf) => match ovf {
                Ovf::Panic(loc) => format!("alx_neg({}, {})", self.e(x), c_str(loc)),
                _ => format!("alx_wsub(0, {})", self.e(x)),
            },
            LE::Not(x) => format!("(!{})", self.e(x)),
            LE::Cond(c, a, b) => format!("({} ? {} : {})", self.e(c), self.e(a), self.e(b)),
            LE::Call(f, args) => format!("{f}({})", args.iter().map(|x| self.e(x)).collect::<Vec<_>>().join(", ")),
            LE::Ffi(i, args) => {
                let x = &self.p.externs[*i];
                let (mut pre, mut post, mut cargs) = (String::new(), String::new(), vec![]);
                for (k, (t, a)) in x.params.iter().zip(args).enumerate() {
                    let ae = self.e(a);
                    match t {
                        FfiTy::Str => {
                            let _ = write!(pre, "AlxStr a{k}_ = {ae}; char *c{k}_ = alx_cstr_new(a{k}_); ");
                            let _ = write!(post, "free(c{k}_); ");
                            cargs.push(format!("c{k}_"));
                        }
                        FfiTy::Bytes => {
                            let _ = write!(pre, "__auto_type a{k}_ = {ae}; ");
                            cargs.push(format!("(uint8_t *)a{k}_.ptr"));
                        }
                        FfiTy::Ptr => cargs.push(format!("(void *)(intptr_t)({ae})")),
                        FfiTy::Int(_) => cargs.push(format!("({})({ae})", ffi_cty(*t))),
                        _ => cargs.push(ae),
                    }
                }
                let call = format!("alx_ffi_{i}({})", cargs.join(", "));
                let (get, val) = match x.ret {
                    FfiTy::Unit => (format!("{call};"), "(int8_t)0".to_string()),
                    FfiTy::Ptr => (format!("void *r_ = {call};"), "(int64_t)(intptr_t)r_".to_string()),
                    FfiTy::Int(_) => (format!("{} r_ = {call};", ffi_cty(x.ret)), "(int64_t)r_".to_string()),
                    t => (format!("{} r_ = {call};", ffi_cty(t)), "r_".to_string()),
                };
                format!("({{ {pre}alx_ffi_enter(); {get} alx_ffi_save_errno(); {post}{val}; }})")
            }
            LE::Rt(rt, args) => {
                let a: Vec<String> = args.iter().map(|x| self.e(x)).collect();
                let s = |f: &str| format!("{f}({})", a.join(", "));
                match rt {
                    Rt::IntToS => s("alx_int_to_s"),
                    Rt::PIntToS => s("alx_p_to_s"),
                    Rt::StrRev => s("alx_str_rev"),
                    Rt::StrDelete => s("alx_str_delete"),
                    Rt::StrSplit => s("alx_str_split"),
                    Rt::StrJoin => s("alx_str_join"),
                    Rt::StrToI => s("alx_str_to_i"),
                    Rt::StrIndex => s("alx_str_index"),
                    Rt::StrChar => s("alx_str_charlen"),
                    Rt::StrByte => {
                        if a[2] == "INT64_C(0)" {
                            format!("alx_str_byte({}, {})", a[0], a[1])
                        } else {
                            s("alx_str_sub")
                        }
                    }
                    Rt::StrLen => format!("({}).len", a[0]),
                    Rt::NDigits => s("alx_int_ndigits"),
                    Rt::PNDigits => s("alx_p_ndigits"),
                    Rt::Isqrt => s("alx_isqrt"),
                    Rt::Digits => s("alx_digits"),
                    Rt::PDigits => s("alx_p_digits"),
                    Rt::SatAdd => s("alx_sat_add"),
                    Rt::ArrCopy => {
                        // The array type isn't on the expression: use typeof.
                        format!("({{ __typeof__({0}) src_ = {0}; __typeof__(src_) dst_ = src_; dst_.ptr = alx_alloc((size_t)(src_.len ? src_.len : 1) * sizeof *src_.ptr); if (src_.len) memcpy(dst_.ptr, src_.ptr, (size_t)src_.len * sizeof *src_.ptr); dst_.cap = src_.len; dst_; }})", a[0])
                    }
                    Rt::Even => s("alx_even"),
                    Rt::PEven => s("alx_p_even"),
                    Rt::PToI64 => s("alx_p_to_i64"),
                    Rt::PFromStr => s("alx_p_from_str"),
                    Rt::IntToF => format!("((double)({}))", a[0]),
                    Rt::FToI => s("alx_f_to_i"),
                    Rt::FSqrt => s("sqrt"),
                    Rt::FAbs => s("fabs"),
                    Rt::Math(f) => s(f.c_name()),
                    Rt::FBits => format!("alx_f_bits({})", a[0]),
                    Rt::FFromBits => format!("alx_f_from_bits({})", a[0]),
                    Rt::FToS => s("alx_f_to_s"),
                    Rt::FFmt => s("alx_f_fmt"),
                    Rt::FFmtE => s("alx_f_fmt_e"),
                    Rt::StrPad => s("alx_str_pad"),
                    Rt::StrQuote => s("alx_str_quote"),
                    Rt::StrCat if a.len() == 2 => format!("alx_str_cat2({}, {})", a[0], a[1]),
                    Rt::StrCat => format!("alx_str_cat({}, (AlxStr[]){{{}}})", a.len(), a.join(", ")),
                    Rt::U64ToS => s("alx_u64_to_s"),
                    Rt::IntFmt => s("alx_int_fmt"),
                    Rt::FToU64 => s("alx_f_to_u64"),
                    Rt::RuneToS => s("alx_rune_to_s"),
                    Rt::FileStatus => s("alx_file_status"),
                    Rt::FileRead => s("alx_file_read_or_empty"),
                    Rt::NowNs => s("alx_now_ns"),
                    Rt::RegionCur => s("alx_region_cur"),
                    Rt::RegionMark => s("alx_region_mark"),
                    Rt::RegionMarkLarges => s("alx_region_mark_larges"),
                    Rt::RegionReset => s("alx_region_reset"),
                    Rt::Errno => "((int64_t)alx_ffi_errno_)".into(),
                    Rt::Strerror => s("alx_strerror"),
                    Rt::StrFromCstr => format!("alx_str_from_cstr((const char *)(intptr_t)({}))", a[0]),
                    Rt::StrFromPtr => format!("alx_str_from_ptr((const char *)(intptr_t)({}), {})", a[0], a[1]),
                    Rt::CapBegin => s("alx_cap_begin"),
                    Rt::CapEnd => s("alx_cap_end"),
                    Rt::StrFromBytes => format!("alx_str_from_bytes((const uint8_t *)({0}).ptr, ({0}).len)", a[0]),
                }
            }
            LE::Index { arr, idx, check } => {
                let (a, i) = (self.e(arr), self.e(idx));
                match check {
                    Some(loc) => format!("ALX_IDX({a}, {i}, {})", c_str(loc)),
                    None => format!("({a}).ptr[{i}]"),
                }
            }
            LE::Len(x) => format!("({}).len", self.e(x)),
            LE::ArrLit(t, vs) => {
                LITERAL_TYPES.with(|l| l.borrow_mut().push(LTy::Arr(Box::new(t.clone()))));
                let n = ty_name(&LTy::Arr(Box::new(t.clone())));
                if vs.is_empty() {
                    format!("{n}_cap(0)")
                } else {
                    format!("{n}_lit({}, ({}[]){{{}}})", vs.len(), cty_mem(t), vs.iter().map(|x| self.e(x)).collect::<Vec<_>>().join(", "))
                }
            }
            LE::ArrNew(t, n, fill, loc) => {
                LITERAL_TYPES.with(|l| l.borrow_mut().push(LTy::Arr(Box::new(t.clone()))));
                format!("{}_new({}, {}, {})", ty_name(&LTy::Arr(Box::new(t.clone()))), self.e(n), self.e(fill), c_str(loc))
            }
            LE::ArrWithCap(t, n) => {
                LITERAL_TYPES.with(|l| l.borrow_mut().push(LTy::Arr(Box::new(t.clone()))));
                format!("{}_cap({})", ty_name(&LTy::Arr(Box::new(t.clone()))), self.e(n))
            }
            LE::Slice(t, a, start, len) => format!("(({}){{({}).ptr + {}, {}, 0}})", ty_name(t), self.e(a), self.e(start), self.e(len)),
            LE::Range(lo, hi, ex) => format!("((AlxRange){{{}, {}, {}}})", self.e(lo), self.e(hi), ex),
            LE::RangeField(r, k) => format!("({}).{}", self.e(r), ["lo", "hi", "excl"][*k as usize]),
            LE::GenNew(id, vals) => format!("gen{id}_new({})", vals.iter().map(|x| self.e(x)).collect::<Vec<_>>().join(", ")),
            LE::ToP(x) => format!("alx_p_from({})", self.e(x)),
        }
    }
}

/// `s == s.reverse` (either side) on a variable: the variable.
pub fn palindrome_test<'a>(a: &'a LE, b: &'a LE) -> Option<&'a LE> {
    let rev_of = |x: &LE, r: &LE| matches!((x, r), (LE::Var(v), LE::Rt(Rt::StrRev, args)) if matches!(args.as_slice(), [LE::Var(w)] if w == v));
    if rev_of(a, b) {
        Some(a)
    } else if rev_of(b, a) {
        Some(b)
    } else {
        None
    }
}

/// A C double literal (the lexer can produce infinities from huge literals).
fn c_double(v: f64) -> String {
    if v.is_nan() {
        "NAN".into()
    } else if v.is_infinite() {
        if v > 0.0 { "INFINITY".into() } else { "(-INFINITY)".into() }
    } else {
        // Debug is the shortest round-trip form, always with `.` or `e`.
        format!("({v:?})")
    }
}
