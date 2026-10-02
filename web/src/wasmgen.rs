//! LIR → WebAssembly: the browser's backend. Same semantics as the C backend
//! and the Cranelift JIT; the runtime is `rt.rs`, imported from the module
//! that compiled the program (memory included), so programs run at native
//! wasm speed with no JS in the loop.
//!
//! Values flatten to scalars (`lay`): i64 words, and bytes (bool) carried as
//! i32. Aggregates live in memory with the native C layout.
//!
//! Generators: wasm has no goto, so a generator can't jump back into the
//! middle of a loop. Instead `next` re-enters its body with `resume` = the
//! yield to continue after: every statement before a yield is guarded by
//! `resume == 0`, branches and loops containing the target are re-entered
//! without evaluating their conditions, and reaching the target yield clears
//! `resume`. All variables are saved to the generator's state at each yield
//! and reloaded on entry.

use crate::rt;
use alx::lir::*;
use std::collections::HashMap;
use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, ElementSection, Elements, EntityType, ExportKind, ExportSection, Function, FunctionSection, ImportSection, InstructionSink, MemArg, MemoryType, Module, RefType, TableSection, TableType, TypeSection, ValType,
};

const W: ValType = ValType::I64;
const I: ValType = ValType::I32;
const D: ValType = ValType::F64;

/// Runtime entry points: name and signature ("args>results", j = i64, i = i32).
const RT: &[(&str, &str)] = &[
    ("alxr_alloc", "j>j"),
    ("alxr_zalloc", "j>j"),
    ("alxr_grow", "jjjj>j"),
    ("alxr_copy", "jjj>j"),
    ("alxr_panic", "jjjj>"),
    ("alxr_overflow", "jj>"),
    ("alxr_die_str", "jj>"),
    ("alxr_file_status", "jj>j"),
    ("alxr_file_read_or_empty", "jj>"),
    ("alxr_pow", "jjjj>j"),
    ("alxr_mul_chk", "jj>j"),
    ("alxr_isqrt", "jjj>j"),
    ("alxr_digits", "jjj>"),
    ("alxr_int_to_s", "j>"),
    ("alxr_int_ndigits", "j>j"),
    ("alxr_str_rev", "jj>"),
    ("alxr_str_is_pal", "jj>i"),
    ("alxr_str_eq", "jjjj>i"),
    ("alxr_str_cmp", "jjjj>i"),
    ("alxr_str_delete", "jjjj>"),
    ("alxr_str_split", "jjjj>"),
    ("alxr_str_to_i", "jj>j"),
    ("alxr_str_charlen", "jjj>j"),
    ("alxr_str_sub", "jjjj>"),
    ("alxr_sort_i64", "jj>"),
    ("alxr_sort_str", "jj>"),
    ("alxr_puts_i64", "j>"),
    ("alxr_puts_str", "jj>"),
    ("alxr_print_str", "jj>"),
    ("alxr_puts_bool", "i>"),
    ("alxr_puts_unit", ">"),
    ("alxr_p_add", "jjjj>"),
    ("alxr_p_sub", "jjjj>"),
    ("alxr_p_mul", "jjjj>"),
    ("alxr_p_div", "jjjjjj>"),
    ("alxr_p_rem", "jjjjjj>"),
    ("alxr_p_pow", "jjjjjj>"),
    ("alxr_p_cmp", "jjjj>i"),
    ("alxr_p_even", "jj>i"),
    ("alxr_p_to_i64", "jjjj>j"),
    ("alxr_p_to_s", "jj>"),
    ("alxr_p_ndigits", "jj>j"),
    ("alxr_p_digits", "jjjj>"),
    ("alxr_puts_pint", "jj>"),
    ("alxr_puts_f64", "f>"),
    ("alxr_f_to_s", "f>"),
    ("alxr_f_fmt", "fj>"),
    ("alxr_f_fmt_e", "fji>"),
    ("alxr_str_pad", "jjjj>"),
    ("alxr_str_quote", "jj>"),
    ("alxr_f_to_i", "fjj>j"),
    ("alxr_math", "fffj>f"),
    ("alxr_str_cat", "jj>"),
    ("alxr_umulhi", "jj>j"),
    ("alxr_puts_u64", "j>"),
    ("alxr_u64_to_s", "j>"),
    ("alxr_int_fmt", "jjii>"),
    ("alxr_f_to_u64", "fjj>j"),
    ("alxr_rune_to_s", "j>"),
    ("alxr_str_from_bytes", "jj>"),
];

fn sig_of(s: &str) -> (Vec<ValType>, Vec<ValType>) {
    let (a, r) = s.split_once('>').unwrap();
    let t = |c: char| match c {
        'j' => W,
        'f' => D,
        _ => I,
    };
    (a.chars().map(t).collect(), r.chars().map(t).collect())
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum F {
    /// 8-byte word: i64.
    Wd,
    /// 1 byte in memory, i32 in locals (bool).
    By,
    /// 8-byte IEEE double.
    Fl,
    /// A narrow integer: (bytes, signed) in memory, i64 in locals.
    N(u8, bool),
}

impl F {
    fn vt(self) -> ValType {
        match self {
            F::Wd => W,
            F::By => I,
            F::Fl => D,
            F::N(..) => W,
        }
    }
}

struct Lay {
    size: u32,
    align: u32,
    fields: Vec<(u32, F)>,
}

fn lay(t: &LTy) -> Lay {
    let words = |n: u32| Lay { size: 8 * n, align: 8, fields: (0..n).map(|i| (8 * i, F::Wd)).collect() };
    match t {
        LTy::Region => words(1),
        LTy::Task(_) | LTy::Chan(_) | LTy::Lock | LTy::Atomic => words(1),
        LTy::I64 | LTy::Gen(_) => words(1),
        LTy::F64 => Lay { size: 8, align: 8, fields: vec![(0, F::Fl)] },
        LTy::IntK(k) if k.bits() == 64 => words(1),
        LTy::IntK(k) => {
            let n = (k.bits() / 8) as u8;
            Lay { size: n as u32, align: n as u32, fields: vec![(0, F::N(n, k.signed()))] }
        }
        LTy::PInt | LTy::Str => words(2),
        LTy::Arr(_) => words(3),
        LTy::Bool => Lay { size: 1, align: 1, fields: vec![(0, F::By)] },
        LTy::Unit => Lay { size: 1, align: 1, fields: vec![] },
        LTy::Range => Lay { size: 24, align: 8, fields: vec![(0, F::Wd), (8, F::Wd), (16, F::By)] },
        LTy::Tup(ts) => {
            let (mut off, mut align, mut fields) = (0u32, 1u32, vec![]);
            for t in ts {
                let l = lay(t);
                off = off.next_multiple_of(l.align);
                fields.extend(l.fields.iter().map(|(o, f)| (off + o, *f)));
                off += l.size;
                align = align.max(l.align);
            }
            Lay { size: off.next_multiple_of(align).max(1), align, fields }
        }
    }
}

fn vts(t: &LTy) -> Vec<ValType> {
    lay(t).fields.iter().map(|(_, f)| f.vt()).collect()
}

fn elem(t: &LTy) -> &LTy {
    match t {
        LTy::Arr(e) => e,
        _ => panic!("wasmgen: not an array: {t:?}"),
    }
}

fn count_yields(s: &LS) -> usize {
    match s {
        LS::Yield(_) => 1,
        LS::If(_, a, b) => a.iter().map(count_yields).sum::<usize>() + b.iter().map(count_yields).sum::<usize>(),
        LS::Loop(_, b) => b.iter().map(count_yields).sum(),
        _ => 0,
    }
}

/// Generator state: next-function table index, state, then every variable.
fn gen_offsets(f: &LFunc) -> (Vec<u32>, u32) {
    let mut off = 16u32;
    let mut offs = vec![];
    for v in &f.vars {
        let l = lay(&v.ty);
        off = off.next_multiple_of(l.align);
        offs.push(off);
        off += l.size;
    }
    (offs, off.next_multiple_of(8))
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Plain,
    Main,
    Gen,
    Worker,
}

#[derive(Clone, Copy, PartialEq)]
enum Frame {
    Brk(Label),
    Cont(Label),
    Other,
}

/// Module-wide state.
struct Ctx<'p> {
    types: Vec<(Vec<ValType>, Vec<ValType>)>,
    type_idx: HashMap<(Vec<ValType>, Vec<ValType>), u32>,
    rt_idx: HashMap<&'static str, u32>,
    funcs: HashMap<&'p str, (u32, &'p LFunc)>,
    gens: HashMap<usize, (u32, u32, &'p LGen)>, // func index, table index
    workers: HashMap<usize, (u32, &'p LWorker)>,
    consts: HashMap<String, (i64, i64)>,
    ret: i64,
    /// Each global's type and address in the runtime's memory.
    globals: Vec<(LTy, i64)>,
}

impl Ctx<'_> {
    fn ty(&mut self, params: Vec<ValType>, results: Vec<ValType>) -> u32 {
        let key = (params, results);
        if let Some(i) = self.type_idx.get(&key) {
            return *i;
        }
        let i = self.types.len() as u32;
        self.types.push(key.clone());
        self.type_idx.insert(key, i);
        i
    }

    /// A constant byte string in the runtime's memory: (address, length).
    fn bytes(&mut self, s: &str) -> (i64, i64) {
        if let Some(c) = self.consts.get(s) {
            return *c;
        }
        let b: Box<[u8]> = s.as_bytes().to_vec().into_boxed_slice();
        let c = (b.as_ptr() as usize as i64, b.len() as i64);
        rt::st().consts.push(b);
        self.consts.insert(s.to_string(), c);
        c
    }
}

fn user_sig(f: &LFunc) -> (Vec<ValType>, Vec<ValType>) {
    if f.is_main {
        return (vec![], vec![]);
    }
    let params: Vec<ValType> = f.params.iter().flat_map(|v| vts(&f.vars[*v].ty)).collect();
    (params, vts(&f.ret))
}

fn gen_sig(g: &LGen) -> (Vec<ValType>, Vec<ValType>) {
    let mut r = vec![I];
    r.extend(vts(&g.elem));
    (vec![W], r)
}

fn worker_sig(w: &LWorker) -> (Vec<ValType>, Vec<ValType>) {
    let p = w.func.params[0];
    (vts(&w.func.vars[p].ty), vts(&w.func.ret))
}

/// Does the program use tasks or channels? (Checked on the Debug form of the
/// IR: the browser has no threads, so these are rejected up front.)
fn uses_concurrency(p: &LProgram) -> bool {
    let funcs = p.funcs.iter().chain(p.gens.iter().map(|g| &g.func)).chain(p.workers.iter().map(|w| &w.func));
    funcs.into_iter().any(|f| {
        let d = format!("{:?}", f.body);
        ["Spawn {", "Wait {", "ChanSend {", "ChanRecv {", "ChanClose {", "Select {", "ChanNew(", "ChanLen(", "LockNew", "AtomicNew("].iter().any(|k| d.contains(k))
    })
}

/// Does the program call C (`extern def`, `C.errno`, `Str.from_cstr`...)? The
/// browser has no C library.
fn uses_ffi(p: &LProgram) -> bool {
    if !p.externs.is_empty() {
        return true;
    }
    let funcs = p.funcs.iter().chain(p.gens.iter().map(|g| &g.func)).chain(p.workers.iter().map(|w| &w.func));
    funcs.into_iter().any(|f| {
        let d = format!("{:?}", f.body);
        ["Rt(Errno", "Rt(Strerror", "Rt(StrFromCstr", "Rt(StrFromPtr"].iter().any(|k| d.contains(k))
    })
}

pub fn emit(p: &LProgram) -> Result<Vec<u8>, String> {
    if uses_ffi(p) {
        return Err("`extern def` and the C library aren't available in the browser".into());
    }
    if uses_concurrency(p) {
        return Err("spawn, channels, Mutex and Atomic aren't available in the browser yet".into());
    }
    rt::st().consts.clear();
    let mut cx = Ctx { types: vec![], type_idx: HashMap::new(), rt_idx: HashMap::new(), funcs: HashMap::new(), gens: HashMap::new(), workers: HashMap::new(), consts: HashMap::new(), ret: rt::ret_addr(), globals: vec![] };
    for t in &p.globals {
        let b: Box<[u8]> = vec![0u8; lay(t).size.next_multiple_of(8) as usize].into_boxed_slice();
        cx.globals.push((t.clone(), b.as_ptr() as usize as i64));
        rt::st().consts.push(b);
    }
    let mut imports = ImportSection::new();
    imports.import("env", "memory", EntityType::Memory(MemoryType { minimum: 1, maximum: None, memory64: false, shared: false, page_size_log2: None }));
    for (k, (name, sig)) in RT.iter().enumerate() {
        let (a, r) = sig_of(sig);
        let t = cx.ty(a, r);
        imports.import("rt", name, EntityType::Function(t));
        cx.rt_idx.insert(name, k as u32);
    }
    let mut next = RT.len() as u32;
    let mut order: Vec<(u32, (Vec<ValType>, Vec<ValType>))> = vec![];
    for f in &p.funcs {
        if f.external {
            return Err(format!("external function `{}`", f.name));
        }
        cx.funcs.insert(&f.name, (next, f));
        order.push((next, user_sig(f)));
        next += 1;
    }
    for (ti, g) in p.gens.iter().enumerate() {
        cx.gens.insert(g.id, (next, ti as u32, g));
        order.push((next, gen_sig(g)));
        next += 1;
    }
    for w in &p.workers {
        cx.workers.insert(w.id, (next, w));
        order.push((next, worker_sig(w)));
        next += 1;
    }
    let mut fsec = FunctionSection::new();
    for (_, (a, r)) in &order {
        let t = cx.ty(a.clone(), r.clone());
        fsec.function(t);
    }

    let mut code = CodeSection::new();
    let mut jobs: Vec<(&LFunc, Kind, Option<&LGen>, Option<&LWorker>)> = vec![];
    for f in &p.funcs {
        let k = if f.is_main { Kind::Main } else { Kind::Plain };
        jobs.push((f, k, None, None));
    }
    for g in &p.gens {
        jobs.push((&g.func, Kind::Gen, Some(g), None));
    }
    for w in &p.workers {
        jobs.push((&w.func, Kind::Worker, None, Some(w)));
    }
    for (f, kind, g, w) in jobs {
        let func = Fx::new(&mut cx, f, kind, g, w).body();
        code.function(&func);
    }

    let main = p.funcs.iter().find(|f| f.is_main).ok_or("no main")?;
    let mut exports = ExportSection::new();
    exports.export("main", ExportKind::Func, cx.funcs[main.name.as_str()].0);

    let mut tables = TableSection::new();
    tables.table(TableType { element_type: RefType::FUNCREF, table64: false, minimum: p.gens.len() as u64, maximum: Some(p.gens.len() as u64), shared: false });
    let mut elems = ElementSection::new();
    if !p.gens.is_empty() {
        let idx: Vec<u32> = p.gens.iter().map(|g| cx.gens[&g.id].0).collect();
        elems.active(None, &ConstExpr::i32_const(0), Elements::Functions(idx.into()));
    }

    let mut types = TypeSection::new();
    for (a, r) in &cx.types {
        types.ty().function(a.iter().copied(), r.iter().copied());
    }
    let mut m = Module::new();
    m.section(&types).section(&imports).section(&fsec).section(&tables).section(&exports);
    if !p.gens.is_empty() {
        m.section(&elems);
    }
    m.section(&code);
    Ok(m.finish())
}

struct Fx<'c, 'p> {
    cx: &'c mut Ctx<'p>,
    f: &'p LFunc,
    kind: Kind,
    lgen: Option<&'p LGen>,
    nparams: u32,
    locals: Vec<ValType>,
    vars: Vec<Vec<u32>>,
    code: Vec<u8>,
    frames: Vec<Frame>,
    // generators
    g: u32,
    resume: u32,
    offs: Vec<u32>,
    ny: usize,
}

fn mem(off: u32, f: F) -> MemArg {
    let align = match f {
        F::By => 0,
        F::N(n, _) => n.trailing_zeros(),
        _ => 3,
    };
    MemArg { offset: off as u64, align, memory_index: 0 }
}

impl<'c, 'p> Fx<'c, 'p> {
    fn new(cx: &'c mut Ctx<'p>, f: &'p LFunc, kind: Kind, lgen: Option<&'p LGen>, w: Option<&'p LWorker>) -> Self {
        let params: Vec<ValType> = match kind {
            Kind::Gen => vec![W],
            Kind::Worker => worker_sig(w.unwrap()).0,
            _ => user_sig(f).0,
        };
        let mut fx = Fx { cx, f, kind, lgen, nparams: params.len() as u32, locals: vec![], vars: vec![], code: vec![], frames: vec![], g: 0, resume: 0, offs: vec![], ny: 0 };
        // Parameters are the parameter variables' locals, in parameter order.
        let pvars: Vec<V> = if kind == Kind::Gen { vec![] } else { f.params.clone() };
        let mut vars: Vec<Option<Vec<u32>>> = vec![None; f.vars.len()];
        let mut next_param = 0u32;
        for &v in &pvars {
            let n = vts(&f.vars[v].ty).len() as u32;
            vars[v] = Some((next_param..next_param + n).collect());
            next_param += n;
        }
        for (i, v) in f.vars.iter().enumerate() {
            if vars[i].is_none() {
                vars[i] = Some(vts(&v.ty).iter().map(|t| fx.local(*t)).collect());
            }
        }
        fx.vars = vars.into_iter().map(Option::unwrap).collect();
        fx
    }

    fn ins(&mut self) -> InstructionSink<'_> {
        InstructionSink::new(&mut self.code)
    }

    fn local(&mut self, t: ValType) -> u32 {
        self.locals.push(t);
        self.nparams + self.locals.len() as u32 - 1
    }

    // ---------- control ----------

    fn bt(&mut self, results: Vec<ValType>) -> BlockType {
        match results.len() {
            0 => BlockType::Empty,
            1 => BlockType::Result(results[0]),
            _ => BlockType::FunctionType(self.cx.ty(vec![], results)),
        }
    }

    fn if_(&mut self, results: Vec<ValType>) {
        let bt = self.bt(results);
        self.ins().if_(bt);
        self.frames.push(Frame::Other);
    }

    fn else_(&mut self) {
        self.ins().else_();
    }

    fn end(&mut self) {
        self.ins().end();
        self.frames.pop();
    }

    fn depth(&self, fr: Frame) -> u32 {
        let i = self.frames.iter().rposition(|x| *x == fr).expect("wasmgen: label not in scope");
        (self.frames.len() - 1 - i) as u32
    }

    // ---------- values ----------

    fn i64c(&mut self, v: i64) {
        self.ins().i64_const(v);
    }

    fn get(&mut self, ls: &[u32]) {
        for l in ls {
            self.ins().local_get(*l);
        }
    }

    /// Pop values of these types into fresh locals.
    fn pop(&mut self, ts: &[ValType]) -> Vec<u32> {
        let ls: Vec<u32> = ts.iter().map(|t| self.local(*t)).collect();
        for l in ls.iter().rev() {
            self.ins().local_set(*l);
        }
        ls
    }

    fn set_var(&mut self, v: V) {
        for l in self.vars[v].clone().iter().rev() {
            self.ins().local_set(*l);
        }
    }

    fn zeros(&mut self, ts: &[ValType]) {
        for t in ts {
            match *t {
                W => self.ins().i64_const(0),
                D => self.ins().f64_const(0.0.into()),
                _ => self.ins().i32_const(0),
            };
        }
    }

    fn addr(&mut self, base: u32) {
        self.ins().local_get(base).i32_wrap_i64();
    }

    fn load(&mut self, t: &LTy, base: u32, off: u32) {
        for (o, f) in lay(t).fields {
            self.addr(base);
            match f {
                F::Wd => self.ins().i64_load(mem(off + o, f)),
                F::By => self.ins().i32_load8_u(mem(off + o, f)),
                F::Fl => self.ins().f64_load(mem(off + o, f)),
                F::N(1, true) => self.ins().i64_load8_s(mem(off + o, f)),
                F::N(1, false) => self.ins().i64_load8_u(mem(off + o, f)),
                F::N(2, true) => self.ins().i64_load16_s(mem(off + o, f)),
                F::N(2, false) => self.ins().i64_load16_u(mem(off + o, f)),
                F::N(_, true) => self.ins().i64_load32_s(mem(off + o, f)),
                F::N(_, false) => self.ins().i64_load32_u(mem(off + o, f)),
            };
        }
    }

    fn store(&mut self, t: &LTy, base: u32, off: u32, vals: &[u32]) {
        for ((o, f), v) in lay(t).fields.into_iter().zip(vals) {
            self.addr(base);
            self.ins().local_get(*v);
            match f {
                F::Wd => self.ins().i64_store(mem(off + o, f)),
                F::By => self.ins().i32_store8(mem(off + o, f)),
                F::Fl => self.ins().f64_store(mem(off + o, f)),
                F::N(1, _) => self.ins().i64_store8(mem(off + o, f)),
                F::N(2, _) => self.ins().i64_store16(mem(off + o, f)),
                F::N(..) => self.ins().i64_store32(mem(off + o, f)),
            };
        }
    }

    /// Push RET[0..n] (words).
    fn ret_words(&mut self, n: u32) {
        let r = self.cx.ret as i32;
        for k in 0..n {
            self.ins().i32_const(r).i64_load(mem(8 * k, F::Wd));
        }
    }

    fn rt(&mut self, name: &str) {
        let i = self.cx.rt_idx[name];
        self.ins().call(i);
    }

    fn str_const(&mut self, s: &str) {
        let (p, n) = self.cx.bytes(s);
        self.i64c(p);
        self.i64c(n);
    }

    fn panic(&mut self, msg: &str, loc: &str) {
        self.str_const(msg);
        self.str_const(loc);
        self.rt("alxr_panic");
        self.ins().unreachable();
    }

    fn overflow(&mut self, loc: &str) {
        self.str_const(loc);
        self.rt("alxr_overflow");
        self.ins().unreachable();
    }

    /// Result types of this function (a generator: its element type).
    fn ret_types(&self) -> Vec<ValType> {
        match self.kind {
            Kind::Gen => vts(&self.lgen.unwrap().elem),
            _ => vts(&self.f.ret),
        }
    }

    // ---------- functions ----------

    fn body(mut self) -> Function {
        let f = self.f;
        match self.kind {
            Kind::Gen => {
                self.g = 0;
                self.resume = self.local(I);
                let (offs, _) = gen_offsets(f);
                self.offs = offs.clone();
                for (i, v) in f.vars.iter().enumerate() {
                    let g = self.g;
                    self.load(&v.ty, g, offs[i]);
                    self.set_var(i);
                }
                let st = self.local(W);
                self.addr(0);
                self.ins().i64_load(mem(8, F::Wd)).local_tee(st).i64_const(-1).i64_eq();
                self.if_(vec![]);
                self.ins().i32_const(0);
                let ts = self.ret_types();
                self.zeros(&ts);
                self.ins().return_();
                self.end();
                let r = self.resume;
                self.ins().local_get(st).i32_wrap_i64().local_set(r);
            }
            Kind::Worker => {}
            _ => {}
        }
        self.block(&f.body);
        // Falling off the end.
        match self.kind {
            Kind::Main => {}
            Kind::Plain if f.ret == LTy::Unit => {}
            Kind::Worker => {
                let ts = self.ret_types();
                self.zeros(&ts);
            }
            Kind::Gen => self.gen_finish(false),
            _ => self.panic("function ended without a value", &f.name.clone()),
        }
        self.ins().end();
        let mut func = Function::new_with_locals_types(self.locals.iter().copied());
        func.raw(self.code.iter().copied());
        func
    }

    /// state = -1; (0, zeros), returned or left as the function's result.
    fn gen_finish(&mut self, ret: bool) {
        self.addr(self.g);
        self.ins().i64_const(-1).i64_store(mem(8, F::Wd)).i32_const(0);
        let ts = self.ret_types();
        self.zeros(&ts);
        if ret {
            self.ins().return_();
        }
    }

    fn block(&mut self, ss: &[LS]) {
        if self.kind != Kind::Gen {
            for s in ss {
                self.stmt(s);
            }
            return;
        }
        let counts: Vec<usize> = ss.iter().map(count_yields).collect();
        let last_y = counts.iter().rposition(|c| *c > 0);
        for (i, s) in ss.iter().enumerate() {
            let c = counts[i];
            if c == 0 {
                if last_y.is_some_and(|ly| i < ly) {
                    let r = self.resume;
                    self.ins().local_get(r).i32_eqz();
                    self.if_(vec![]);
                    self.stmt(s);
                    self.end();
                } else {
                    self.stmt(s);
                }
            } else {
                let (lo, hi) = (self.ny as i32 + 1, (self.ny + c) as i32);
                let r = self.resume;
                // resume == 0 || lo <= resume <= hi
                self.ins().local_get(r).i32_eqz().local_get(r).i32_const(lo).i32_ge_s().local_get(r).i32_const(hi).i32_le_s().i32_and().i32_or();
                self.if_(vec![]);
                self.yielding(s);
                self.end();
            }
        }
    }

    /// A statement containing yields, in a generator.
    fn yielding(&mut self, s: &LS) {
        let r = self.resume;
        match s {
            LS::Yield(e) => {
                self.ny += 1;
                let k = self.ny as i64;
                self.ins().local_get(r).i32_eqz();
                self.if_(vec![]);
                let et = self.lgen.unwrap().elem.clone();
                self.e(e);
                let x = self.pop(&vts(&et));
                for (i, v) in self.f.vars.iter().enumerate() {
                    let ls = self.vars[i].clone();
                    let (g, off) = (self.g, self.offs[i]);
                    self.store(&v.ty, g, off, &ls);
                }
                self.addr(self.g);
                self.ins().i64_const(k).i64_store(mem(8, F::Wd)).i32_const(1);
                self.get(&x);
                self.ins().return_();
                self.else_();
                self.ins().i32_const(0).local_set(r);
                self.end();
            }
            LS::If(c, a, b) => {
                let ca: usize = a.iter().map(count_yields).sum();
                self.ins().local_get(r).i32_eqz();
                self.if_(vec![I]);
                self.e(c);
                self.else_();
                if ca == 0 {
                    self.ins().i32_const(0);
                } else {
                    let (lo, hi) = (self.ny as i32 + 1, (self.ny + ca) as i32);
                    self.ins().local_get(r).i32_const(lo).i32_ge_s().local_get(r).i32_const(hi).i32_le_s().i32_and();
                }
                self.end();
                self.if_(vec![]);
                self.block(a);
                self.else_();
                self.block(b);
                self.end();
            }
            LS::Loop(..) => self.stmt(s),
            _ => unreachable!(),
        }
    }

    // ---------- statements ----------

    fn stmt(&mut self, s: &LS) {
        match s {
            LS::SetGlobal(k, v) => {
                let (t, at) = self.cx.globals[*k].clone();
                let ts = self.e(v);
                let vals = self.pop(&ts);
                let base = self.local(W);
                self.i64c(at);
                self.ins().local_set(base);
                self.store(&t, base, 0, &vals);
            }
            LS::RegionFree(_) => {}
            LS::RegionEnter { .. } | LS::RegionExit { .. } | LS::RegionUse { .. } | LS::RegionRestore(_) => {}
            LS::Spawn { .. } | LS::Wait { .. } | LS::ChanSend { .. } | LS::ChanRecv { .. } | LS::ChanClose { .. } | LS::Select { .. } | LS::Lock(_) | LS::Unlock(_) | LS::AtomicStore(..) => unreachable!("rejected by uses_concurrency"),
            LS::Set(v, e) => {
                self.e(e);
                self.set_var(*v);
            }
            LS::SetIndex { arr, idx, val, check } => {
                let et = elem(&self.f.vars[*arr].ty).clone();
                self.e(idx);
                let i = self.pop(&[W])[0];
                self.e(val);
                let x = self.pop(&vts(&et));
                let a = self.vars[*arr].clone();
                let addr = self.elem_addr(a[0], a[1], i, &et, check.as_deref());
                self.store(&et, addr, 0, &x);
            }
            LS::SetPlace { var, steps, val } => {
                let x = self.eval_locals(val);
                // Through the variable's own locals until the first index,
                // then through memory.
                let mut ty = self.f.vars[*var].ty.clone();
                let mut start = 0usize;
                let mut mem: Option<(u32, u32)> = None;
                for st in steps {
                    match st {
                        Step::Field(k) => {
                            let LTy::Tup(ts) = ty.clone() else { panic!("wasmgen: field of {ty:?}") };
                            match &mut mem {
                                None => start += ts[..*k].iter().map(|t| vts(t).len()).sum::<usize>(),
                                Some((_, off)) => *off += field_offset(&ty, *k),
                            }
                            ty = ts[*k].clone();
                        }
                        Step::Index(i, check) => {
                            let a = match mem {
                                None => self.vars[*var][start..start + 3].to_vec(),
                                Some((addr, off)) => {
                                    self.load(&ty, addr, off);
                                    self.pop(&[W, W, W])
                                }
                            };
                            let iv = self.eval_locals(i)[0];
                            let et = elem(&ty).clone();
                            let addr = self.elem_addr(a[0], a[1], iv, &et, check.as_deref());
                            mem = Some((addr, 0));
                            ty = et;
                        }
                    }
                }
                match mem {
                    None => {
                        let dst = self.vars[*var][start..start + x.len()].to_vec();
                        for (d, s) in dst.iter().zip(&x) {
                            self.ins().local_get(*s).local_set(*d);
                        }
                    }
                    Some((addr, off)) => self.store(&ty, addr, off, &x),
                }
            }
            LS::Push(v, e) => {
                let et = elem(&self.f.vars[*v].ty).clone();
                let esz = lay(&et).size as i64;
                self.e(e);
                let x = self.pop(&vts(&et));
                let a = self.vars[*v].clone();
                let (p, len, cap) = (a[0], a[1], a[2]);
                self.ins().local_get(len).local_get(cap).i64_eq();
                self.if_(vec![]);
                let nc = self.local(W);
                self.ins().i64_const(4).local_get(cap).i64_const(1).i64_shl().local_get(cap).i64_eqz().select().local_set(nc);
                self.ins().local_get(p).local_get(len).local_get(nc).i64_const(esz);
                self.rt("alxr_grow");
                self.ins().local_set(p).local_get(nc).local_set(cap);
                self.end();
                let addr = self.local(W);
                self.ins().local_get(p).local_get(len).i64_const(esz).i64_mul().i64_add().local_set(addr);
                self.store(&et, addr, 0, &x);
                self.ins().local_get(len).i64_const(1).i64_add().local_set(len);
            }
            LS::Eval(e) => {
                let n = self.e(e).len();
                for _ in 0..n {
                    self.ins().drop();
                }
            }
            LS::If(c, a, b) => {
                self.e(c);
                self.if_(vec![]);
                self.block(a);
                if !b.is_empty() {
                    self.else_();
                    self.block(b);
                }
                self.end();
            }
            LS::Loop(l, body) => {
                self.ins().block(BlockType::Empty);
                self.frames.push(Frame::Brk(*l));
                self.ins().loop_(BlockType::Empty);
                self.frames.push(Frame::Cont(*l));
                self.block(body);
                self.ins().br(0);
                self.end();
                self.end();
            }
            LS::Break(l) => {
                let d = self.depth(Frame::Brk(*l));
                self.ins().br(d);
            }
            LS::Continue(l) => {
                let d = self.depth(Frame::Cont(*l));
                self.ins().br(d);
            }
            LS::Return(v) => match self.kind {
                Kind::Plain | Kind::Worker => {
                    if let Some(v) = v {
                        self.e(v);
                    }
                    self.ins().return_();
                }
                Kind::Gen => self.gen_finish(true),
                Kind::Main => {
                    self.ins().return_();
                }
            },
            LS::NextOrBreak { source, dst, label } => {
                self.e(source);
                let g = self.pop(&[W])[0];
                let dt = self.f.vars[*dst].ty.clone();
                let mut r = vec![I];
                r.extend(vts(&dt));
                let t = self.cx.ty(vec![W], r);
                self.ins().local_get(g);
                self.addr(g);
                self.ins().i64_load(mem(0, F::Wd)).i32_wrap_i64().call_indirect(0, t);
                let vals = self.pop(&vts(&dt));
                let ok = self.pop(&[I])[0];
                let d = self.depth(Frame::Brk(*label));
                self.ins().local_get(ok).i32_eqz().br_if(d);
                self.get(&vals);
                self.set_var(*dst);
            }
            LS::Yield(_) => unreachable!("wasmgen: yield outside a generator"),
            LS::Pmap { dst, arr, worker } => {
                // Sequential in the browser (no threads without cross-origin isolation).
                let (widx, w) = self.cx.workers[worker];
                let it = self.f.vars[*dst].ty.clone();
                let in_t = w.func.vars[w.func.params[0]].ty.clone();
                let out_t = elem(&it).clone();
                let (iesz, oesz) = (lay(&in_t).size as i64, lay(&out_t).size as i64);
                self.e(arr);
                let a = self.pop(&[W, W, W]);
                let out = self.local(W);
                self.ins().local_get(a[1]).i64_const(oesz).i64_mul();
                self.rt("alxr_alloc");
                self.ins().local_set(out);
                let i = self.local(W);
                let addr = self.local(W);
                self.ins().i64_const(0).local_set(i).block(BlockType::Empty).loop_(BlockType::Empty);
                self.frames.push(Frame::Other);
                self.frames.push(Frame::Other);
                self.ins().local_get(i).local_get(a[1]).i64_ge_s().br_if(1);
                self.ins().local_get(a[0]).local_get(i).i64_const(iesz).i64_mul().i64_add().local_set(addr);
                self.load(&in_t, addr, 0);
                self.ins().call(widx);
                let vals = self.pop(&vts(&out_t));
                self.ins().local_get(out).local_get(i).i64_const(oesz).i64_mul().i64_add().local_set(addr);
                self.store(&out_t, addr, 0, &vals);
                self.ins().local_get(i).i64_const(1).i64_add().local_set(i).br(0);
                self.end();
                self.end();
                self.ins().local_get(out).local_get(a[1]).local_get(a[1]);
                self.set_var(*dst);
            }
            LS::Print(e) => {
                self.e(e);
                self.rt("alxr_print_str");
            }
            LS::Puts(e, t) => {
                self.e(e);
                match t {
                    LTy::I64 => self.rt("alxr_puts_i64"),
                    LTy::F64 => self.rt("alxr_puts_f64"),
                    LTy::IntK(IntKind::U64) => self.rt("alxr_puts_u64"),
                    LTy::IntK(_) => self.rt("alxr_puts_i64"),
                    LTy::Str => self.rt("alxr_puts_str"),
                    LTy::Bool => self.rt("alxr_puts_bool"),
                    LTy::PInt => self.rt("alxr_puts_pint"),
                    LTy::Unit => self.rt("alxr_puts_unit"),
                    _ => {
                        for _ in 0..vts(t).len() {
                            self.ins().drop();
                        }
                        self.panic("`puts` of this type isn't supported yet", "puts");
                    }
                }
            }
            LS::Die(e) => {
                self.e(e);
                self.rt("alxr_die_str");
                self.ins().unreachable();
            }
            LS::Panic(msg, loc) => self.panic(msg, loc),
            LS::Exit(e) => {
                self.e(e);
                self.ins().drop();
                self.str_const("");
                self.rt("alxr_die_str");
                self.ins().unreachable();
            }
            LS::PanicStr(e) => {
                self.e(e);
                self.rt("alxr_die_str");
                self.ins().unreachable();
            }
            LS::SortInPlace(v, el) => {
                let a = self.vars[*v].clone();
                self.ins().local_get(a[0]).local_get(a[1]);
                match el {
                    LTy::I64 => self.rt("alxr_sort_i64"),
                    LTy::Str => self.rt("alxr_sort_str"),
                    _ => {
                        self.ins().drop().drop();
                        self.panic("`sort` of this element type isn't supported yet", "sort");
                    }
                }
            }
        }
    }

    // ---------- arithmetic ----------

    fn add_sub(&mut self, op: Op, a: u32, b: u32) -> u32 {
        let r = self.local(W);
        self.ins().local_get(a).local_get(b);
        if op == Op::Add {
            self.ins().i64_add();
        } else {
            self.ins().i64_sub();
        }
        self.ins().local_set(r);
        r
    }

    /// Push the signed-overflow flag (i32) of r = a op b.
    fn ovf_test(&mut self, op: Op, a: u32, b: u32, r: u32) {
        if op == Op::Add {
            // ((a ^ r) & (b ^ r)) < 0
            self.ins().local_get(a).local_get(r).i64_xor().local_get(b).local_get(r).i64_xor().i64_and().i64_const(0).i64_lt_s();
        } else {
            // ((a ^ b) & (a ^ r)) < 0
            self.ins().local_get(a).local_get(b).i64_xor().local_get(a).local_get(r).i64_xor().i64_and().i64_const(0).i64_lt_s();
        }
    }

    /// Push a * b; `on_ovf` runs with the overflow flag on the stack.
    fn mul_checked(&mut self, a: u32, b: u32, on_ovf: impl FnOnce(&mut Self)) {
        // Both fit in 32 bits: the product can't overflow.
        self.ins().local_get(a).local_get(a).i64_extend32_s().i64_eq().local_get(b).local_get(b).i64_extend32_s().i64_eq().i32_and();
        self.if_(vec![W]);
        self.ins().local_get(a).local_get(b).i64_mul();
        self.else_();
        let r = self.local(W);
        self.ins().local_get(a).local_get(b);
        self.rt("alxr_mul_chk");
        self.ins().local_set(r);
        self.ret_words(1);
        self.ins().i32_wrap_i64();
        on_ovf(self);
        self.ins().local_get(r);
        self.end();
    }

    /// Push truncating a / b or a % b (Go); b != 0, and not (Div) MIN / -1.
    fn floor_divrem(&mut self, op: Op, a: u32, b: u32) {
        let bs = self.local(W);
        // b == -1 → divide by 1 (avoids the MIN % -1 trap), then fix up.
        self.ins().i64_const(1).local_get(b).local_get(b).i64_const(-1).i64_eq().select().local_set(bs);
        if op == Op::Div {
            self.ins().i64_const(0).local_get(a).i64_sub();
            self.ins().local_get(a).local_get(bs).i64_div_s();
            self.ins().local_get(b).i64_const(-1).i64_eq().select();
        } else {
            self.ins().i64_const(0);
            self.ins().local_get(a).local_get(bs).i64_rem_s();
            self.ins().local_get(b).i64_const(-1).i64_eq().select();
        }
    }

    fn arith(&mut self, op: Op, a: &LE, b: &LE, ovf: &Ovf) {
        let konst = if let LE::I(k) = b { Some(*k) } else { None };
        let loc = match ovf {
            Ovf::Panic(l) => l.clone(),
            Ovf::Wrap => "wrap".into(),
            Ovf::Unchecked => "proven".into(),
        };
        if matches!(op, Op::Div | Op::Rem) {
            if let Some(k) = konst.filter(|k| *k > 1 && (*k as u64).is_power_of_two()) {
                // Truncating division by 2^s: bias negatives by 2^s - 1, then shift.
                let sh = k.trailing_zeros() as i64;
                self.e(a);
                let x = self.pop(&[W])[0];
                let q = self.local(W);
                self.ins().local_get(x).local_get(x).i64_const(63).i64_shr_s().i64_const(64 - sh).i64_shr_u().i64_add().i64_const(sh).i64_shr_s().local_set(q);
                if op == Op::Div {
                    self.ins().local_get(q);
                } else {
                    self.ins().local_get(x).local_get(q).i64_const(sh).i64_shl().i64_sub();
                }
                return;
            }
        }
        self.e(a);
        let ta = self.pop(&[W])[0];
        self.e(b);
        let tb = self.pop(&[W])[0];
        match op {
            Op::Add | Op::Sub => {
                if let Ovf::Panic(loc) = ovf {
                    let r = self.add_sub(op, ta, tb);
                    self.ovf_test(op, ta, tb, r);
                    self.if_(vec![]);
                    self.overflow(loc);
                    self.end();
                    self.ins().local_get(r);
                } else {
                    self.ins().local_get(ta).local_get(tb);
                    if op == Op::Add {
                        self.ins().i64_add();
                    } else {
                        self.ins().i64_sub();
                    }
                }
            }
            Op::Mul => {
                if let Ovf::Panic(loc) = ovf {
                    self.mul_checked(ta, tb, |s| {
                        s.if_(vec![]);
                        s.overflow(loc);
                        s.end();
                    });
                } else {
                    self.ins().local_get(ta).local_get(tb).i64_mul();
                }
            }
            Op::Div | Op::Rem => {
                if konst.is_none_or(|k| k == 0 || k == -1) {
                    self.ins().local_get(tb).i64_eqz();
                    self.if_(vec![]);
                    self.panic("division by zero", &loc);
                    self.end();
                    if op == Op::Div {
                        self.ins().local_get(ta).i64_const(i64::MIN).i64_eq().local_get(tb).i64_const(-1).i64_eq().i32_and();
                        self.if_(vec![]);
                        self.overflow(&loc);
                        self.end();
                    }
                }
                self.floor_divrem(op, ta, tb);
            }
            Op::Pow => {
                self.ins().local_get(ta).local_get(tb);
                self.str_const(&loc);
                self.rt("alxr_pow");
            }
            _ => unreachable!(),
        }
    }

    fn elem_addr(&mut self, p: u32, len: u32, i: u32, et: &LTy, check: Option<&str>) -> u32 {
        if let Some(loc) = check {
            self.ins().local_get(i).local_get(len).i64_ge_u();
            self.if_(vec![]);
            self.panic("index out of bounds", loc);
            self.end();
        }
        let addr = self.local(W);
        self.ins().local_get(p).local_get(i).i64_const(lay(et).size as i64).i64_mul().i64_add().local_set(addr);
        addr
    }

    // ---------- expressions ----------

    fn ty(&self, e: &LE) -> LTy {
        match e {
            LE::Ffi(..) => unreachable!("rejected by uses_ffi"),
            LE::Global(k) => self.cx.globals[*k].0.clone(),
            LE::LockNew => LTy::Lock,
            LE::AtomicNew(_) => LTy::Atomic,
            LE::AtomicLoad(_) | LE::AtomicRmw(..) => LTy::I64,
            LE::AtomicCas(..) => LTy::Bool,
            LE::RegionNew(_) => LTy::Region,
            LE::RegionBytes(_) => LTy::I64,
            LE::RegionOf(_) => LTy::Region,
            LE::RegionProgram => LTy::Region,
            LE::ChanNew(t, _) => LTy::Chan(Box::new(t.clone())),
            LE::ChanLen(_) => LTy::I64,
            LE::Var(v) => self.f.vars[*v].ty.clone(),
            LE::I(_) | LE::Arith(..) | LE::Neg(..) | LE::Len(_) => LTy::I64,
            LE::F(_) | LE::FArith(..) | LE::FNeg(_) | LE::Prim(Prim::UToF, _) => LTy::F64,
            LE::Prim(Prim::ULt | Prim::ULe, _) => LTy::Bool,
            LE::Prim(..) => LTy::I64,
            LE::Loc(_) | LE::S(_) => LTy::Str,
            LE::B(_) | LE::Cmp(..) | LE::Not(_) => LTy::Bool,
            LE::Unit => LTy::Unit,
            LE::Tup(t, _) => t.clone(),
            LE::Field(x, i) => match self.ty(x) {
                LTy::Tup(ts) => ts[*i].clone(),
                t => panic!("wasmgen: field of {t:?}"),
            },
            LE::PArith(op, ..) => match op {
                Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge => LTy::Bool,
                _ => LTy::PInt,
            },
            LE::Cond(_, a, _) => self.ty(a),
            LE::Call(f, _) => self.cx.funcs[f.as_str()].1.ret.clone(),
            LE::Rt(r, args) => match r {
                Rt::IntToS | Rt::PIntToS | Rt::StrRev | Rt::StrDelete | Rt::FToS | Rt::FFmt | Rt::FFmtE | Rt::StrPad | Rt::StrQuote | Rt::StrCat | Rt::U64ToS | Rt::IntFmt | Rt::RuneToS | Rt::StrFromBytes | Rt::FileRead | Rt::CapEnd => LTy::Str,
                Rt::FileStatus | Rt::NowNs | Rt::CapBegin => LTy::I64,
                Rt::IntToF | Rt::FSqrt | Rt::FAbs | Rt::Math(_) | Rt::FFromBits => LTy::F64,
                Rt::FBits => LTy::IntK(IntKind::U64),
                Rt::StrByte => {
                    if matches!(args[2], LE::I(0)) {
                        LTy::I64
                    } else {
                        LTy::Str
                    }
                }
                Rt::StrSplit => LTy::Arr(Box::new(LTy::Str)),
                Rt::Digits => LTy::Arr(Box::new(LTy::I64)),
                Rt::PDigits => LTy::Arr(Box::new(LTy::PInt)),
                Rt::ArrCopy => self.ty(&args[0]),
                Rt::Even | Rt::PEven => LTy::Bool,
                _ => LTy::I64,
            },
            LE::Index { arr, .. } => elem(&self.ty(arr)).clone(),
            LE::ArrLit(t, _) | LE::ArrNew(t, ..) | LE::ArrWithCap(t, _) => LTy::Arr(Box::new(t.clone())),
            LE::Slice(t, ..) => t.clone(),
            LE::Range(..) => LTy::Range,
            LE::RangeField(_, k) => {
                if *k == 2 {
                    LTy::Bool
                } else {
                    LTy::I64
                }
            }
            LE::GenNew(id, _) => LTy::Gen(Box::new(self.cx.gens[id].2.elem.clone())),
            LE::ToP(_) => LTy::PInt,
        }
    }

    /// Evaluate onto the stack; returns the pushed value types.
    fn e(&mut self, e: &LE) -> Vec<ValType> {
        let t = self.ty(e);
        let out = vts(&t);
        match e {
            LE::RegionNew(_) => self.i64c(0),
            LE::RegionBytes(_) => self.i64c(0),
            LE::RegionOf(_) => self.i64c(0),
            // The browser never frees: regions are no-ops (handle 0).
            LE::RegionProgram => self.i64c(0),
            LE::Ffi(..) => unreachable!("rejected by uses_ffi"),
            LE::Global(k) => {
                let (t, at) = self.cx.globals[*k].clone();
                let base = self.local(W);
                self.i64c(at);
                self.ins().local_set(base);
                self.load(&t, base, 0);
            }
            LE::ChanNew(..) | LE::ChanLen(_) | LE::LockNew | LE::AtomicNew(_) | LE::AtomicLoad(_) | LE::AtomicRmw(..) | LE::AtomicCas(..) => unreachable!("rejected by uses_concurrency"),
            LE::Var(v) => {
                let ls = self.vars[*v].clone();
                self.get(&ls);
            }
            LE::I(i) => self.i64c(*i),
            LE::F(v) => {
                self.ins().f64_const((*v).into());
            }
            LE::FArith(op, a, b) => {
                self.e(a);
                self.e(b);
                match op {
                    Op::Add => self.ins().f64_add(),
                    Op::Sub => self.ins().f64_sub(),
                    Op::Mul => self.ins().f64_mul(),
                    _ => self.ins().f64_div(),
                };
            }
            LE::FNeg(x) => {
                self.e(x);
                self.ins().f64_neg();
            }
            LE::Prim(p, args) => {
                for a in args {
                    self.e(a);
                }
                match p {
                    Prim::And => {
                        self.ins().i64_and();
                    }
                    Prim::Or => {
                        self.ins().i64_or();
                    }
                    Prim::Xor => {
                        self.ins().i64_xor();
                    }
                    Prim::AndNot => {
                        self.ins().i64_const(-1).i64_xor().i64_and();
                    }
                    Prim::Not => {
                        self.ins().i64_const(-1).i64_xor();
                    }
                    Prim::Shl => {
                        self.ins().i64_shl();
                    }
                    Prim::ShrS => {
                        self.ins().i64_shr_s();
                    }
                    Prim::ShrU => {
                        self.ins().i64_shr_u();
                    }
                    Prim::ULt => {
                        self.ins().i64_lt_u();
                    }
                    Prim::ULe => {
                        self.ins().i64_le_u();
                    }
                    Prim::UDiv => {
                        self.ins().i64_div_u();
                    }
                    Prim::URem => {
                        self.ins().i64_rem_u();
                    }
                    Prim::UMulHi => self.rt("alxr_umulhi"),
                    Prim::UToF => {
                        self.ins().f64_convert_i64_u();
                    }
                    Prim::Wrap(k) => {
                        match (k.signed(), k.bits()) {
                            (true, 8) => self.ins().i64_extend8_s(),
                            (true, 16) => self.ins().i64_extend16_s(),
                            (true, _) => self.ins().i64_extend32_s(),
                            (false, b) => self.ins().i64_const(((1u64 << b) - 1) as i64).i64_and(),
                        };
                    }
                }
            }
            LE::B(b) => {
                self.ins().i32_const(*b as i32);
            }
            LE::S(s) | LE::Loc(s) => self.str_const(s),
            LE::Unit => {}
            LE::Tup(_, vs) => {
                for v in vs {
                    self.e(v);
                }
            }
            LE::Field(x, i) if matches!(**x, LE::Index { .. }) => {
                // An element's field: load just that field.
                let LE::Index { arr, idx, check } = &**x else { unreachable!() };
                let et = elem(&self.ty(arr)).clone();
                let a = self.eval_locals(arr);
                let iv = self.eval_locals(idx)[0];
                let addr = self.elem_addr(a[0], a[1], iv, &et, check.as_deref());
                let LTy::Tup(ts) = &et else { unreachable!() };
                let off = field_offset(&et, *i);
                self.load(&ts[*i].clone(), addr, off);
            }
            LE::Field(x, i) => {
                let LTy::Tup(ts) = self.ty(x) else { unreachable!() };
                let ls = self.eval_locals(x);
                let start: usize = ts[..*i].iter().map(|t| vts(t).len()).sum();
                let n = vts(&ts[*i]).len();
                self.get(&ls[start..start + n].to_vec());
            }
            LE::Arith(op, a, b, ovf) => self.arith(*op, a, b, ovf),
            LE::PArith(op, a, b) => {
                self.e(a);
                self.e(b);
                let name = match op {
                    Op::Add => "alxr_p_add",
                    Op::Sub => "alxr_p_sub",
                    Op::Mul => "alxr_p_mul",
                    Op::Div => "alxr_p_div",
                    Op::Rem => "alxr_p_rem",
                    Op::Pow => "alxr_p_pow",
                    _ => {
                        self.rt("alxr_p_cmp");
                        self.cmp0(*op);
                        return out;
                    }
                };
                match op {
                    Op::Div => self.str_const("div"),
                    Op::Rem => self.str_const("rem"),
                    Op::Pow => self.str_const("pow"),
                    _ => {}
                }
                self.rt(name);
                self.ret_words(2);
            }
            LE::Cmp(op, a, b, t) => match (op, t) {
                (Op::And, _) => {
                    self.e(a);
                    self.if_(vec![I]);
                    self.e(b);
                    self.else_();
                    self.ins().i32_const(0);
                    self.end();
                }
                (Op::Or, _) => {
                    self.e(a);
                    self.if_(vec![I]);
                    self.ins().i32_const(1);
                    self.else_();
                    self.e(b);
                    self.end();
                }
                (_, LTy::Str) => {
                    if let (Op::Eq | Op::Ne, Some(x)) = (op, alx::cgen::palindrome_test(a, b)) {
                        self.e(x);
                        self.rt("alxr_str_is_pal");
                        if *op == Op::Ne {
                            self.ins().i32_eqz();
                        }
                        return out;
                    }
                    self.e(a);
                    self.e(b);
                    match op {
                        Op::Eq => self.rt("alxr_str_eq"),
                        Op::Ne => {
                            self.rt("alxr_str_eq");
                            self.ins().i32_eqz();
                        }
                        _ => {
                            self.rt("alxr_str_cmp");
                            self.cmp0(*op);
                        }
                    }
                }
                (_, LTy::PInt) => {
                    self.e(a);
                    self.e(b);
                    self.rt("alxr_p_cmp");
                    self.cmp0(*op);
                }
                (_, LTy::F64) => {
                    self.e(a);
                    self.e(b);
                    match op {
                        Op::Eq => self.ins().f64_eq(),
                        Op::Ne => self.ins().f64_ne(),
                        Op::Lt => self.ins().f64_lt(),
                        Op::Le => self.ins().f64_le(),
                        Op::Gt => self.ins().f64_gt(),
                        _ => self.ins().f64_ge(),
                    };
                }
                (_, LTy::Bool) => {
                    self.e(a);
                    self.e(b);
                    match op {
                        Op::Eq => self.ins().i32_eq(),
                        Op::Ne => self.ins().i32_ne(),
                        Op::Lt => self.ins().i32_lt_u(),
                        Op::Le => self.ins().i32_le_u(),
                        Op::Gt => self.ins().i32_gt_u(),
                        _ => self.ins().i32_ge_u(),
                    };
                }
                _ => {
                    self.e(a);
                    self.e(b);
                    match op {
                        Op::Eq => self.ins().i64_eq(),
                        Op::Ne => self.ins().i64_ne(),
                        Op::Lt => self.ins().i64_lt_s(),
                        Op::Le => self.ins().i64_le_s(),
                        Op::Gt => self.ins().i64_gt_s(),
                        _ => self.ins().i64_ge_s(),
                    };
                }
            },
            LE::Neg(x, ovf) => {
                self.e(x);
                let t = self.pop(&[W])[0];
                if let Ovf::Panic(loc) = ovf {
                    self.ins().local_get(t).i64_const(i64::MIN).i64_eq();
                    self.if_(vec![]);
                    self.overflow(loc);
                    self.end();
                }
                self.ins().i64_const(0).local_get(t).i64_sub();
            }
            LE::Not(x) => {
                self.e(x);
                self.ins().i32_eqz();
            }
            LE::Cond(c, a, b) => {
                self.e(c);
                self.if_(out.clone());
                self.e(a);
                self.else_();
                self.e(b);
                self.end();
            }
            LE::Call(f, args) => {
                let idx = self.cx.funcs[f.as_str()].0;
                for a in args {
                    self.e(a);
                }
                self.ins().call(idx);
            }
            LE::Rt(r, args) => self.rt_expr(*r, args),
            LE::Index { arr, idx, check } => {
                let et = elem(&self.ty(arr)).clone();
                let a = self.eval_locals(arr);
                let i = self.eval_locals(idx)[0];
                let addr = self.elem_addr(a[0], a[1], i, &et, check.as_deref());
                self.load(&et, addr, 0);
            }
            LE::Len(x) => {
                let ls = self.eval_locals(x);
                self.ins().local_get(ls[1]);
            }
            LE::ArrLit(t, vs) => {
                let esz = lay(t).size as i64;
                let p = self.local(W);
                self.ins().i64_const(esz * vs.len() as i64);
                self.rt("alxr_alloc");
                self.ins().local_set(p);
                for (k, v) in vs.iter().enumerate() {
                    let x = self.eval_locals(v);
                    self.store(t, p, (k as i64 * esz) as u32, &x);
                }
                self.ins().local_get(p).i64_const(vs.len() as i64).i64_const(vs.len() as i64);
            }
            LE::ArrNew(t, n, fill, loc) => {
                let l = lay(t);
                let esz = l.size as i64;
                let nv = self.eval_locals(n)[0];
                self.ins().local_get(nv).i64_const(0).i64_lt_s();
                self.if_(vec![]);
                self.panic("negative array size", loc);
                self.end();
                let x = self.eval_locals(fill);
                let p = self.local(W);
                self.ins().local_get(nv).i64_const(esz).i64_mul();
                self.rt("alxr_alloc");
                self.ins().local_set(p);
                if esz == 1 && l.fields.len() == 1 {
                    self.addr(p);
                    self.ins().local_get(x[0]);
                    if matches!(l.fields[0].1, F::N(..)) {
                        self.ins().i32_wrap_i64();
                    }
                    self.ins().local_get(nv).i32_wrap_i64().memory_fill(0);
                } else {
                    let (i, addr) = (self.local(W), self.local(W));
                    self.ins().i64_const(0).local_set(i).block(BlockType::Empty).loop_(BlockType::Empty);
                    self.frames.push(Frame::Other);
                    self.frames.push(Frame::Other);
                    self.ins().local_get(i).local_get(nv).i64_ge_s().br_if(1);
                    self.ins().local_get(p).local_get(i).i64_const(esz).i64_mul().i64_add().local_set(addr);
                    self.store(t, addr, 0, &x);
                    self.ins().local_get(i).i64_const(1).i64_add().local_set(i).br(0);
                    self.end();
                    self.end();
                }
                self.ins().local_get(p).local_get(nv).local_get(nv);
            }
            LE::ArrWithCap(t, n) => {
                let esz = lay(t).size as i64;
                let nv = self.eval_locals(n)[0];
                let cap = self.local(W);
                self.ins().i64_const(0).local_get(nv).local_get(nv).i64_const(0).i64_lt_s().select().local_set(cap);
                self.ins().local_get(cap).i64_const(esz).i64_mul();
                self.rt("alxr_alloc");
                self.ins().i64_const(0).local_get(cap);
            }
            LE::Slice(t, a, start, len) => {
                let esz = lay(elem(t)).size as i64;
                let av = self.eval_locals(a);
                let s = self.eval_locals(start)[0];
                self.ins().local_get(av[0]).local_get(s).i64_const(esz).i64_mul().i64_add();
                self.e(len);
                self.ins().i64_const(0);
            }
            LE::Range(lo, hi, ex) => {
                self.e(lo);
                self.e(hi);
                self.ins().i32_const(*ex as i32);
            }
            LE::RangeField(r, k) => {
                let ls = self.eval_locals(r);
                self.ins().local_get(ls[*k as usize]);
            }
            LE::GenNew(id, vals) => {
                let (_, ti, g) = self.cx.gens[id];
                let (offs, size) = gen_offsets(&g.func);
                let p = self.local(W);
                self.ins().i64_const(size as i64);
                self.rt("alxr_zalloc");
                self.ins().local_set(p);
                self.addr(p);
                self.ins().i64_const(ti as i64).i64_store(mem(0, F::Wd));
                for (k, v) in g.captures.iter().enumerate() {
                    let x = self.eval_locals(&vals[k]);
                    self.store(&g.func.vars[*v].ty, p, offs[*v], &x);
                }
                self.ins().local_get(p);
            }
            LE::ToP(x) => {
                self.e(x);
                self.ins().i64_const(0);
            }
        }
        out
    }

    fn eval_locals(&mut self, e: &LE) -> Vec<u32> {
        let ts = self.e(e);
        self.pop(&ts)
    }

    /// i32 c on the stack → (c op 0).
    fn cmp0(&mut self, op: Op) {
        self.ins().i32_const(0);
        match op {
            Op::Eq => self.ins().i32_eq(),
            Op::Ne => self.ins().i32_ne(),
            Op::Lt => self.ins().i32_lt_s(),
            Op::Le => self.ins().i32_le_s(),
            Op::Gt => self.ins().i32_gt_s(),
            _ => self.ins().i32_ge_s(),
        };
    }

    fn rt_expr(&mut self, r: Rt, args: &[LE]) {
        let call_ret = |s: &mut Self, name: &str, words: u32| {
            for a in args {
                s.e(a);
            }
            s.rt(name);
            s.ret_words(words);
        };
        let call = |s: &mut Self, name: &str| {
            for a in args {
                s.e(a);
            }
            s.rt(name);
        };
        match r {
            Rt::IntToS => call_ret(self, "alxr_int_to_s", 2),
            Rt::PIntToS => call_ret(self, "alxr_p_to_s", 2),
            Rt::StrRev => call_ret(self, "alxr_str_rev", 2),
            Rt::StrDelete => call_ret(self, "alxr_str_delete", 2),
            Rt::StrSplit => call_ret(self, "alxr_str_split", 3),
            Rt::StrToI => call(self, "alxr_str_to_i"),
            Rt::StrChar => call(self, "alxr_str_charlen"),
            Rt::StrByte => {
                if matches!(args[2], LE::I(0)) {
                    let s = self.eval_locals(&args[0]);
                    let i = self.eval_locals(&args[1])[0];
                    self.ins().local_get(s[0]).local_get(i).i64_add().i32_wrap_i64().i64_load8_u(mem(0, F::By));
                } else {
                    call_ret(self, "alxr_str_sub", 2);
                }
            }
            Rt::StrLen => {
                let s = self.eval_locals(&args[0]);
                self.ins().local_get(s[1]);
            }
            Rt::NDigits => call(self, "alxr_int_ndigits"),
            Rt::PNDigits => call(self, "alxr_p_ndigits"),
            Rt::Isqrt => call(self, "alxr_isqrt"),
            Rt::Digits => call_ret(self, "alxr_digits", 3),
            Rt::PDigits => call_ret(self, "alxr_p_digits", 3),
            Rt::SatAdd => {
                let a = self.eval_locals(&args[0])[0];
                let b = self.eval_locals(&args[1])[0];
                let r = self.add_sub(Op::Add, a, b);
                self.ins().i64_const(i64::MAX).local_get(r);
                self.ovf_test(Op::Add, a, b, r);
                self.ins().select();
            }
            Rt::ArrCopy => {
                let t = self.ty(&args[0]);
                let esz = lay(elem(&t)).size as i64;
                let a = self.eval_locals(&args[0]);
                self.ins().local_get(a[0]).local_get(a[1]).i64_const(esz);
                self.rt("alxr_copy");
                self.ins().local_get(a[1]).local_get(a[1]);
            }
            Rt::Even => {
                self.e(&args[0]);
                self.ins().i64_const(1).i64_and().i64_eqz();
            }
            Rt::PEven => call(self, "alxr_p_even"),
            Rt::PToI64 => call(self, "alxr_p_to_i64"),
            Rt::IntToF => {
                self.e(&args[0]);
                self.ins().f64_convert_i64_s();
            }
            Rt::FToI => call(self, "alxr_f_to_i"),
            Rt::FSqrt => {
                self.e(&args[0]);
                self.ins().f64_sqrt();
            }
            Rt::FAbs => {
                self.e(&args[0]);
                self.ins().f64_abs();
            }
            Rt::Math(f) => {
                for a in args {
                    self.e(a);
                }
                match f {
                    MathFn::Floor => drop(self.ins().f64_floor()),
                    MathFn::Ceil => drop(self.ins().f64_ceil()),
                    MathFn::Trunc => drop(self.ins().f64_trunc()),
                    MathFn::RoundEven => drop(self.ins().f64_nearest()),
                    MathFn::Copysign => drop(self.ins().f64_copysign()),
                    _ => {
                        // The host function takes three Floats and the function id.
                        for _ in args.len()..3 {
                            self.ins().f64_const(0.0.into());
                        }
                        self.i64c(f as u8 as i64);
                        self.rt("alxr_math");
                    }
                }
            }
            Rt::FBits => {
                self.e(&args[0]);
                self.ins().i64_reinterpret_f64();
            }
            Rt::FFromBits => {
                self.e(&args[0]);
                self.ins().f64_reinterpret_i64();
            }
            Rt::FToS => call_ret(self, "alxr_f_to_s", 2),
            Rt::U64ToS => call_ret(self, "alxr_u64_to_s", 2),
            Rt::IntFmt => call_ret(self, "alxr_int_fmt", 2),
            Rt::Errno | Rt::Strerror | Rt::StrFromCstr | Rt::StrFromPtr => unreachable!("rejected by uses_ffi"),
            Rt::NowNs | Rt::CapBegin => {
                self.ins().i64_const(0);
            }
            Rt::CapEnd => self.str_const(""),
            Rt::FileStatus => call(self, "alxr_file_status"),
            Rt::FileRead => call_ret(self, "alxr_file_read_or_empty", 2),
            Rt::RuneToS => call_ret(self, "alxr_rune_to_s", 2),
            Rt::FToU64 => call(self, "alxr_f_to_u64"),
            Rt::FFmt => call_ret(self, "alxr_f_fmt", 2),
            Rt::FFmtE => call_ret(self, "alxr_f_fmt_e", 2),
            Rt::StrPad => call_ret(self, "alxr_str_pad", 2),
            Rt::StrQuote => call_ret(self, "alxr_str_quote", 2),
            Rt::StrFromBytes => {
                let v = self.eval_locals(&args[0]);
                self.ins().local_get(v[0]).local_get(v[1]);
                self.rt("alxr_str_from_bytes");
                self.ret_words(2);
            }
            Rt::StrCat => {
                let p = self.local(W);
                self.ins().i64_const(16 * args.len() as i64);
                self.rt("alxr_alloc");
                self.ins().local_set(p);
                for (k, a) in args.iter().enumerate() {
                    let v = self.eval_locals(a);
                    self.store(&LTy::Str, p, 16 * k as u32, &v);
                }
                self.ins().local_get(p).i64_const(args.len() as i64);
                self.rt("alxr_str_cat");
                self.ret_words(2);
            }
        }
    }
}

/// Byte offset of field `k` of a tuple type (C layout).
fn field_offset(t: &LTy, k: usize) -> u32 {
    let LTy::Tup(ts) = t else { panic!("wasmgen: field of {t:?}") };
    let mut off = 0u32;
    for (i, ft) in ts.iter().enumerate() {
        let l = lay(ft);
        off = off.next_multiple_of(l.align);
        if i == k {
            return off;
        }
        off += l.size;
    }
    unreachable!()
}
