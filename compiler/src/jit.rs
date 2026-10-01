//! LIR → Cranelift, compiled in memory and run in-process: the backend
//! behind `alx run`. Same semantics as the C backend (cgen.rs), same runtime
//! (linked into `alx` by build.rs). Runtime entry points take only int64 and
//! pointer arguments (runtime/jit_shims.c), so aggregates never cross the
//! platform ABI by value.
//!
//! Values: every LIR type flattens to a list of scalars (`lay`), kept in SSA
//! variables. Memory (array elements, generator state, call results) uses
//! the C layout of the same type, so the runtime reads it unchanged.

use crate::lir::*;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::condcodes::FloatCC;
use cranelift_codegen::ir::types::{F64, I8, I64};
use cranelift_codegen::ir::{AbiParam, Block, BlockArg, FuncRef, InstBuilder, MemFlagsData, SigRef, Signature, StackSlotData, StackSlotKind, TrapCode, Type, Value};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module, default_libcall_names};
use std::collections::HashMap;
use std::ffi::CString;

macro_rules! runtime {
    ($($n:ident),* $(,)?) => {
        #[allow(clashing_extern_declarations)]
        unsafe extern "C" { $(pub fn $n();)* }
    };
}
mod rt {
    runtime!(
        alx_init, alx_panic, alx_overflow, alx_pow, alx_isqrt, alx_sort_i64, alx_sort_str, alx_puts_i64,
        alxj_alloc, alxj_zalloc, alxj_arr_alloc, alxj_arr_grow, alxj_arr_new, alxj_arr_copy,
        alxj_int_to_s, alxj_str_rev, alxj_str_delete, alxj_str_split, alxj_str_to_i, alxj_str_charlen, alxj_str_sub,
        alxj_str_eq, alxj_str_cmp, alxj_str_is_pal, alxj_int_ndigits, alxj_digits, alxj_try_pow,
        alxj_err_overflow, alxj_die, alxj_file_read, alxj_puts_str, alxj_puts_bool, alxj_puts_unit, alxj_pmap, alxj_finish,
        alxj_p_add, alxj_p_sub, alxj_p_mul, alxj_p_div, alxj_p_rem, alxj_p_pow, alxj_p_cmp, alxj_p_even, alxj_p_to_i64,
        alxj_p_to_s, alxj_p_ndigits, alxj_p_digits, alxj_puts_pint,
        alxj_puts_f64, alxj_f_to_s, alxj_f_fmt, alxj_f_to_i, alxj_str_cat,
    );
}
type RtFn = unsafe extern "C" fn();

const ERR_SIZE: u32 = 32; // sizeof(AlxErr)

/// C layout of a type: size, alignment, and its scalars (offset, type).
struct Lay {
    size: u32,
    align: u32,
    fields: Vec<(u32, Type)>,
}

fn lay(t: &LTy) -> Lay {
    let words = |n: u32| Lay { size: 8 * n, align: 8, fields: (0..n).map(|i| (8 * i, I64)).collect() };
    match t {
        LTy::I64 | LTy::Gen(_) => words(1),
        LTy::F64 => Lay { size: 8, align: 8, fields: vec![(0, F64)] },
        LTy::PInt | LTy::Str => words(2),
        LTy::Arr(_) => words(3),
        LTy::Bool => Lay { size: 1, align: 1, fields: vec![(0, I8)] },
        LTy::Unit => Lay { size: 1, align: 1, fields: vec![] },
        LTy::Range => Lay { size: 24, align: 8, fields: vec![(0, I64), (8, I64), (16, I8)] },
        LTy::Tup(ts) => {
            let (mut off, mut align, mut fields) = (0u32, 1u32, vec![]);
            for t in ts {
                let l = lay(t);
                off = off.next_multiple_of(l.align);
                fields.extend(l.fields.iter().map(|(o, ty)| (off + o, *ty)));
                off += l.size;
                align = align.max(l.align);
            }
            Lay { size: off.next_multiple_of(align).max(1), align, fields }
        }
    }
}

fn nflat(t: &LTy) -> usize {
    lay(t).fields.len()
}

fn elem(t: &LTy) -> &LTy {
    match t {
        LTy::Arr(e) => e,
        _ => panic!("jit: not an array: {t:?}"),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Plain,
    Fallible,
    Main,
    Gen,
    Worker,
}

enum ErrSrc {
    Overflow(Value),
    At(Value),
}

struct Decls<'p> {
    funcs: HashMap<&'p str, (FuncId, &'p LFunc)>,
    gens: HashMap<usize, (FuncId, &'p LGen)>,
    workers: HashMap<usize, FuncId>,
}

/// Compile the program and run it in this process. Returns only if the
/// program finishes normally (panics abort; uncaught errors exit 1).
pub fn run(p: &LProgram) -> Result<(), String> {
    let mut fb = settings::builder();
    let set = |fb: &mut settings::Builder, k: &str, v: &str| fb.set(k, v).map_err(|e| format!("jit: {k}: {e}"));
    set(&mut fb, "opt_level", "speed")?;
    set(&mut fb, "enable_multi_ret_implicit_sret", "true")?;
    set(&mut fb, "use_colocated_libcalls", "false")?;
    set(&mut fb, "is_pic", "false")?;
    let isa = cranelift_native::builder().map_err(|e| format!("jit: {e}"))?.finish(settings::Flags::new(fb)).map_err(|e| format!("jit: {e}"))?;
    let mut m = JITModule::new(JITBuilder::with_isa(isa, default_libcall_names()));

    let mut d = Decls { funcs: HashMap::new(), gens: HashMap::new(), workers: HashMap::new() };
    for f in &p.funcs {
        if f.external {
            return Err(format!("jit: external function `{}`", f.name));
        }
        let sig = user_sig(&m, f);
        let id = m.declare_function(&f.name, Linkage::Local, &sig).map_err(|e| e.to_string())?;
        d.funcs.insert(&f.name, (id, f));
    }
    for g in &p.gens {
        let id = m.declare_function(&format!("gen{}_next", g.id), Linkage::Local, &gen_sig(&m)).map_err(|e| e.to_string())?;
        d.gens.insert(g.id, (id, g));
    }
    for w in &p.workers {
        let id = m.declare_function(&format!("worker{}", w.id), Linkage::Local, &worker_sig(&m)).map_err(|e| e.to_string())?;
        d.workers.insert(w.id, id);
    }

    let mut ctx = m.make_context();
    let mut fbc = FunctionBuilderContext::new();
    let mut strs = Strs::default();
    let mut jobs: Vec<(FuncId, &LFunc, Kind, Option<&LGen>)> = vec![];
    for f in &p.funcs {
        let k = if f.is_main {
            Kind::Main
        } else if f.fallible {
            Kind::Fallible
        } else {
            Kind::Plain
        };
        jobs.push((d.funcs[f.name.as_str()].0, f, k, None));
    }
    for g in &p.gens {
        jobs.push((d.gens[&g.id].0, &g.func, Kind::Gen, Some(g)));
    }
    for w in &p.workers {
        jobs.push((d.workers[&w.id], &w.func, Kind::Worker, None));
    }
    let fcfg = m.isa().frontend_config();
    for (id, f, kind, g) in jobs {
        ctx.func.signature = match kind {
            Kind::Gen => gen_sig(&m),
            Kind::Worker => worker_sig(&m),
            _ => user_sig(&m, f),
        };
        {
            let b = FunctionBuilder::new(&mut ctx.func, &mut fbc);
            let mut fx = Fx { b, m: &mut m, d: &d, strs: &mut strs, f, kind, lgen: g, vars: vec![], loops: HashMap::new(), params: vec![], gstate: None, resume: vec![], yields: 0, sigs: HashMap::new(), frefs: HashMap::new() };
            fx.body();
            fx.b.seal_all_blocks();
            fx.b.finalize(fcfg);
        }
        m.define_function(id, &mut ctx).map_err(|e| format!("jit: {}: {e:?}", f.name))?;
        m.clear_context(&mut ctx);
    }
    m.finalize_definitions().map_err(|e| e.to_string())?;
    let main = p.funcs.iter().find(|f| f.is_main).ok_or("jit: no main")?;
    let ptr = m.get_finalized_function(d.funcs[main.name.as_str()].0);
    unsafe {
        let main: extern "C" fn() = std::mem::transmute(ptr);
        rt::alx_init();
        main();
        rt::alxj_finish();
    }
    std::mem::forget(m);
    Ok(())
}

fn user_sig(m: &JITModule, f: &LFunc) -> Signature {
    let mut s = m.make_signature();
    if f.is_main {
        return s;
    }
    for v in &f.params {
        for (_, t) in lay(&f.vars[*v].ty).fields {
            s.params.push(AbiParam::new(t));
        }
    }
    if f.fallible {
        if f.ret != LTy::Unit {
            s.params.push(AbiParam::new(I64));
        }
        s.params.push(AbiParam::new(I64));
        s.returns.push(AbiParam::new(I8).uext());
    } else {
        for (_, t) in lay(&f.ret).fields {
            s.returns.push(AbiParam::new(t));
        }
    }
    s
}

/// `bool next(AlxGen *g, void *out)`
fn gen_sig(m: &JITModule) -> Signature {
    let mut s = m.make_signature();
    s.params.extend([AbiParam::new(I64), AbiParam::new(I64)]);
    s.returns.push(AbiParam::new(I8).uext());
    s
}

/// `bool worker(const void *in, void *out, AlxErr *err)`, called by alx_pmap.
fn worker_sig(m: &JITModule) -> Signature {
    let mut s = m.make_signature();
    s.params.extend([AbiParam::new(I64), AbiParam::new(I64), AbiParam::new(I64)]);
    s.returns.push(AbiParam::new(I8).uext());
    s
}

/// String literals and locations, leaked: they live as long as the program.
#[derive(Default)]
struct Strs {
    bytes: HashMap<String, usize>,
    cstrs: HashMap<String, usize>,
}

impl Strs {
    fn bytes(&mut self, s: &str) -> i64 {
        *self.bytes.entry(s.to_string()).or_insert_with(|| Box::leak(s.as_bytes().to_vec().into_boxed_slice()).as_ptr() as usize) as i64
    }
    fn cstr(&mut self, s: &str) -> i64 {
        *self.cstrs.entry(s.to_string()).or_insert_with(|| Box::leak(CString::new(s.replace('\0', "")).unwrap().into_boxed_c_str()).as_ptr() as usize) as i64
    }
}

struct Fx<'m, 'b, 'p> {
    b: FunctionBuilder<'b>,
    m: &'m mut JITModule,
    d: &'m Decls<'p>,
    strs: &'m mut Strs,
    f: &'p LFunc,
    kind: Kind,
    lgen: Option<&'p LGen>,
    vars: Vec<Vec<Variable>>,
    loops: HashMap<Label, (Block, Block)>,
    /// Entry block parameters.
    params: Vec<Value>,
    /// Generators: state pointer and each variable's offset in the state.
    gstate: Option<(Value, Vec<u32>)>,
    resume: Vec<Block>,
    yields: usize,
    sigs: HashMap<(Vec<Type>, Option<Type>), SigRef>,
    frefs: HashMap<FuncId, FuncRef>,
}

fn bargs(v: &[Value]) -> Vec<BlockArg> {
    v.iter().map(|&x| x.into()).collect()
}

fn count_yields(ss: &[LS]) -> usize {
    ss.iter()
        .map(|s| match s {
            LS::Yield(_) => 1,
            LS::If(_, a, b) => count_yields(a) + count_yields(b),
            LS::Loop(_, b) => count_yields(b),
            _ => 0,
        })
        .sum()
}

/// Generator state: `AlxGen base` (next pointer), then `state`, then every variable.
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

impl Fx<'_, '_, '_> {
    // ---------- plumbing ----------

    fn ic(&mut self, v: i64) -> Value {
        self.b.ins().iconst(I64, v)
    }

    /// End the current block with a jump to `blk` and continue there.
    fn enter(&mut self, blk: Block) {
        self.b.ins().jump(blk, &[]);
        self.b.switch_to_block(blk);
    }

    /// After a terminator: continue in a fresh (unreachable) block.
    fn fresh(&mut self) {
        let blk = self.b.create_block();
        self.b.switch_to_block(blk);
    }

    fn call_rt(&mut self, f: RtFn, args: &[Value], ret: bool) -> Option<Value> {
        self.call_rt_t(f, args, ret.then_some(I64))
    }

    /// Call a runtime function: i64/pointer and f64 arguments (bools are
    /// widened), an optional i64 or f64 result.
    fn call_rt_t(&mut self, f: RtFn, args: &[Value], ret: Option<Type>) -> Option<Value> {
        let args: Vec<Value> = args
            .iter()
            .map(|&a| match self.b.func.dfg.value_type(a) {
                I64 | F64 => a,
                _ => self.b.ins().uextend(I64, a),
            })
            .collect();
        let tys: Vec<Type> = args.iter().map(|a| self.b.func.dfg.value_type(*a)).collect();
        let key = (tys.clone(), ret);
        let sig = match self.sigs.get(&key) {
            Some(s) => *s,
            None => {
                let mut s = self.m.make_signature();
                s.params.extend(tys.iter().map(|t| AbiParam::new(*t)));
                if let Some(r) = ret {
                    s.returns.push(AbiParam::new(r));
                }
                let r = self.b.import_signature(s);
                self.sigs.insert(key, r);
                r
            }
        };
        let fp = self.ic(f as usize as i64);
        let call = self.b.ins().call_indirect(sig, fp, &args);
        if ret.is_some() { Some(self.b.inst_results(call)[0]) } else { None }
    }

    fn rt_bool(&mut self, f: RtFn, args: &[Value]) -> Value {
        let r = self.call_rt(f, args, true).unwrap();
        self.b.ins().icmp_imm(IntCC::NotEqual, r, 0)
    }

    fn fref(&mut self, id: FuncId) -> FuncRef {
        if let Some(r) = self.frefs.get(&id) {
            return *r;
        }
        let r = self.m.declare_func_in_func(id, self.b.func);
        self.frefs.insert(id, r);
        r
    }

    fn slot(&mut self, size: u32) -> Value {
        let ss = self.b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, size.max(1), 3));
        self.b.ins().stack_addr(I64, ss, 0)
    }

    fn store(&mut self, t: &LTy, vals: &[Value], addr: Value, off: i32) {
        for ((o, _), v) in lay(t).fields.iter().zip(vals) {
            self.b.ins().store(MemFlagsData::trusted(), *v, addr, off + *o as i32);
        }
    }

    fn load(&mut self, t: &LTy, addr: Value, off: i32) -> Vec<Value> {
        lay(t).fields.iter().map(|(o, ty)| self.b.ins().load(*ty, MemFlagsData::trusted(), addr, off + *o as i32)).collect()
    }

    fn spill(&mut self, t: &LTy, vals: &[Value]) -> Value {
        let a = self.slot(lay(t).size);
        self.store(t, vals, a, 0);
        a
    }

    fn cstr(&mut self, s: &str) -> Value {
        let p = self.strs.cstr(s);
        self.ic(p)
    }

    /// Abort with a message: never returns.
    fn panic(&mut self, msg: &str, loc: &str) {
        let (m, l) = (self.cstr(msg), self.cstr(loc));
        self.call_rt(rt::alx_panic, &[m, l], false);
        self.b.ins().trap(TrapCode::unwrap_user(1));
        self.fresh();
    }

    /// If `cond`, run `then` in a cold block that never falls through.
    fn cold_if(&mut self, cond: Value, then: impl FnOnce(&mut Self)) {
        let (cold, cont) = (self.b.create_block(), self.b.create_block());
        self.b.set_cold_block(cold);
        self.b.ins().brif(cond, cold, &[], cont, &[]);
        self.b.switch_to_block(cold);
        then(self);
        self.enter(cont);
    }

    fn overflow_if(&mut self, cond: Value, loc: &str) {
        self.cold_if(cond, |s| {
            let l = s.cstr(loc);
            s.call_rt(rt::alx_overflow, &[l], false);
            s.b.ins().trap(TrapCode::unwrap_user(1));
            s.fresh();
        });
    }

    fn get(&mut self, v: V) -> Vec<Value> {
        self.vars[v].clone().into_iter().map(|x| self.b.use_var(x)).collect()
    }

    fn set(&mut self, v: V, vals: &[Value]) {
        for (x, val) in self.vars[v].clone().into_iter().zip(vals) {
            self.b.def_var(x, *val);
        }
    }

    // ---------- functions ----------

    fn body(&mut self) {
        let f = self.f;
        let entry = self.b.create_block();
        self.b.append_block_params_for_function_params(entry);
        self.b.switch_to_block(entry);
        self.params = self.b.block_params(entry).to_vec();
        for v in &f.vars {
            let vs = lay(&v.ty).fields.iter().map(|(_, t)| self.b.declare_var(*t)).collect();
            self.vars.push(vs);
        }
        match self.kind {
            Kind::Gen => {
                let (offs, _) = gen_offsets(f);
                let g = self.params[0];
                for (i, v) in f.vars.iter().enumerate() {
                    let vals = self.load(&v.ty, g, offs[i] as i32);
                    self.set(i, &vals);
                }
                self.gstate = Some((g, offs));
                let n = count_yields(&f.body);
                self.resume = (0..n).map(|_| self.b.create_block()).collect();
                let (start, done) = (self.b.create_block(), self.b.create_block());
                let st = self.b.ins().load(I64, MemFlagsData::trusted(), g, 8);
                let mut table = vec![start];
                table.extend(self.resume.iter().copied());
                // state: 0 = start, k = after yield k, -1 = finished
                let jt_data = cranelift_codegen::ir::JumpTableData::new(
                    self.b.func.dfg.block_call(done, &[]),
                    &table.iter().map(|b| self.b.func.dfg.block_call(*b, &[])).collect::<Vec<_>>(),
                );
                let jt = self.b.create_jump_table(jt_data);
                // -1 (finished) reduces to u32::MAX: out of the table, so `done`.
                let idx = self.b.ins().ireduce(cranelift_codegen::ir::types::I32, st);
                self.b.ins().br_table(idx, jt);
                self.b.switch_to_block(done);
                let z = self.b.ins().iconst(I8, 0);
                self.b.ins().return_(&[z]);
                self.b.switch_to_block(start);
            }
            Kind::Worker => {
                let p = f.params[0];
                let input = self.params[0];
                let vals = self.load(&f.vars[p].ty, input, 0);
                self.set(p, &vals);
                self.zero_vars(&[p]);
            }
            _ => {
                let mut k = 0;
                for &p in &f.params {
                    let n = self.vars[p].len();
                    let vals = self.params[k..k + n].to_vec();
                    self.set(p, &vals);
                    k += n;
                }
                let params = f.params.clone();
                self.zero_vars(&params);
            }
        }
        self.block(&f.body);
        // Falling off the end.
        match self.kind {
            Kind::Main | Kind::Plain if f.ret == LTy::Unit => {
                self.b.ins().return_(&[]);
                self.fresh();
            }
            Kind::Worker | Kind::Fallible if f.ret == LTy::Unit || self.kind == Kind::Worker => {
                let one = self.b.ins().iconst(I8, 1);
                self.b.ins().return_(&[one]);
                self.fresh();
            }
            Kind::Gen => self.gen_finish(),
            _ => self.panic("function ended without a value", &f.name.clone()),
        }
        self.b.ins().trap(TrapCode::unwrap_user(1));
    }

    fn zero_vars(&mut self, skip: &[V]) {
        for i in 0..self.vars.len() {
            if skip.contains(&i) {
                continue;
            }
            let zs: Vec<Value> = lay(&self.f.vars[i].ty).fields.iter().map(|(_, t)| self.zero(*t)).collect();
            self.set(i, &zs);
        }
    }

    fn zero(&mut self, t: Type) -> Value {
        if t == F64 { self.b.ins().f64const(0.0) } else { self.b.ins().iconst(t, 0) }
    }

    fn gen_finish(&mut self) {
        let (g, _) = self.gstate.clone().unwrap();
        let m1 = self.ic(-1);
        self.b.ins().store(MemFlagsData::trusted(), m1, g, 8);
        let z = self.b.ins().iconst(I8, 0);
        self.b.ins().return_(&[z]);
        self.fresh();
    }

    fn block(&mut self, ss: &[LS]) {
        for s in ss {
            self.stmt(s);
        }
    }

    /// The error path: return the error to the caller, or print it and exit.
    fn err_path(&mut self, path: &ErrPath, src: ErrSrc) {
        let ret = matches!(path, ErrPath::Return) && matches!(self.kind, Kind::Fallible | Kind::Worker);
        if ret {
            let errp = *self.params.last().unwrap();
            match src {
                ErrSrc::Overflow(loc) => {
                    self.call_rt(rt::alxj_err_overflow, &[errp, loc], false);
                }
                ErrSrc::At(p) => {
                    for o in (0..ERR_SIZE as i32).step_by(8) {
                        let w = self.b.ins().load(I64, MemFlagsData::trusted(), p, o);
                        self.b.ins().store(MemFlagsData::trusted(), w, errp, o);
                    }
                }
            }
            let z = self.b.ins().iconst(I8, 0);
            self.b.ins().return_(&[z]);
        } else {
            let p = match src {
                ErrSrc::Overflow(loc) => {
                    let p = self.slot(ERR_SIZE);
                    self.call_rt(rt::alxj_err_overflow, &[p, loc], false);
                    p
                }
                ErrSrc::At(p) => p,
            };
            self.call_rt(rt::alxj_die, &[p], false);
            self.b.ins().trap(TrapCode::unwrap_user(1));
        }
        self.fresh();
    }

    fn fail_if(&mut self, cond: Value, path: &ErrPath, src: impl FnOnce(&mut Self) -> ErrSrc) {
        self.cold_if(cond, |s| {
            let e = src(s);
            s.err_path(path, e);
        });
    }

    // ---------- statements ----------

    fn stmt(&mut self, s: &LS) {
        match s {
            LS::Set(v, e) => {
                let x = self.e(e);
                self.set(*v, &x);
            }
            LS::SetIndex { arr, idx, val, check } => {
                let a = self.get(*arr);
                let i = self.e1(idx);
                let x = self.e(val);
                let et = elem(&self.f.vars[*arr].ty).clone();
                let addr = self.elem_addr(&a, i, &et, check.as_deref());
                self.store(&et, &x, addr, 0);
            }
            LS::SetPlace { var, steps, val } => {
                let x = self.e(val);
                // Walk the steps: through the variable's own scalars until the
                // first index, then through memory.
                let mut ty = self.f.vars[*var].ty.clone();
                let mut start = 0usize;
                let mut mem: Option<(Value, i32)> = None;
                for st in steps {
                    match st {
                        Step::Field(k) => {
                            let LTy::Tup(ts) = ty.clone() else { panic!("jit: field of {ty:?}") };
                            match &mut mem {
                                None => start += ts[..*k].iter().map(nflat).sum::<usize>(),
                                Some((_, off)) => *off += field_offset(&ty, *k) as i32,
                            }
                            ty = ts[*k].clone();
                        }
                        Step::Index(i, check) => {
                            let a = match mem {
                                None => self.get(*var)[start..start + 3].to_vec(),
                                Some((addr, off)) => self.load(&ty, addr, off),
                            };
                            let iv = self.e1(i);
                            let et = elem(&ty).clone();
                            let addr = self.elem_addr(&a, iv, &et, check.as_deref());
                            mem = Some((addr, 0));
                            ty = et;
                        }
                    }
                }
                match mem {
                    None => {
                        let mut all = self.get(*var);
                        all.splice(start..start + x.len(), x);
                        self.set(*var, &all);
                    }
                    Some((addr, off)) => self.store(&ty, &x, addr, off),
                }
            }
            LS::Push(v, e) => {
                let x = self.e(e);
                let et = elem(&self.f.vars[*v].ty).clone();
                let esz = lay(&et).size as i64;
                let a = self.get(*v);
                let full = self.b.ins().icmp(IntCC::Equal, a[1], a[2]);
                let (grow, cont) = (self.b.create_block(), self.b.create_block());
                self.b.ins().brif(full, grow, &[], cont, &[]);
                self.b.switch_to_block(grow);
                let dbl = self.b.ins().ishl_imm(a[2], 1);
                let four = self.ic(4);
                let nc = self.b.ins().select(a[2], dbl, four);
                let ez = self.ic(esz);
                let np = self.call_rt(rt::alxj_arr_grow, &[a[0], a[1], nc, ez], true).unwrap();
                self.set(*v, &[np, a[1], nc]);
                self.enter(cont);
                let a = self.get(*v);
                let off = self.b.ins().imul_imm(a[1], esz);
                let addr = self.b.ins().iadd(a[0], off);
                self.store(&et, &x, addr, 0);
                let n1 = self.b.ins().iadd_imm(a[1], 1);
                self.set(*v, &[a[0], n1, a[2]]);
            }
            LS::Eval(e) => {
                self.e(e);
            }
            LS::If(c, a, b) => {
                let c = self.e1(c);
                let (tb, eb, join) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
                self.b.ins().brif(c, tb, &[], eb, &[]);
                self.b.switch_to_block(tb);
                self.block(a);
                self.b.ins().jump(join, &[]);
                self.b.switch_to_block(eb);
                self.block(b);
                self.enter(join);
            }
            LS::Loop(l, body) => {
                let (head, exit) = (self.b.create_block(), self.b.create_block());
                self.loops.insert(*l, (head, exit));
                self.enter(head);
                self.block(body);
                self.b.ins().jump(head, &[]);
                self.b.switch_to_block(exit);
            }
            LS::Break(l) => {
                let exit = self.loops[l].1;
                self.b.ins().jump(exit, &[]);
                self.fresh();
            }
            LS::Continue(l) => {
                let head = self.loops[l].0;
                self.b.ins().jump(head, &[]);
                self.fresh();
            }
            LS::Return(v) => {
                match (self.kind, v) {
                    (Kind::Fallible | Kind::Worker, v) => {
                        if let Some(v) = v {
                            let x = self.e(v);
                            let outp = if self.kind == Kind::Worker { self.params[1] } else { self.params[self.params.len() - 2] };
                            let rt = self.f.ret.clone();
                            self.store(&rt, &x, outp, 0);
                        }
                        let one = self.b.ins().iconst(I8, 1);
                        self.b.ins().return_(&[one]);
                    }
                    (Kind::Plain, Some(v)) => {
                        let x = self.e(v);
                        self.b.ins().return_(&x);
                    }
                    (Kind::Gen, _) => {
                        self.gen_finish();
                        return;
                    }
                    _ => {
                        self.b.ins().return_(&[]);
                    }
                }
                self.fresh();
            }
            LS::TryArith { dst, op, a, b, loc, path } => {
                let (a, b) = (self.e1(a), self.e1(b));
                let r = match op {
                    Op::Add | Op::Sub | Op::Mul => {
                        let (r, of) = match op {
                            Op::Add => self.b.ins().sadd_overflow(a, b),
                            Op::Sub => self.b.ins().ssub_overflow(a, b),
                            _ => self.b.ins().smul_overflow(a, b),
                        };
                        self.fail_if(of, path, |s| ErrSrc::Overflow(s.cstr(loc)));
                        r
                    }
                    Op::Div | Op::Rem => {
                        let bad = self.div_bad(*op, a, b);
                        self.fail_if(bad, path, |s| ErrSrc::Overflow(s.cstr(loc)));
                        self.floor_divrem(*op, a, b)
                    }
                    _ => {
                        let out = self.slot(8);
                        let ok = self.rt_bool(rt::alxj_try_pow, &[a, b, out]);
                        let bad = self.b.ins().icmp_imm(IntCC::Equal, ok, 0);
                        self.fail_if(bad, path, |s| ErrSrc::Overflow(s.cstr(loc)));
                        self.b.ins().load(I64, MemFlagsData::trusted(), out, 0)
                    }
                };
                self.set(*dst, &[r]);
            }
            LS::TryCall { dst, f, args, path } => {
                let (id, callee) = self.d.funcs[f.as_str()];
                let mut av = vec![];
                for a in args {
                    av.extend(self.e(a));
                }
                let out = if callee.ret != LTy::Unit { Some(self.slot(lay(&callee.ret).size)) } else { None };
                av.extend(out);
                let errp = self.slot(ERR_SIZE);
                av.push(errp);
                let fr = self.fref(id);
                let call = self.b.ins().call(fr, &av);
                let ok = self.b.inst_results(call)[0];
                let bad = self.b.ins().icmp_imm(IntCC::Equal, ok, 0);
                self.fail_if(bad, path, |_| ErrSrc::At(errp));
                if let (Some(d), Some(out)) = (dst, out) {
                    let vals = self.load(&callee.ret.clone(), out, 0);
                    self.set(*d, &vals);
                }
            }
            LS::TryRead { dst, path_arg, loc, path } => {
                let pv = self.e(path_arg);
                let ps = self.spill(&LTy::Str, &pv);
                let l = self.cstr(loc);
                let out = self.slot(16);
                let errp = self.slot(ERR_SIZE);
                let ok = self.rt_bool(rt::alxj_file_read, &[ps, l, out, errp]);
                let bad = self.b.ins().icmp_imm(IntCC::Equal, ok, 0);
                self.fail_if(bad, path, |_| ErrSrc::At(errp));
                let vals = self.load(&LTy::Str, out, 0);
                self.set(*dst, &vals);
            }
            LS::NextOrBreak { source, dst, label } => {
                let g = self.e1(source);
                let fp = self.b.ins().load(I64, MemFlagsData::trusted(), g, 0);
                let dt = self.f.vars[*dst].ty.clone();
                let out = self.slot(lay(&dt).size);
                let sig = self.b.import_signature(gen_sig(self.m));
                let call = self.b.ins().call_indirect(sig, fp, &[g, out]);
                let ok = self.b.inst_results(call)[0];
                let exit = self.loops[label].1;
                let cont = self.b.create_block();
                self.b.ins().brif(ok, cont, &[], exit, &[]);
                self.b.switch_to_block(cont);
                let vals = self.load(&dt, out, 0);
                self.set(*dst, &vals);
            }
            LS::Yield(e) => {
                let x = self.e(e);
                let (g, offs) = self.gstate.clone().unwrap();
                let et = self.lgen.unwrap().elem.clone();
                let outp = self.params[1];
                self.store(&et, &x, outp, 0);
                for (i, v) in self.f.vars.iter().enumerate() {
                    let vals = self.get(i);
                    self.store(&v.ty, &vals, g, offs[i] as i32);
                }
                let k = self.ic(self.yields as i64 + 1);
                self.b.ins().store(MemFlagsData::trusted(), k, g, 8);
                let one = self.b.ins().iconst(I8, 1);
                self.b.ins().return_(&[one]);
                let blk = self.resume[self.yields];
                self.yields += 1;
                self.b.switch_to_block(blk);
                // Variables were reloaded from the state at entry.
            }
            LS::Pmap { dst, arr, worker, path } => {
                let at = self.ty(arr);
                let a = self.e(arr);
                let in_esz = lay(elem(&at)).size as i64;
                let out_t = elem(&self.f.vars[*dst].ty).clone();
                let out_esz = lay(&out_t).size as i64;
                let (ie, oe) = (self.ic(in_esz), self.ic(out_esz));
                let outp = self.call_rt(rt::alxj_arr_alloc, &[a[1], oe], true).unwrap();
                let errp = self.slot(ERR_SIZE);
                let wid = self.d.workers[worker];
                let fr = self.fref(wid);
                let wf = self.b.ins().func_addr(I64, fr);
                let ok = self.rt_bool(rt::alxj_pmap, &[a[0], a[1], ie, outp, oe, wf, errp]);
                let bad = self.b.ins().icmp_imm(IntCC::Equal, ok, 0);
                self.fail_if(bad, path, |_| ErrSrc::At(errp));
                self.set(*dst, &[outp, a[1], a[1]]);
            }
            LS::Puts(e, t) => {
                let x = self.e(e);
                match t {
                    LTy::I64 => {
                        self.call_rt(rt::alx_puts_i64, &x, false);
                    }
                    LTy::F64 => {
                        self.call_rt(rt::alxj_puts_f64, &x, false);
                    }
                    LTy::Bool => {
                        self.call_rt(rt::alxj_puts_bool, &x, false);
                    }
                    LTy::Str => {
                        let p = self.spill(t, &x);
                        self.call_rt(rt::alxj_puts_str, &[p], false);
                    }
                    LTy::PInt => {
                        let p = self.spill(t, &x);
                        self.call_rt(rt::alxj_puts_pint, &[p], false);
                    }
                    LTy::Unit => {
                        self.call_rt(rt::alxj_puts_unit, &[], false);
                    }
                    _ => self.panic("`puts` of this type isn't supported yet", "puts"),
                }
            }
            LS::Panic(msg, loc) => self.panic(msg, loc),
            LS::SortInPlace(v, el) => {
                let f = match el {
                    LTy::I64 => rt::alx_sort_i64,
                    LTy::Str => rt::alx_sort_str,
                    _ => return self.panic("`sort` of this element type isn't supported yet", "sort"),
                };
                let t = self.f.vars[*v].ty.clone();
                let a = self.get(*v);
                let p = self.spill(&t, &a);
                self.call_rt(f, &[p], false);
                let a = self.load(&t, p, 0);
                self.set(*v, &a);
            }
        }
    }

    // ---------- expressions ----------

    fn ty(&self, e: &LE) -> LTy {
        match e {
            LE::Var(v) => self.f.vars[*v].ty.clone(),
            LE::I(_) | LE::Loc(_) | LE::Arith(..) | LE::Neg(..) | LE::Len(_) => LTy::I64,
            LE::F(_) | LE::FArith(..) | LE::FNeg(_) => LTy::F64,
            LE::B(_) | LE::Cmp(..) | LE::Not(_) => LTy::Bool,
            LE::S(_) => LTy::Str,
            LE::Unit => LTy::Unit,
            LE::Tup(t, _) => t.clone(),
            LE::Field(x, i) => match self.ty(x) {
                LTy::Tup(ts) => ts[*i].clone(),
                t => panic!("jit: field of {t:?}"),
            },
            LE::PArith(op, ..) => match op {
                Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge => LTy::Bool,
                _ => LTy::PInt,
            },
            LE::Cond(_, a, _) => self.ty(a),
            LE::Call(f, _) => self.d.funcs[f.as_str()].1.ret.clone(),
            LE::Rt(rt, args) => match rt {
                Rt::IntToS | Rt::PIntToS | Rt::StrRev | Rt::StrDelete | Rt::FToS | Rt::FFmt | Rt::StrCat => LTy::Str,
                Rt::IntToF | Rt::FSqrt | Rt::FAbs => LTy::F64,
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
            LE::GenNew(id, _) => LTy::Gen(Box::new(self.d.gens[id].1.elem.clone())),
            LE::ToP(_) => LTy::PInt,
        }
    }

    fn e1(&mut self, e: &LE) -> Value {
        let v = self.e(e);
        assert_eq!(v.len(), 1, "jit: expected a scalar: {e:?}");
        v[0]
    }

    fn elem_addr(&mut self, a: &[Value], i: Value, et: &LTy, check: Option<&str>) -> Value {
        if let Some(loc) = check {
            let oob = self.b.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, i, a[1]);
            let loc = loc.to_string();
            self.cold_if(oob, |s| s.panic("index out of bounds", &loc));
        }
        let off = self.b.ins().imul_imm(i, lay(et).size as i64);
        self.b.ins().iadd(a[0], off)
    }

    /// Division by zero, or MIN / -1 (Div only): the cases that fail.
    fn div_bad(&mut self, op: Op, a: Value, b: Value) -> Value {
        let z = self.b.ins().icmp_imm(IntCC::Equal, b, 0);
        if op == Op::Rem {
            return z;
        }
        let m1 = self.b.ins().icmp_imm(IntCC::Equal, b, -1);
        let mn = self.b.ins().icmp_imm(IntCC::Equal, a, i64::MIN);
        let both = self.b.ins().band(m1, mn);
        self.b.ins().bor(z, both)
    }

    /// Truncating division / remainder (Go semantics); `b` is nonzero, and
    /// not -1 with a == MIN for Div.
    fn floor_divrem(&mut self, op: Op, a: Value, b: Value) -> Value {
        // Divide by 1 instead of -1: avoids the machine trap on MIN % -1.
        let m1 = self.b.ins().icmp_imm(IntCC::Equal, b, -1);
        let one = self.ic(1);
        let bs = self.b.ins().select(m1, one, b);
        if op == Op::Div {
            let q = self.b.ins().sdiv(a, bs);
            let na = self.b.ins().ineg(a);
            self.b.ins().select(m1, na, q)
        } else {
            let r = self.b.ins().srem(a, bs);
            let z = self.ic(0);
            self.b.ins().select(m1, z, r)
        }
    }

    fn arith(&mut self, op: Op, a: &LE, b: &LE, ovf: &Ovf) -> Value {
        let konst = if let LE::I(k) = b { Some(*k) } else { None };
        let (av, bv) = (self.e1(a), self.e1(b));
        let loc = match ovf {
            Ovf::Panic(l) => l.as_str(),
            Ovf::Wrap => "wrap",
            Ovf::Unchecked => "proven",
        };
        match op {
            Op::Add | Op::Sub | Op::Mul => match ovf {
                Ovf::Panic(loc) => {
                    let (r, of) = match op {
                        Op::Add => self.b.ins().sadd_overflow(av, bv),
                        Op::Sub => self.b.ins().ssub_overflow(av, bv),
                        _ => self.b.ins().smul_overflow(av, bv),
                    };
                    self.overflow_if(of, &loc.clone());
                    r
                }
                _ => match op {
                    Op::Add => self.b.ins().iadd(av, bv),
                    Op::Sub => self.b.ins().isub(av, bv),
                    _ => self.b.ins().imul(av, bv),
                },
            },
            Op::Div | Op::Rem => {
                if let Some(k) = konst.filter(|k| *k > 1 && (*k as u64).is_power_of_two()) {
                    // Truncating division by 2^s: bias negatives by 2^s - 1, then shift.
                    let sh = k.trailing_zeros() as i64;
                    let sign = self.b.ins().sshr_imm(av, 63);
                    let bias = self.b.ins().ushr_imm(sign, 64 - sh);
                    let biased = self.b.ins().iadd(av, bias);
                    let q = self.b.ins().sshr_imm(biased, sh);
                    return if op == Op::Div {
                        q
                    } else {
                        let qk = self.b.ins().ishl_imm(q, sh);
                        self.b.ins().isub(av, qk)
                    };
                }
                if konst.is_none_or(|k| k == 0 || k == -1) {
                    let z = self.b.ins().icmp_imm(IntCC::Equal, bv, 0);
                    let loc = loc.to_string();
                    self.cold_if(z, |s| s.panic("division by zero", &loc));
                    if op == Op::Div {
                        let m1 = self.b.ins().icmp_imm(IntCC::Equal, bv, -1);
                        let mn = self.b.ins().icmp_imm(IntCC::Equal, av, i64::MIN);
                        let both = self.b.ins().band(m1, mn);
                        self.overflow_if(both, &loc);
                    }
                }
                self.floor_divrem(op, av, bv)
            }
            Op::Pow => {
                let l = self.cstr(loc);
                self.call_rt(rt::alx_pow, &[av, bv, l], true).unwrap()
            }
            _ => unreachable!(),
        }
    }

    fn short_circuit(&mut self, and: bool, a: &LE, b: &LE) -> Value {
        let av = self.e1(a);
        let (rhs, join) = (self.b.create_block(), self.b.create_block());
        let r = self.b.append_block_param(join, I8);
        if and {
            self.b.ins().brif(av, rhs, &[], join, &bargs(&[av]));
        } else {
            self.b.ins().brif(av, join, &bargs(&[av]), rhs, &[]);
        }
        self.b.switch_to_block(rhs);
        let bv = self.e1(b);
        self.b.ins().jump(join, &bargs(&[bv]));
        self.b.switch_to_block(join);
        r
    }

    fn icmp_of(op: Op) -> IntCC {
        match op {
            Op::Eq => IntCC::Equal,
            Op::Ne => IntCC::NotEqual,
            Op::Lt => IntCC::SignedLessThan,
            Op::Le => IntCC::SignedLessThanOrEqual,
            Op::Gt => IntCC::SignedGreaterThan,
            Op::Ge => IntCC::SignedGreaterThanOrEqual,
            _ => unreachable!(),
        }
    }

    fn pint_call(&mut self, f: RtFn, out_ty: Option<&LTy>, args: &[&LE], loc: Option<&str>) -> Vec<Value> {
        let mut ptrs = vec![];
        let out = out_ty.map(|t| self.slot(lay(t).size));
        ptrs.extend(out);
        for a in args {
            let t = self.ty(a);
            let v = self.e(a);
            ptrs.push(self.spill(&t, &v));
        }
        if let Some(l) = loc {
            ptrs.push(self.cstr(l));
        }
        match out_ty {
            Some(t) => {
                self.call_rt(f, &ptrs, false);
                self.load(t, out.unwrap(), 0)
            }
            None => vec![self.call_rt(f, &ptrs, true).unwrap()],
        }
    }

    fn e(&mut self, e: &LE) -> Vec<Value> {
        match e {
            LE::Var(v) => self.get(*v),
            LE::I(i) => vec![self.ic(*i)],
            LE::F(v) => vec![self.b.ins().f64const(*v)],
            LE::FArith(op, a, b) => {
                let (a, b) = (self.e1(a), self.e1(b));
                vec![match op {
                    Op::Add => self.b.ins().fadd(a, b),
                    Op::Sub => self.b.ins().fsub(a, b),
                    Op::Mul => self.b.ins().fmul(a, b),
                    _ => self.b.ins().fdiv(a, b),
                }]
            }
            LE::FNeg(x) => {
                let v = self.e1(x);
                vec![self.b.ins().fneg(v)]
            }
            LE::B(b) => vec![self.b.ins().iconst(I8, *b as i64)],
            LE::S(s) => {
                let p = self.strs.bytes(s);
                vec![self.ic(p), self.ic(s.len() as i64)]
            }
            LE::Loc(s) => vec![self.cstr(s)],
            LE::Unit => vec![],
            LE::Tup(_, vs) => {
                let mut out = vec![];
                for v in vs {
                    out.extend(self.e(v));
                }
                out
            }
            LE::Field(x, i) if matches!(**x, LE::Index { .. }) => {
                // An element's field: load just that field.
                let LE::Index { arr, idx, check } = &**x else { unreachable!() };
                let at = self.ty(arr);
                let et = elem(&at).clone();
                let a = self.e(arr);
                let iv = self.e1(idx);
                let addr = self.elem_addr(&a, iv, &et, check.as_deref());
                let LTy::Tup(ts) = &et else { unreachable!() };
                let off = field_offset(&et, *i) as i32;
                self.load(&ts[*i].clone(), addr, off)
            }
            LE::Field(x, i) => {
                let LTy::Tup(ts) = self.ty(x) else { unreachable!() };
                let start: usize = ts[..*i].iter().map(nflat).sum();
                let n = nflat(&ts[*i]);
                self.e(x)[start..start + n].to_vec()
            }
            LE::Arith(op, a, b, ovf) => vec![self.arith(*op, a, b, ovf)],
            LE::PArith(op, a, b) => {
                let p = LTy::PInt;
                match op {
                    Op::Add => self.pint_call(rt::alxj_p_add, Some(&p), &[a, b], None),
                    Op::Sub => self.pint_call(rt::alxj_p_sub, Some(&p), &[a, b], None),
                    Op::Mul => self.pint_call(rt::alxj_p_mul, Some(&p), &[a, b], None),
                    Op::Div => self.pint_call(rt::alxj_p_div, Some(&p), &[a, b], Some("div")),
                    Op::Rem => self.pint_call(rt::alxj_p_rem, Some(&p), &[a, b], Some("rem")),
                    Op::Pow => self.pint_call(rt::alxj_p_pow, Some(&p), &[a, b], Some("pow")),
                    _ => {
                        let c = self.pint_call(rt::alxj_p_cmp, None, &[a, b], None)[0];
                        vec![self.b.ins().icmp_imm(Self::icmp_of(*op), c, 0)]
                    }
                }
            }
            LE::Cmp(op, a, b, t) => {
                if matches!(op, Op::And | Op::Or) {
                    return vec![self.short_circuit(*op == Op::And, a, b)];
                }
                match t {
                    LTy::Str => {
                        if let (Op::Eq | Op::Ne, Some(x)) = (op, crate::cgen::palindrome_test(a, b)) {
                            let v = self.e(x);
                            let p = self.spill(&LTy::Str, &v);
                            let r = self.call_rt(rt::alxj_str_is_pal, &[p], true).unwrap();
                            let cc = if *op == Op::Eq { IntCC::NotEqual } else { IntCC::Equal };
                            return vec![self.b.ins().icmp_imm(cc, r, 0)];
                        }
                        let (av, bv) = (self.e(a), self.e(b));
                        let (pa, pb) = (self.spill(&LTy::Str, &av), self.spill(&LTy::Str, &bv));
                        if matches!(op, Op::Eq | Op::Ne) {
                            let r = self.call_rt(rt::alxj_str_eq, &[pa, pb], true).unwrap();
                            let cc = if *op == Op::Eq { IntCC::NotEqual } else { IntCC::Equal };
                            vec![self.b.ins().icmp_imm(cc, r, 0)]
                        } else {
                            let r = self.call_rt(rt::alxj_str_cmp, &[pa, pb], true).unwrap();
                            vec![self.b.ins().icmp_imm(Self::icmp_of(*op), r, 0)]
                        }
                    }
                    LTy::PInt => {
                        let c = self.pint_call(rt::alxj_p_cmp, None, &[a, b], None)[0];
                        vec![self.b.ins().icmp_imm(Self::icmp_of(*op), c, 0)]
                    }
                    LTy::F64 => {
                        let (av, bv) = (self.e1(a), self.e1(b));
                        let cc = match op {
                            Op::Eq => FloatCC::Equal,
                            Op::Ne => FloatCC::NotEqual,
                            Op::Lt => FloatCC::LessThan,
                            Op::Le => FloatCC::LessThanOrEqual,
                            Op::Gt => FloatCC::GreaterThan,
                            _ => FloatCC::GreaterThanOrEqual,
                        };
                        vec![self.b.ins().fcmp(cc, av, bv)]
                    }
                    _ => {
                        let (av, bv) = (self.e1(a), self.e1(b));
                        vec![self.b.ins().icmp(Self::icmp_of(*op), av, bv)]
                    }
                }
            }
            LE::Neg(x, ovf) => {
                let v = self.e1(x);
                if let Ovf::Panic(loc) = ovf {
                    let mn = self.b.ins().icmp_imm(IntCC::Equal, v, i64::MIN);
                    self.overflow_if(mn, &loc.clone());
                }
                vec![self.b.ins().ineg(v)]
            }
            LE::Not(x) => {
                let v = self.e1(x);
                vec![self.b.ins().icmp_imm(IntCC::Equal, v, 0)]
            }
            LE::Cond(c, a, b) => {
                let t = self.ty(a);
                let cv = self.e1(c);
                let (tb, eb, join) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
                let outs: Vec<Value> = lay(&t).fields.iter().map(|(_, ty)| self.b.append_block_param(join, *ty)).collect();
                self.b.ins().brif(cv, tb, &[], eb, &[]);
                self.b.switch_to_block(tb);
                let av = self.e(a);
                self.b.ins().jump(join, &bargs(&av));
                self.b.switch_to_block(eb);
                let bv = self.e(b);
                self.b.ins().jump(join, &bargs(&bv));
                self.b.switch_to_block(join);
                outs
            }
            LE::Call(f, args) => {
                let (id, _) = self.d.funcs[f.as_str()];
                let mut av = vec![];
                for a in args {
                    av.extend(self.e(a));
                }
                let fr = self.fref(id);
                let call = self.b.ins().call(fr, &av);
                self.b.inst_results(call).to_vec()
            }
            LE::Rt(r, args) => self.rt_expr(*r, args),
            LE::Index { arr, idx, check } => {
                let et = elem(&self.ty(arr)).clone();
                let a = self.e(arr);
                let i = self.e1(idx);
                let addr = self.elem_addr(&a, i, &et, check.as_deref());
                self.load(&et, addr, 0)
            }
            LE::Len(x) => vec![self.e(x)[1]],
            LE::ArrLit(t, vs) => {
                let esz = lay(t).size as i64;
                let (n, ez) = (self.ic(vs.len() as i64), self.ic(esz));
                let p = self.call_rt(rt::alxj_arr_alloc, &[n, ez], true).unwrap();
                for (k, v) in vs.iter().enumerate() {
                    let x = self.e(v);
                    self.store(t, &x, p, (k as i64 * esz) as i32);
                }
                vec![p, n, n]
            }
            LE::ArrNew(t, n, fill, loc) => {
                let nv = self.e1(n);
                let fv = self.e(fill);
                let fp = self.spill(t, &fv);
                let ez = self.ic(lay(t).size as i64);
                let l = self.cstr(loc);
                let p = self.call_rt(rt::alxj_arr_new, &[nv, fp, ez, l], true).unwrap();
                vec![p, nv, nv]
            }
            LE::ArrWithCap(t, n) => {
                let nv = self.e1(n);
                let ez = self.ic(lay(t).size as i64);
                let p = self.call_rt(rt::alxj_arr_alloc, &[nv, ez], true).unwrap();
                let z = self.ic(0);
                let neg = self.b.ins().icmp_imm(IntCC::SignedLessThan, nv, 0);
                let cap = self.b.ins().select(neg, z, nv);
                vec![p, z, cap]
            }
            LE::Slice(t, a, start, len) => {
                let esz = lay(elem(t)).size as i64;
                let av = self.e(a);
                let s = self.e1(start);
                let l = self.e1(len);
                let off = self.b.ins().imul_imm(s, esz);
                let p = self.b.ins().iadd(av[0], off);
                let z = self.ic(0);
                vec![p, l, z]
            }
            LE::Range(lo, hi, ex) => {
                let (l, h) = (self.e1(lo), self.e1(hi));
                vec![l, h, self.b.ins().iconst(I8, *ex as i64)]
            }
            LE::RangeField(r, k) => vec![self.e(r)[*k as usize]],
            LE::GenNew(id, vals) => {
                let (fid, g) = self.d.gens[id];
                let (offs, size) = gen_offsets(&g.func);
                let sz = self.ic(size as i64);
                let p = self.call_rt(rt::alxj_zalloc, &[sz], true).unwrap();
                let fr = self.fref(fid);
                let fa = self.b.ins().func_addr(I64, fr);
                self.b.ins().store(MemFlagsData::trusted(), fa, p, 0);
                for (k, v) in g.captures.iter().enumerate() {
                    let x = self.e(&vals[k]);
                    self.store(&g.func.vars[*v].ty, &x, p, offs[*v] as i32);
                }
                vec![p]
            }
            LE::ToP(x) => {
                let v = self.e1(x);
                vec![v, self.ic(0)]
            }
        }
    }

    fn rt_expr(&mut self, r: Rt, args: &[LE]) -> Vec<Value> {
        let s = LTy::Str;
        match r {
            Rt::IntToS => {
                let v = self.e1(&args[0]);
                let out = self.slot(16);
                self.call_rt(rt::alxj_int_to_s, &[out, v], false);
                self.load(&s, out, 0)
            }
            Rt::PIntToS => self.pint_call(rt::alxj_p_to_s, Some(&s), &[&args[0]], None),
            Rt::StrRev => self.pint_call(rt::alxj_str_rev, Some(&s), &[&args[0]], None),
            Rt::StrDelete => self.pint_call(rt::alxj_str_delete, Some(&s), &[&args[0], &args[1]], None),
            Rt::StrSplit => self.pint_call(rt::alxj_str_split, Some(&LTy::Arr(Box::new(LTy::Str))), &[&args[0], &args[1]], None),
            Rt::StrToI => self.pint_call(rt::alxj_str_to_i, None, &[&args[0]], None),
            Rt::StrChar => {
                let sv = self.e(&args[0]);
                let p = self.spill(&s, &sv);
                let i = self.e1(&args[1]);
                vec![self.call_rt(rt::alxj_str_charlen, &[p, i], true).unwrap()]
            }
            Rt::StrByte => {
                let sv = self.e(&args[0]);
                let i = self.e1(&args[1]);
                if matches!(args[2], LE::I(0)) {
                    let addr = self.b.ins().iadd(sv[0], i);
                    vec![self.b.ins().uload8(I64, MemFlagsData::trusted(), addr, 0)]
                } else {
                    let n = self.e1(&args[2]);
                    let p = self.spill(&s, &sv);
                    let out = self.slot(16);
                    self.call_rt(rt::alxj_str_sub, &[out, p, i, n], false);
                    self.load(&s, out, 0)
                }
            }
            Rt::StrLen => vec![self.e(&args[0])[1]],
            Rt::NDigits => {
                let v = self.e1(&args[0]);
                vec![self.call_rt(rt::alxj_int_ndigits, &[v], true).unwrap()]
            }
            Rt::PNDigits => self.pint_call(rt::alxj_p_ndigits, None, &[&args[0]], None),
            Rt::Isqrt => {
                let v = self.e1(&args[0]);
                let l = self.e1(&args[1]);
                vec![self.call_rt(rt::alx_isqrt, &[v, l], true).unwrap()]
            }
            Rt::Digits => {
                let v = self.e1(&args[0]);
                let l = self.e1(&args[1]);
                let t = LTy::Arr(Box::new(LTy::I64));
                let out = self.slot(24);
                self.call_rt(rt::alxj_digits, &[out, v, l], false);
                self.load(&t, out, 0)
            }
            Rt::PDigits => {
                let t = LTy::Arr(Box::new(LTy::PInt));
                let v = self.e(&args[0]);
                let p = self.spill(&LTy::PInt, &v);
                let l = self.e1(&args[1]);
                let out = self.slot(24);
                self.call_rt(rt::alxj_p_digits, &[out, p, l], false);
                self.load(&t, out, 0)
            }
            Rt::SatAdd => {
                let (a, b) = (self.e1(&args[0]), self.e1(&args[1]));
                let (r, of) = self.b.ins().sadd_overflow(a, b);
                let mx = self.ic(i64::MAX);
                vec![self.b.ins().select(of, mx, r)]
            }
            Rt::ArrCopy => {
                let t = self.ty(&args[0]);
                let esz = self.ic(lay(elem(&t)).size as i64);
                let v = self.e(&args[0]);
                let p = self.spill(&t, &v);
                let out = self.slot(24);
                self.call_rt(rt::alxj_arr_copy, &[out, p, esz], false);
                self.load(&t, out, 0)
            }
            Rt::Even => {
                let v = self.e1(&args[0]);
                let low = self.b.ins().band_imm(v, 1);
                vec![self.b.ins().icmp_imm(IntCC::Equal, low, 0)]
            }
            Rt::PEven => {
                let r = self.pint_call(rt::alxj_p_even, None, &[&args[0]], None)[0];
                vec![self.b.ins().icmp_imm(IntCC::NotEqual, r, 0)]
            }
            Rt::IntToF => {
                let v = self.e1(&args[0]);
                vec![self.b.ins().fcvt_from_sint(F64, v)]
            }
            Rt::FToI => {
                let v = self.e1(&args[0]);
                let l = self.e1(&args[1]);
                vec![self.call_rt(rt::alxj_f_to_i, &[v, l], true).unwrap()]
            }
            Rt::FSqrt => {
                let v = self.e1(&args[0]);
                vec![self.b.ins().sqrt(v)]
            }
            Rt::FAbs => {
                let v = self.e1(&args[0]);
                vec![self.b.ins().fabs(v)]
            }
            Rt::FToS => {
                let v = self.e1(&args[0]);
                let out = self.slot(16);
                self.call_rt(rt::alxj_f_to_s, &[out, v], false);
                self.load(&s, out, 0)
            }
            Rt::FFmt => {
                let v = self.e1(&args[0]);
                let d = self.e1(&args[1]);
                let out = self.slot(16);
                self.call_rt(rt::alxj_f_fmt, &[out, v, d], false);
                self.load(&s, out, 0)
            }
            Rt::StrCat => {
                let parts = self.slot(16 * args.len() as u32);
                for (k, a) in args.iter().enumerate() {
                    let v = self.e(a);
                    self.store(&s, &v, parts, 16 * k as i32);
                }
                let n = self.ic(args.len() as i64);
                let out = self.slot(16);
                self.call_rt(rt::alxj_str_cat, &[out, parts, n], false);
                self.load(&s, out, 0)
            }
            Rt::PToI64 => {
                let v = self.e(&args[0]);
                let p = self.spill(&LTy::PInt, &v);
                let l = self.e1(&args[1]);
                vec![self.call_rt(rt::alxj_p_to_i64, &[p, l], true).unwrap()]
            }
        }
    }
}

/// Byte offset of field `k` of a tuple type (C layout).
fn field_offset(t: &LTy, k: usize) -> u32 {
    let LTy::Tup(ts) = t else { panic!("jit: field of {t:?}") };
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
