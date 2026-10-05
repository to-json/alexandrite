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
use cranelift_codegen::ir::AtomicRmwOp;
use cranelift_codegen::ir::types::{F64, I8, I64};
use cranelift_codegen::ir::{AbiParam, Block, BlockArg, FuncRef, InstBuilder, MemFlagsData, SigRef, Signature, StackSlot, StackSlotData, StackSlotKind, TrapCode, Type, Value};
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
        alxj_region_cur, alxj_region_mark, alxj_region_mark_larges, alxj_region_reset, alxj_region_enter, alxj_region_exit, alxj_region_use, alxj_region_set, alxj_region_program, alxj_region_of, alxj_region_new_child, alxj_region_free, alxj_region_bytes,
        alxj_alloc, alxj_zalloc, alxj_arr_alloc, alxj_arr_grow, alxj_arr_new, alxj_arr_copy,
        alxj_int_to_s, alxj_str_rev, alxj_str_delete, alxj_str_split, alxj_str_join, alxj_str_to_i, alxj_str_index, alxj_str_charlen, alxj_str_sub,
        alxj_str_eq, alxj_str_cmp, alxj_str_is_pal, alxj_int_ndigits, alxj_digits,
        alxj_puts_str, alxj_print_str, alxj_puts_bool, alxj_puts_unit, alxj_pmap, alxj_pmap_try, alxj_finish,
        alxj_p_add, alxj_p_sub, alxj_p_mul, alxj_p_div, alxj_p_rem, alxj_p_pow, alxj_p_cmp, alxj_p_even, alxj_p_to_i64,
        alxj_p_to_s, alxj_p_from_str, alxj_p_ndigits, alxj_p_digits, alxj_puts_pint,
        alxj_puts_f64, alxj_f_to_s, alxj_f_fmt, alxj_f_fmt_e, alxj_str_pad, alxj_str_quote, alxj_f_to_i, alxj_str_cat,
        alxj_puts_u64, alxj_u64_to_s, alxj_int_fmt, alxj_f_to_u64, alxj_rune_to_s, alxj_str_from_bytes,
        alxj_die_str, alxj_panic_str, alxj_exit, alxj_now_ns, alxj_cap_begin, alxj_cap_end, alxj_file_status, alxj_file_read_or_empty,
        alxj_ffi_enter, alxj_ffi_save_errno, alxj_cstr_new, alxj_cstr_free, alxj_errno, alxj_strerror, alxj_str_from_cstr, alxj_str_from_ptr,
        alx_sys_open, alx_sys_fcntl, alx_sys_const, alx_sys_stat, alx_sys_fstat, alx_sys_dir_open, alx_sys_dir_next, alx_sys_dir_close, alx_environ, alx_sys_spawn, alx_sys_wait, alx_sys_pipe, alx_sys_exec, alx_sys_poll2, alx_argc, alx_argv, alx_sleep_ns, alx_wall_ns, alx_mono_ns, alx_local_offset, alx_local_zone,
        alx_fd_wait, alx_fd_close, alx_sock_listen, alx_sock_accept, alx_sock_connect, alx_sock_error, alx_sock_local_addr, alx_sock_peer_addr, alx_sock_set_nodelay, alx_sock_shutdown, alx_sock_lookup, alx_mem_held, alx_mem_peak, alx_count_allocs, alx_alloc_count,
        alx_sig_watch, alx_sig_unwatch, alx_sig_reset, alx_sig_ignored, alx_user_lookup, alx_user_groups,
        alxj_spawn, alxj_task_wait, alxj_lock_new, alxj_lock, alxj_unlock, alxj_lock_poisoned, alxj_lock_clear_poison, alxj_atomic_new, alxj_chan_new, alxj_chan_len, alxj_chan_send, alxj_chan_recv, alxj_chan_close, alxj_select,
    );
}
type RtFn = unsafe extern "C" fn();

/// libm, called by pointer from JIT code (math block).
mod libm {
    unsafe extern "C" {
        pub fn sin(x: f64) -> f64;
        pub fn cos(x: f64) -> f64;
        pub fn tan(x: f64) -> f64;
        pub fn asin(x: f64) -> f64;
        pub fn acos(x: f64) -> f64;
        pub fn atan(x: f64) -> f64;
        pub fn atan2(x: f64, y: f64) -> f64;
        pub fn sinh(x: f64) -> f64;
        pub fn cosh(x: f64) -> f64;
        pub fn tanh(x: f64) -> f64;
        pub fn asinh(x: f64) -> f64;
        pub fn acosh(x: f64) -> f64;
        pub fn atanh(x: f64) -> f64;
        pub fn exp(x: f64) -> f64;
        pub fn exp2(x: f64) -> f64;
        pub fn expm1(x: f64) -> f64;
        pub fn log(x: f64) -> f64;
        pub fn log2(x: f64) -> f64;
        pub fn log10(x: f64) -> f64;
        pub fn log1p(x: f64) -> f64;
        pub fn pow(x: f64, y: f64) -> f64;
        pub fn cbrt(x: f64) -> f64;
        pub fn hypot(x: f64, y: f64) -> f64;
        pub fn round(x: f64) -> f64;
        pub fn fmod(x: f64, y: f64) -> f64;
        pub fn remainder(x: f64, y: f64) -> f64;
        pub fn nextafter(x: f64, y: f64) -> f64;
        pub fn erf(x: f64) -> f64;
        pub fn erfc(x: f64) -> f64;
        pub fn tgamma(x: f64) -> f64;
        pub fn lgamma(x: f64) -> f64;
    }
}

/// The libm function behind a `MathFn` that Cranelift has no instruction for.
fn libm_ptr(f: MathFn) -> RtFn {
    use MathFn::*;
    let p = match f {
        Sin => libm::sin as usize,
        Cos => libm::cos as usize,
        Tan => libm::tan as usize,
        Asin => libm::asin as usize,
        Acos => libm::acos as usize,
        Atan => libm::atan as usize,
        Atan2 => libm::atan2 as usize,
        Sinh => libm::sinh as usize,
        Cosh => libm::cosh as usize,
        Tanh => libm::tanh as usize,
        Asinh => libm::asinh as usize,
        Acosh => libm::acosh as usize,
        Atanh => libm::atanh as usize,
        Exp => libm::exp as usize,
        Exp2 => libm::exp2 as usize,
        Expm1 => libm::expm1 as usize,
        Log => libm::log as usize,
        Log2 => libm::log2 as usize,
        Log10 => libm::log10 as usize,
        Log1p => libm::log1p as usize,
        Pow => libm::pow as usize,
        Cbrt => libm::cbrt as usize,
        Hypot => libm::hypot as usize,
        Round => libm::round as usize,
        Fmod => libm::fmod as usize,
        Remainder => libm::remainder as usize,
        Nextafter => libm::nextafter as usize,
        Erf => libm::erf as usize,
        Erfc => libm::erfc as usize,
        Gamma => libm::tgamma as usize,
        Lgamma => libm::lgamma as usize,
        Floor | Ceil | Trunc | RoundEven | Fma | Copysign => unreachable!("native"),
    };
    unsafe { std::mem::transmute::<usize, RtFn>(p) }
}


/// C layout of a type: size, alignment, its scalars (offset, SSA type) and
/// how each sits in memory (narrow integers are stored at their own width
/// but held as I64 in registers).
struct Lay {
    size: u32,
    align: u32,
    fields: Vec<(u32, Type)>,
    mem: Vec<Mem>,
}

#[derive(Clone, Copy, PartialEq)]
enum Mem {
    Full,
    /// (bytes, signed)
    Narrow(u8, bool),
}

fn lay(t: &LTy) -> Lay {
    let words = |n: u32| Lay { size: 8 * n, align: 8, fields: (0..n).map(|i| (8 * i, I64)).collect(), mem: vec![Mem::Full; n as usize] };
    match t {
        LTy::Region => words(1),
        LTy::Task(_) | LTy::Chan(_) | LTy::Lock | LTy::Atomic => words(1),
        LTy::I64 | LTy::Gen(_) => words(1),
        LTy::F64 => Lay { size: 8, align: 8, fields: vec![(0, F64)], mem: vec![Mem::Full] },
        LTy::IntK(k) if k.bits() == 64 => words(1),
        LTy::IntK(k) => {
            let n = (k.bits() / 8) as u8;
            Lay { size: n as u32, align: n as u32, fields: vec![(0, I64)], mem: vec![Mem::Narrow(n, k.signed())] }
        }
        LTy::PInt | LTy::Str => words(2),
        LTy::Arr(_) => words(3),
        LTy::Bool => Lay { size: 1, align: 1, fields: vec![(0, I8)], mem: vec![Mem::Full] },
        LTy::Unit => Lay { size: 1, align: 1, fields: vec![], mem: vec![] },
        LTy::Range => Lay { size: 24, align: 8, fields: vec![(0, I64), (8, I64), (16, I8)], mem: vec![Mem::Full; 3] },
        LTy::Tup(ts) => {
            let (mut off, mut align, mut fields, mut mem) = (0u32, 1u32, vec![], vec![]);
            for t in ts {
                let l = lay(t);
                off = off.next_multiple_of(l.align);
                fields.extend(l.fields.iter().map(|(o, ty)| (off + o, *ty)));
                mem.extend(l.mem);
                off += l.size;
                align = align.max(l.align);
            }
            Lay { size: off.next_multiple_of(align).max(1), align, fields, mem }
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
    Main,
    Gen,
    Worker,
}

struct Decls<'p> {
    funcs: HashMap<&'p str, (FuncId, &'p LFunc)>,
    gens: HashMap<usize, (FuncId, &'p LGen)>,
    workers: HashMap<usize, FuncId>,
    /// A worker's input and result types (for `spawn`).
    worker_in: HashMap<usize, LTy>,
    worker_out: HashMap<usize, LTy>,
    /// Each global's type and address: memory of this process, never freed
    /// (the program runs here, once).
    globals: Vec<(LTy, i64)>,
    /// The program's C functions (address, signature), by `LE::Ffi` index.
    externs: Vec<(usize, FfiSig)>,
}

/// The address of a C symbol: this executable's own runtime shims, else the
/// process's global symbols (libc and anything loaded).
fn resolve_c_symbol(name: &str) -> Option<usize> {
    let own = match name {
        "alx_sys_open" => Some(rt::alx_sys_open as usize),
        "alx_sys_fcntl" => Some(rt::alx_sys_fcntl as usize),
        "alx_sys_const" => Some(rt::alx_sys_const as usize),
        "alx_sys_stat" => Some(rt::alx_sys_stat as usize),
        "alx_sys_fstat" => Some(rt::alx_sys_fstat as usize),
        "alx_sys_dir_open" => Some(rt::alx_sys_dir_open as usize),
        "alx_sys_dir_next" => Some(rt::alx_sys_dir_next as usize),
        "alx_sys_dir_close" => Some(rt::alx_sys_dir_close as usize),
        "alx_environ" => Some(rt::alx_environ as usize),
        "alx_sys_spawn" => Some(rt::alx_sys_spawn as usize),
        "alx_sys_wait" => Some(rt::alx_sys_wait as usize),
        "alx_sys_pipe" => Some(rt::alx_sys_pipe as usize),
        "alx_sys_exec" => Some(rt::alx_sys_exec as usize),
        "alx_sys_poll2" => Some(rt::alx_sys_poll2 as usize),
        "alx_argc" => Some(rt::alx_argc as usize),
        "alx_argv" => Some(rt::alx_argv as usize),
        "alx_sleep_ns" => Some(rt::alx_sleep_ns as usize),
        "alx_wall_ns" => Some(rt::alx_wall_ns as usize),
        "alx_mono_ns" => Some(rt::alx_mono_ns as usize),
        "alx_local_offset" => Some(rt::alx_local_offset as usize),
        "alx_local_zone" => Some(rt::alx_local_zone as usize),
        "alx_fd_wait" => Some(rt::alx_fd_wait as usize),
        "alx_fd_close" => Some(rt::alx_fd_close as usize),
        "alx_sock_listen" => Some(rt::alx_sock_listen as usize),
        "alx_sock_accept" => Some(rt::alx_sock_accept as usize),
        "alx_sock_connect" => Some(rt::alx_sock_connect as usize),
        "alx_sock_error" => Some(rt::alx_sock_error as usize),
        "alx_sock_local_addr" => Some(rt::alx_sock_local_addr as usize),
        "alx_sock_peer_addr" => Some(rt::alx_sock_peer_addr as usize),
        "alx_sock_set_nodelay" => Some(rt::alx_sock_set_nodelay as usize),
        "alx_sock_shutdown" => Some(rt::alx_sock_shutdown as usize),
        "alx_sock_lookup" => Some(rt::alx_sock_lookup as usize),
        "alx_sig_watch" => Some(rt::alx_sig_watch as usize),
        "alx_sig_unwatch" => Some(rt::alx_sig_unwatch as usize),
        "alx_sig_reset" => Some(rt::alx_sig_reset as usize),
        "alx_sig_ignored" => Some(rt::alx_sig_ignored as usize),
        "alx_user_lookup" => Some(rt::alx_user_lookup as usize),
        "alx_user_groups" => Some(rt::alx_user_groups as usize),
        "alx_mem_held" => Some(rt::alx_mem_held as usize),
        "alx_mem_peak" => Some(rt::alx_mem_peak as usize),
        "alx_count_allocs" => Some(rt::alx_count_allocs as usize),
        "alx_alloc_count" => Some(rt::alx_alloc_count as usize),
        _ => None,
    };
    if own.is_some() {
        return own;
    }
    let c = CString::new(name).ok()?;
    let mut p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c.as_ptr()) };
    if p.is_null() {
        static DIRS_LOADED: std::sync::Once = std::sync::Once::new();
        DIRS_LOADED.call_once(|| {
            if let Ok(extra) = std::env::var("ALX_LIBS") {
                for l in extra.split(':') {
                    if let Ok(cstr) = CString::new(l) {
                        unsafe {
                            libc::dlopen(cstr.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL);
                        }
                    }
                }
            }
        });
        p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c.as_ptr()) };
    }
    (!p.is_null()).then_some(p as usize)
}

/// The Cranelift type of a C integer kind, and whether it is signed.
fn ffi_int_ty(k: IntKind) -> Type {
    match k.bits() {
        8 => I8,
        16 => cranelift_codegen::ir::types::I16,
        32 => cranelift_codegen::ir::types::I32,
        _ => I64,
    }
}

unsafe extern "C" {
    fn alx_set_args(argc: i32, argv: *const *const std::os::raw::c_char);
}

/// The program's arguments (argv[0] first), for `os.args`.
pub fn set_args(args: &[String]) {
    let cs: Vec<std::ffi::CString> = args.iter().map(|a| std::ffi::CString::new(a.as_str()).unwrap_or_default()).collect();
    let ptrs: Vec<*const std::os::raw::c_char> = cs.iter().map(|c| c.as_ptr()).collect();
    // The runtime keeps the pointers: leak them for the program's lifetime.
    let n = ptrs.len() as i32;
    let ptrs = Box::leak(ptrs.into_boxed_slice());
    std::mem::forget(cs);
    unsafe { alx_set_args(n, ptrs.as_ptr()) }
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

    let mut externs = vec![];
    for x in &p.externs {
        let addr = resolve_c_symbol(&x.sym).ok_or_else(|| format!("undefined extern symbol `{}` (no such C function in this process)", x.sym))?;
        externs.push((addr, x.clone()));
    }
    let globals = p.globals.iter().map(|t| (t.clone(), Box::leak(vec![0u64; lay(t).size.div_ceil(8) as usize].into_boxed_slice()).as_mut_ptr() as i64)).collect();
    let mut d = Decls { funcs: HashMap::new(), gens: HashMap::new(), workers: HashMap::new(), worker_in: HashMap::new(), worker_out: HashMap::new(), externs, globals };
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
        d.worker_in.insert(w.id, w.input.clone());
        d.worker_out.insert(w.id, w.func.ret.clone());
    }

    let mut ctx = m.make_context();
    let mut fbc = FunctionBuilderContext::new();
    let mut strs = Strs::default();
    let mut jobs: Vec<(FuncId, &LFunc, Kind, Option<&LGen>)> = vec![];
    for f in &p.funcs {
        let k = if f.is_main { Kind::Main } else { Kind::Plain };
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
            let mut fx = Fx { b, m: &mut m, d: &d, strs: &mut strs, f, kind, lgen: g, vars: vec![], loops: HashMap::new(), params: vec![], gstate: None, resume: vec![], yields: 0, sigs: HashMap::new(), frefs: HashMap::new(), slot_pool: vec![] };
            fx.body();
            fx.b.seal_all_blocks();
            fx.b.finalize(fcfg);
        }
        let dump = std::env::var("ALX_JIT_DUMP").is_ok_and(|d| f.name.contains(d.as_str()));
        ctx.set_disasm(dump);
        m.define_function(id, &mut ctx).map_err(|e| format!("jit: {}: {e:?}", f.name))?;
        if dump {
            eprintln!("{}\n{}", ctx.func, ctx.compiled_code().and_then(|c| c.vcode.clone()).unwrap_or_default());
        }
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
    for (_, t) in lay(&f.ret).fields {
        s.returns.push(AbiParam::new(t));
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

/// `void worker(const void *in, void *out)`, called by alx_pmap.
fn worker_sig(m: &JITModule) -> Signature {
    let mut s = m.make_signature();
    s.params.extend([AbiParam::new(I64), AbiParam::new(I64)]);
    s
}

/// String literals and locations, leaked: they live as long as the program.
#[derive(Default)]
struct Strs {
    bytes: HashMap<Vec<u8>, usize>,
    cstrs: HashMap<String, usize>,
}

impl Strs {
    fn bytes(&mut self, s: &[u8]) -> i64 {
        *self.bytes.entry(s.to_vec()).or_insert_with(|| Box::leak(s.to_vec().into_boxed_slice()).as_ptr() as usize) as i64
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
    /// Stack slots for spills and runtime out-parameters: every use is over
    /// within one statement, so each statement starts with all of them free
    /// (without reuse, deep call chains overflowed a task's stack).
    slot_pool: Vec<(StackSlot, u32, bool)>,
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

    /// A call to a C function with the platform's C ABI: narrow integers are
    /// passed at their own width and extended as the ABI requires (`sext` /
    /// `uext`; Apple arm64 callers extend to 32 bits), results are extended
    /// back to a register. Strings are NUL-terminated copies freed after the
    /// call; errno is saved right after it.
    fn ffi_call(&mut self, i: usize, args: &[LE]) -> Vec<Value> {
        let (addr, x) = self.d.externs[i].clone();
        let vals: Vec<Vec<Value>> = args.iter().map(|a| self.e(a)).collect();
        let mut sig = self.m.make_signature();
        let (mut cargs, mut frees) = (vec![], vec![]);
        for (t, v) in x.params.iter().zip(&vals) {
            match t {
                FfiTy::Str => {
                    let p = self.spill(&LTy::Str, v);
                    let c = self.call_rt(rt::alxj_cstr_new, &[p], true).unwrap();
                    frees.push(c);
                    sig.params.push(AbiParam::new(I64));
                    cargs.push(c);
                }
                FfiTy::Bytes | FfiTy::Ptr | FfiTy::Int(IntKind::I64 | IntKind::U64) => {
                    sig.params.push(AbiParam::new(I64));
                    cargs.push(v[0]);
                }
                FfiTy::Int(k) => {
                    let ty = ffi_int_ty(*k);
                    let r = self.b.ins().ireduce(ty, v[0]);
                    sig.params.push(if k.signed() { AbiParam::new(ty).sext() } else { AbiParam::new(ty).uext() });
                    cargs.push(r);
                }
                FfiTy::F64 => {
                    sig.params.push(AbiParam::new(F64));
                    cargs.push(v[0]);
                }
                FfiTy::Bool => {
                    sig.params.push(AbiParam::new(I8).uext());
                    cargs.push(v[0]);
                }
                FfiTy::Unit => unreachable!(),
            }
        }
        let ret_ty = match x.ret {
            FfiTy::Unit => None,
            FfiTy::Ptr | FfiTy::Int(IntKind::I64 | IntKind::U64) => Some(I64),
            FfiTy::Int(k) => Some(ffi_int_ty(k)),
            FfiTy::F64 => Some(F64),
            FfiTy::Bool => Some(I8),
            _ => unreachable!(),
        };
        if let Some(t) = ret_ty {
            sig.returns.push(AbiParam::new(t));
        }
        let sigref = self.b.import_signature(sig);
        self.call_rt(rt::alxj_ffi_enter, &[], false);
        let fp = self.ic(addr as i64);
        let call = self.b.ins().call_indirect(sigref, fp, &cargs);
        let r = ret_ty.map(|_| self.b.inst_results(call)[0]);
        self.call_rt(rt::alxj_ffi_save_errno, &[], false);
        for c in frees {
            self.call_rt(rt::alxj_cstr_free, &[c], false);
        }
        match (x.ret, r) {
            (FfiTy::Int(k), Some(r)) if k.bits() < 64 => {
                vec![if k.signed() { self.b.ins().sextend(I64, r) } else { self.b.ins().uextend(I64, r) }]
            }
            (_, Some(r)) => vec![r],
            (_, None) => vec![],
        }
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
        let size = size.max(1).next_multiple_of(8);
        let ss = match self.slot_pool.iter_mut().find(|(_, n, used)| !*used && *n >= size) {
            Some(e) => {
                e.2 = true;
                e.0
            }
            None => {
                let ss = self.b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, size, 3));
                self.slot_pool.push((ss, size, true));
                ss
            }
        };
        self.b.ins().stack_addr(I64, ss, 0)
    }

    fn store(&mut self, t: &LTy, vals: &[Value], addr: Value, off: i32) {
        let l = lay(t);
        for (((o, _), m), v) in l.fields.iter().zip(&l.mem).zip(vals) {
            let at = off + *o as i32;
            let f = MemFlagsData::trusted();
            match m {
                Mem::Full => self.b.ins().store(f, *v, addr, at),
                Mem::Narrow(1, _) => self.b.ins().istore8(f, *v, addr, at),
                Mem::Narrow(2, _) => self.b.ins().istore16(f, *v, addr, at),
                Mem::Narrow(_, _) => self.b.ins().istore32(f, *v, addr, at),
            };
        }
    }

    fn load(&mut self, t: &LTy, addr: Value, off: i32) -> Vec<Value> {
        let l = lay(t);
        l.fields
            .iter()
            .zip(&l.mem)
            .map(|((o, ty), m)| {
                let (at, f) = (off + *o as i32, MemFlagsData::trusted());
                match m {
                    Mem::Full => self.b.ins().load(*ty, f, addr, at),
                    Mem::Narrow(1, true) => self.b.ins().sload8(I64, f, addr, at),
                    Mem::Narrow(1, false) => self.b.ins().uload8(I64, f, addr, at),
                    Mem::Narrow(2, true) => self.b.ins().sload16(I64, f, addr, at),
                    Mem::Narrow(2, false) => self.b.ins().uload16(I64, f, addr, at),
                    Mem::Narrow(_, true) => self.b.ins().sload32(f, addr, at),
                    Mem::Narrow(_, false) => self.b.ins().uload32(f, addr, at),
                }
            })
            .collect()
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
            Kind::Worker => {
                self.b.ins().return_(&[]);
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

    // ---------- statements ----------

    fn stmt(&mut self, s: &LS) {
        for e in self.slot_pool.iter_mut() {
            e.2 = false;
        }
        match s {
            LS::SetGlobal(k, v) => {
                let vals = self.e(v);
                let (t, at) = self.d.globals[*k].clone();
                let addr = self.ic(at);
                self.store(&t, &vals, addr, 0);
            }
            LS::RegionFree(r) => {
                let r = self.e1(r);
                self.call_rt(rt::alxj_region_free, &[r], false);
            }
            LS::RegionEnter { region, saved } => {
                let sv = self.call_rt(rt::alxj_region_cur, &[], true).unwrap();
                self.set(*saved, &[sv]);
                let r = self.call_rt(rt::alxj_region_enter, &[], true).unwrap();
                self.set(*region, &[r]);
            }
            LS::RegionExit { region, saved } => {
                let (r, sv) = (self.get(*region), self.get(*saved));
                self.call_rt(rt::alxj_region_exit, &[r[0], sv[0]], false);
            }
            LS::RegionUse { region, saved } => {
                let r = self.e1(region);
                let sv = self.call_rt(rt::alxj_region_use, &[r], true).unwrap();
                self.set(*saved, &[sv]);
            }
            LS::RegionRestore(saved) => {
                let sv = self.get(*saved);
                self.call_rt(rt::alxj_region_set, &[sv[0]], false);
            }
            LS::Spawn { dst, worker, env } => {
                let (it, ot) = (self.d.worker_in[worker].clone(), self.d.worker_out[worker].clone());
                let x = self.e(env);
                let envp = self.spill(&it, &x);
                let (ie, oe) = (self.ic(lay(&it).size as i64), self.ic(lay(&ot).size as i64));
                let fr = self.fref(self.d.workers[worker]);
                let wf = self.b.ins().func_addr(I64, fr);
                let h = self.call_rt(rt::alxj_spawn, &[wf, envp, ie, oe], true).unwrap();
                self.set(*dst, &[h]);
            }
            LS::Wait { task, ok, val, msg } => {
                let t = self.e1(task);
                let vt = self.f.vars[*val].ty.clone();
                // Preset both buffers with the current values: the runtime
                // writes only the one that applies.
                let (vv, mv) = (self.get(*val), self.get(*msg));
                let vp = self.spill(&vt, &vv);
                let mp = self.spill(&LTy::Str, &mv);
                let r = self.call_rt(rt::alxj_task_wait, &[t, vp, mp], true).unwrap();
                let okv = self.b.ins().ireduce(I8, r);
                self.set(*ok, &[okv]);
                let nv = self.load(&vt, vp, 0);
                self.set(*val, &nv);
                let nm = self.load(&LTy::Str, mp, 0);
                self.set(*msg, &nm);
            }
            LS::ChanSend { ch, val, loc } => {
                let LTy::Chan(et) = self.ty(ch) else { panic!("jit: send on a non-channel") };
                let c = self.e1(ch);
                let x = self.e(val);
                let p = self.spill(&et, &x);
                let l = self.cstr(loc);
                self.call_rt(rt::alxj_chan_send, &[c, p, l], false);
            }
            LS::ChanRecv { ch, ok, val } => {
                let c = self.e1(ch);
                let vt = self.f.vars[*val].ty.clone();
                let vv = self.get(*val);
                let p = self.spill(&vt, &vv);
                let r = self.call_rt(rt::alxj_chan_recv, &[c, p], true).unwrap();
                let okv = self.b.ins().ireduce(I8, r);
                self.set(*ok, &[okv]);
                let nv = self.load(&vt, p, 0);
                self.set(*val, &nv);
            }
            LS::ChanClose { ch, loc } => {
                let c = self.e1(ch);
                let l = self.cstr(loc);
                self.call_rt(rt::alxj_chan_close, &[c, l], false);
            }
            LS::Lock(l, loc) => {
                let l = self.e1(l);
                let at = self.cstr(loc);
                self.call_rt(rt::alxj_lock, &[l, at], false);
            }
            LS::LockClearPoison(l) => {
                let l = self.e1(l);
                self.call_rt(rt::alxj_lock_clear_poison, &[l], false);
            }
            LS::Unlock(l) => {
                let l = self.e1(l);
                self.call_rt(rt::alxj_unlock, &[l], false);
            }
            LS::AtomicStore(a, v) => {
                let (a, v) = (self.e1(a), self.e1(v));
                self.b.ins().atomic_store(MemFlagsData::trusted(), v, a);
            }
            LS::Select { cases, default, dst } => {
                // AlxSelCase { ch, buf, is_send, ok }: four words each.
                let n = cases.len() as i64;
                let arr = self.slot((32 * cases.len().max(1)) as u32);
                let mf = MemFlagsData::trusted();
                let mut recvs = vec![];
                for (i, c) in cases.iter().enumerate() {
                    let base = 32 * i as i32;
                    match c {
                        SelCase::Send { ch, val } => {
                            let LTy::Chan(et) = self.ty(ch) else { panic!("jit: select send on a non-channel") };
                            let cv = self.e1(ch);
                            let x = self.e(val);
                            let p = self.spill(&et, &x);
                            let one = self.ic(1);
                            let zero = self.ic(0);
                            self.b.ins().store(mf, cv, arr, base);
                            self.b.ins().store(mf, p, arr, base + 8);
                            self.b.ins().store(mf, one, arr, base + 16);
                            self.b.ins().store(mf, zero, arr, base + 24);
                        }
                        SelCase::Recv { ch, ok, val } => {
                            let cv = self.e1(ch);
                            let vt = self.f.vars[*val].ty.clone();
                            let vv = self.get(*val);
                            let p = self.spill(&vt, &vv);
                            let okv = self.get(*ok)[0];
                            let okw = self.b.ins().uextend(I64, okv);
                            let zero = self.ic(0);
                            self.b.ins().store(mf, cv, arr, base);
                            self.b.ins().store(mf, p, arr, base + 8);
                            self.b.ins().store(mf, zero, arr, base + 16);
                            self.b.ins().store(mf, okw, arr, base + 24);
                            recvs.push((base, p, *ok, *val, vt));
                        }
                    }
                }
                let (nv, dv) = (self.ic(n), self.ic(*default as i64));
                let l = self.cstr("select");
                let r = self.call_rt(rt::alxj_select, &[arr, nv, dv, l], true).unwrap();
                self.set(*dst, &[r]);
                for (base, p, ok, val, vt) in recvs {
                    let okw = self.b.ins().load(I64, mf, arr, base + 24);
                    let okv = self.b.ins().ireduce(I8, okw);
                    self.set(ok, &[okv]);
                    let nv = self.load(&vt, p, 0);
                    self.set(val, &nv);
                }
            }
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
                // A slice's cap is 0: pushing to one always copies.
                let full = self.b.ins().icmp(IntCC::SignedGreaterThanOrEqual, a[1], a[2]);
                let (grow, cont) = (self.b.create_block(), self.b.create_block());
                self.b.ins().brif(full, grow, &[], cont, &[]);
                self.b.switch_to_block(grow);
                let dbl = self.b.ins().ishl_imm_s(a[1], 1);
                let four = self.ic(4);
                let nc = self.b.ins().select(a[1], dbl, four);
                let ez = self.ic(esz);
                let np = self.call_rt(rt::alxj_arr_grow, &[a[0], a[1], nc, ez], true).unwrap();
                self.set(*v, &[np, a[1], nc]);
                self.enter(cont);
                let a = self.get(*v);
                let off = self.b.ins().imul_imm_s(a[1], esz);
                let addr = self.b.ins().iadd(a[0], off);
                self.store(&et, &x, addr, 0);
                let n1 = self.b.ins().iadd_imm_s(a[1], 1);
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
                    (Kind::Worker, v) => {
                        if let Some(v) = v {
                            let x = self.e(v);
                            let rt = self.f.ret.clone();
                            self.store(&rt, &x, self.params[1], 0);
                        }
                        self.b.ins().return_(&[]);
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
            LS::Pmap { dst, arr, worker, err } => {
                let at = self.ty(arr);
                let a = self.e(arr);
                let in_esz = lay(elem(&at)).size as i64;
                let out_t = elem(&self.f.vars[*dst].ty).clone();
                let out_esz = lay(&out_t).size as i64;
                let (ie, oe) = (self.ic(in_esz), self.ic(out_esz));
                let outp = self.call_rt(rt::alxj_arr_alloc, &[a[1], oe], true).unwrap();
                let wid = self.d.workers[worker];
                let fr = self.fref(wid);
                let wf = self.b.ins().func_addr(I64, fr);
                match err {
                    None => {
                        self.call_rt(rt::alxj_pmap, &[a[0], a[1], ie, outp, oe, wf], false);
                    }
                    Some(e) => {
                        let rt_ = self.f.vars[*e].ty.clone();
                        let rl = lay(&rt_);
                        let ep = self.slot(rl.size);
                        let (rs, vo) = (self.ic(rl.size as i64), self.ic(field_offset(&rt_, 1) as i64));
                        self.call_rt(rt::alxj_pmap_try, &[a[0], a[1], ie, outp, oe, wf, rs, vo, ep], false);
                        let vals = self.load(&rt_, ep, 0);
                        self.set(*e, &vals);
                    }
                }
                self.set(*dst, &[outp, a[1], a[1]]);
            }
            LS::Print(e) => {
                let x = self.e(e);
                let p = self.spill(&LTy::Str, &x);
                self.call_rt(rt::alxj_print_str, &[p], false);
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
                    LTy::IntK(IntKind::U64) => {
                        self.call_rt(rt::alxj_puts_u64, &x, false);
                    }
                    LTy::IntK(_) => {
                        self.call_rt(rt::alx_puts_i64, &x, false);
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
            LS::Die(e) => {
                let v = self.e(e);
                let ps = self.spill(&LTy::Str, &v);
                self.call_rt(rt::alxj_die_str, &[ps], false);
                self.b.ins().trap(TrapCode::unwrap_user(1));
                self.fresh();
            }
            LS::Panic(msg, loc) => self.panic(msg, loc),
            LS::Exit(e) => {
                let v = self.e(e);
                self.call_rt(rt::alxj_exit, &v, false);
            }
            LS::PanicStr(e) => {
                let v = self.e(e);
                let ps = self.spill(&LTy::Str, &v);
                self.call_rt(rt::alxj_panic_str, &[ps], false);
                self.b.ins().trap(TrapCode::unwrap_user(1));
                self.fresh();
            }
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
            LE::Global(k) => self.d.globals[*k].0.clone(),
            LE::RegionNew(_) => LTy::Region,
            LE::RegionBytes(_) => LTy::I64,
            LE::RegionOf(_) => LTy::Region,
            LE::RegionProgram => LTy::Region,
            LE::ChanNew(t, _) => LTy::Chan(Box::new(t.clone())),
            LE::ChanLen(_) => LTy::I64,
            LE::LockPoisoned(_) => LTy::Bool,
            LE::LockNew => LTy::Lock,
            LE::NullTask(t) => t.clone(),
            LE::AtomicNew(_) => LTy::Atomic,
            LE::AtomicLoad(_) | LE::AtomicRmw(..) => LTy::I64,
            LE::AtomicCas(..) => LTy::Bool,
            LE::Var(v) => self.f.vars[*v].ty.clone(),
            LE::I(_) | LE::Loc(_) | LE::Arith(..) | LE::Neg(..) | LE::Len(_) => LTy::I64,
            LE::F(_) | LE::FArith(..) | LE::FNeg(_) | LE::Prim(Prim::UToF, _) => LTy::F64,
            LE::Prim(Prim::ULt | Prim::ULe | Prim::MulOvf, _) => LTy::Bool,
            LE::Prim(..) => LTy::I64,
            LE::B(_) | LE::Cmp(..) | LE::Not(_) => LTy::Bool,
            LE::S(_) | LE::SB(_) => LTy::Str,
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
            LE::Ffi(i, _) => self.d.externs[*i].1.ret.lty(),
            LE::Rt(rt, args) => match rt {
                Rt::IntToS | Rt::PIntToS | Rt::StrRev | Rt::StrDelete | Rt::StrJoin | Rt::FToS | Rt::FFmt | Rt::FFmtE | Rt::StrPad | Rt::StrQuote | Rt::StrCat | Rt::U64ToS | Rt::IntFmt | Rt::RuneToS | Rt::StrFromBytes | Rt::FileRead | Rt::CapEnd | Rt::Strerror | Rt::StrFromCstr | Rt::StrFromPtr => LTy::Str,
                Rt::FileStatus | Rt::NowNs | Rt::CapBegin => LTy::I64,
                Rt::RegionCur | Rt::RegionMark | Rt::RegionMarkLarges => LTy::Region,
                Rt::RegionReset => LTy::Unit,
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
                Rt::PFromStr => LTy::PInt,
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
        let off = self.b.ins().imul_imm_s(i, lay(et).size as i64);
        self.b.ins().iadd(a[0], off)
    }

    /// Truncating division / remainder (Go semantics); `b` is nonzero, and
    /// not -1 with a == MIN for Div.
    fn floor_divrem(&mut self, op: Op, a: Value, b: Value) -> Value {
        // Divide by 1 instead of -1: avoids the machine trap on MIN % -1.
        let m1 = self.b.ins().icmp_imm_s(IntCC::Equal, b, -1);
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

    /// a * b, where b is the constant k if known: 2^s and 2^s + 1 as shifts
    /// (a shorter dependency chain than a multiply).
    fn imul_k(&mut self, a: Value, b: Value, k: Option<i64>) -> Value {
        match k {
            Some(k) if k > 1 && (k as u64).is_power_of_two() => self.b.ins().ishl_imm_s(a, k.trailing_zeros() as i64),
            Some(k) if k > 2 && ((k - 1) as u64).is_power_of_two() => {
                let sh = self.b.ins().ishl_imm_s(a, (k - 1).trailing_zeros() as i64);
                self.b.ins().iadd(a, sh)
            }
            _ => self.b.ins().imul(a, b),
        }
    }

    fn arith(&mut self, op: Op, a: &LE, b: &LE, ovf: &Ovf) -> Value {
        // A constant operand on the right (multiplication commutes).
        let (a, b) = if op == Op::Mul && matches!(a, LE::I(_)) && !matches!(b, LE::I(_)) { (b, a) } else { (a, b) };
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
                    // Checks as plain compares (Cranelift fuses them into the
                    // branch; its overflow flags go through a cset).
                    let (r, of) = match (op, konst) {
                        (Op::Add | Op::Sub, Some(0)) => (av, None),
                        (Op::Add, Some(k)) => {
                            let r = self.b.ins().iadd(av, bv);
                            let of = if k > 0 { self.b.ins().icmp_imm_s(IntCC::SignedGreaterThan, av, i64::MAX - k) } else { self.b.ins().icmp_imm_s(IntCC::SignedLessThan, av, i64::MIN - k) };
                            (r, Some(of))
                        }
                        (Op::Sub, Some(k)) if k != i64::MIN => {
                            let r = self.b.ins().isub(av, bv);
                            let of = if k > 0 { self.b.ins().icmp_imm_s(IntCC::SignedLessThan, av, i64::MIN + k) } else { self.b.ins().icmp_imm_s(IntCC::SignedGreaterThan, av, i64::MAX + k) };
                            (r, Some(of))
                        }
                        (Op::Add | Op::Sub, _) => {
                            // Overflow iff the result's sign differs from both
                            // operands' (add) / from a's and b's differs from a's (sub).
                            let (r, x) = if op == Op::Add {
                                let r = self.b.ins().iadd(av, bv);
                                (r, self.b.ins().bxor(bv, r))
                            } else {
                                let r = self.b.ins().isub(av, bv);
                                (r, self.b.ins().bxor(av, bv))
                            };
                            let y = self.b.ins().bxor(av, r);
                            let m = self.b.ins().band(x, y);
                            (r, Some(self.b.ins().icmp_imm_s(IntCC::SignedLessThan, m, 0)))
                        }
                        _ => {
                            let r = self.imul_k(av, bv, konst);
                            let hi = self.b.ins().smulhi(av, bv);
                            let sign = self.b.ins().sshr_imm_s(r, 63);
                            (r, Some(self.b.ins().icmp(IntCC::NotEqual, hi, sign)))
                        }
                    };
                    if let Some(of) = of {
                        self.overflow_if(of, &loc.clone());
                    }
                    r
                }
                _ => match op {
                    Op::Add => self.b.ins().iadd(av, bv),
                    Op::Sub => self.b.ins().isub(av, bv),
                    _ => self.imul_k(av, bv, konst),
                },
            },
            Op::Div | Op::Rem => {
                if let Some(k) = konst.filter(|k| *k > 1 && (*k as u64).is_power_of_two()) {
                    // Truncating division by 2^s: bias negatives by 2^s - 1, then shift.
                    let sh = k.trailing_zeros() as i64;
                    let sign = self.b.ins().sshr_imm_s(av, 63);
                    let bias = self.b.ins().ushr_imm_s(sign, 64 - sh);
                    let biased = self.b.ins().iadd(av, bias);
                    let q = self.b.ins().sshr_imm_s(biased, sh);
                    return if op == Op::Div {
                        q
                    } else {
                        let qk = self.b.ins().ishl_imm_s(q, sh);
                        self.b.ins().isub(av, qk)
                    };
                }
                if konst.is_none_or(|k| k == 0 || k == -1) {
                    let z = self.b.ins().icmp_imm_s(IntCC::Equal, bv, 0);
                    let loc = loc.to_string();
                    self.cold_if(z, |s| s.panic("division by zero", &loc));
                    if op == Op::Div {
                        let m1 = self.b.ins().icmp_imm_s(IntCC::Equal, bv, -1);
                        let mn = self.b.ins().icmp_imm_s(IntCC::Equal, av, i64::MIN);
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
            LE::Global(k) => {
                let (t, at) = self.d.globals[*k].clone();
                let addr = self.ic(at);
                self.load(&t, addr, 0)
            }
            LE::RegionNew(p) => {
                let p = self.e1(p);
                vec![self.call_rt(rt::alxj_region_new_child, &[p], true).unwrap()]
            }
            LE::RegionBytes(r) => {
                let r = self.e1(r);
                vec![self.call_rt(rt::alxj_region_bytes, &[r], true).unwrap()]
            }
            LE::RegionOf(x) => {
                // Arr and Str both keep their data pointer in word 0.
                let v = self.e(x);
                vec![self.call_rt(rt::alxj_region_of, &[v[0]], true).unwrap()]
            }
            LE::RegionProgram => vec![self.call_rt(rt::alxj_region_program, &[], true).unwrap()],
            LE::ChanNew(t, cap) => {
                let c = self.e1(cap);
                let esz = self.ic(lay(t).size as i64);
                vec![self.call_rt(rt::alxj_chan_new, &[c, esz], true).unwrap()]
            }
            LE::ChanLen(c) => {
                let c = self.e1(c);
                vec![self.call_rt(rt::alxj_chan_len, &[c], true).unwrap()]
            }
            LE::LockNew => vec![self.call_rt(rt::alxj_lock_new, &[], true).unwrap()],
            LE::LockPoisoned(l) => {
                let l = self.e1(l);
                let r = self.call_rt(rt::alxj_lock_poisoned, &[l], true).unwrap();
                vec![self.b.ins().icmp_imm_s(IntCC::NotEqual, r, 0)]
            }
            LE::NullTask(_) => vec![self.ic(0)],
            LE::AtomicNew(v) => {
                let v = self.e1(v);
                vec![self.call_rt(rt::alxj_atomic_new, &[v], true).unwrap()]
            }
            LE::AtomicLoad(a) => {
                let a = self.e1(a);
                vec![self.b.ins().atomic_load(I64, MemFlagsData::trusted(), a)]
            }
            LE::AtomicRmw(op, a, v) => {
                let (a, v) = (self.e1(a), self.e1(v));
                match op {
                    AtomicOp::Add => {
                        let old = self.b.ins().atomic_rmw(I64, MemFlagsData::trusted(), AtomicRmwOp::Add, a, v);
                        vec![self.b.ins().iadd(old, v)]
                    }
                    AtomicOp::Swap => vec![self.b.ins().atomic_rmw(I64, MemFlagsData::trusted(), AtomicRmwOp::Xchg, a, v)],
                }
            }
            LE::AtomicCas(a, o, n) => {
                let (a, o, n) = (self.e1(a), self.e1(o), self.e1(n));
                let old = self.b.ins().atomic_cas(MemFlagsData::trusted(), a, o, n);
                vec![self.b.ins().icmp(IntCC::Equal, old, o)]
            }
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
            LE::Prim(p, args) => {
                let a: Vec<Value> = args.iter().map(|x| self.e1(x)).collect();
                let ins = self.b.ins();
                vec![match p {
                    Prim::And => ins.band(a[0], a[1]),
                    Prim::Or => ins.bor(a[0], a[1]),
                    Prim::Xor => ins.bxor(a[0], a[1]),
                    Prim::AndNot => ins.band_not(a[0], a[1]),
                    Prim::Not => ins.bnot(a[0]),
                    Prim::Shl => ins.ishl(a[0], a[1]),
                    Prim::ShrS => ins.sshr(a[0], a[1]),
                    Prim::ShrU => ins.ushr(a[0], a[1]),
                    Prim::MulOvf => {
                        // Shares the product with the wrapping multiply beside it.
                        let (lo, hi) = (ins.imul(a[0], a[1]), self.b.ins().smulhi(a[0], a[1]));
                        let sign = self.b.ins().sshr_imm_s(lo, 63);
                        self.b.ins().icmp(IntCC::NotEqual, hi, sign)
                    }
                    Prim::ULt => ins.icmp(IntCC::UnsignedLessThan, a[0], a[1]),
                    Prim::ULe => ins.icmp(IntCC::UnsignedLessThanOrEqual, a[0], a[1]),
                    Prim::UDiv => ins.udiv(a[0], a[1]),
                    Prim::URem => ins.urem(a[0], a[1]),
                    Prim::UMulHi => ins.umulhi(a[0], a[1]),
                    Prim::UToF => ins.fcvt_from_uint(F64, a[0]),
                    Prim::Wrap(k) => {
                        let nt = match k.bits() {
                            8 => I8,
                            16 => cranelift_codegen::ir::types::I16,
                            _ => cranelift_codegen::ir::types::I32,
                        };
                        let r = ins.ireduce(nt, a[0]);
                        if k.signed() { self.b.ins().sextend(I64, r) } else { self.b.ins().uextend(I64, r) }
                    }
                }]
            }
            LE::B(b) => vec![self.b.ins().iconst(I8, *b as i64)],
            LE::S(s) => {
                let p = self.strs.bytes(s.as_bytes());
                vec![self.ic(p), self.ic(s.len() as i64)]
            }
            LE::SB(s) => {
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
                        vec![self.b.ins().icmp_imm_s(Self::icmp_of(*op), c, 0)]
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
                            return vec![self.b.ins().icmp_imm_s(cc, r, 0)];
                        }
                        let (av, bv) = (self.e(a), self.e(b));
                        let (pa, pb) = (self.spill(&LTy::Str, &av), self.spill(&LTy::Str, &bv));
                        if matches!(op, Op::Eq | Op::Ne) {
                            let r = self.call_rt(rt::alxj_str_eq, &[pa, pb], true).unwrap();
                            let cc = if *op == Op::Eq { IntCC::NotEqual } else { IntCC::Equal };
                            vec![self.b.ins().icmp_imm_s(cc, r, 0)]
                        } else {
                            let r = self.call_rt(rt::alxj_str_cmp, &[pa, pb], true).unwrap();
                            vec![self.b.ins().icmp_imm_s(Self::icmp_of(*op), r, 0)]
                        }
                    }
                    LTy::PInt => {
                        let c = self.pint_call(rt::alxj_p_cmp, None, &[a, b], None)[0];
                        vec![self.b.ins().icmp_imm_s(Self::icmp_of(*op), c, 0)]
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
                    let mn = self.b.ins().icmp_imm_s(IntCC::Equal, v, i64::MIN);
                    self.overflow_if(mn, &loc.clone());
                }
                vec![self.b.ins().ineg(v)]
            }
            LE::Not(x) => {
                let v = self.e1(x);
                vec![self.b.ins().icmp_imm_s(IntCC::Equal, v, 0)]
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
            LE::Ffi(i, args) => self.ffi_call(*i, args),
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
            LE::ArrWithCap(_, n) if matches!(**n, LE::I(k) if k <= 0) => {
                let z = self.ic(0);
                vec![z, z, z]
            }
            LE::ArrWithCap(t, n) => {
                let nv = self.e1(n);
                let ez = self.ic(lay(t).size as i64);
                let p = self.call_rt(rt::alxj_arr_alloc, &[nv, ez], true).unwrap();
                let z = self.ic(0);
                let neg = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, nv, 0);
                let cap = self.b.ins().select(neg, z, nv);
                vec![p, z, cap]
            }
            LE::Slice(t, a, start, len) => {
                let esz = lay(elem(t)).size as i64;
                let av = self.e(a);
                let s = self.e1(start);
                let l = self.e1(len);
                let off = self.b.ins().imul_imm_s(s, esz);
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
            Rt::PFromStr => self.pint_call(rt::alxj_p_from_str, Some(&LTy::PInt), &[&args[0]], None),
            Rt::StrRev => self.pint_call(rt::alxj_str_rev, Some(&s), &[&args[0]], None),
            Rt::StrDelete => self.pint_call(rt::alxj_str_delete, Some(&s), &[&args[0], &args[1]], None),
            Rt::StrSplit => self.pint_call(rt::alxj_str_split, Some(&LTy::Arr(Box::new(LTy::Str))), &[&args[0], &args[1]], None),
            Rt::StrJoin => self.pint_call(rt::alxj_str_join, Some(&LTy::Str), &[&args[0], &args[1]], None),
            Rt::StrToI => self.pint_call(rt::alxj_str_to_i, None, &[&args[0]], None),
            Rt::StrIndex => self.pint_call(rt::alxj_str_index, None, &[&args[0], &args[1], &args[2]], None),
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
                let low = self.b.ins().band_imm_s(v, 1);
                vec![self.b.ins().icmp_imm_s(IntCC::Equal, low, 0)]
            }
            Rt::PEven => {
                let r = self.pint_call(rt::alxj_p_even, None, &[&args[0]], None)[0];
                vec![self.b.ins().icmp_imm_s(IntCC::NotEqual, r, 0)]
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
            Rt::Math(f) => {
                let a: Vec<Value> = args.iter().map(|x| self.e1(x)).collect();
                vec![match f {
                    MathFn::Floor => self.b.ins().floor(a[0]),
                    MathFn::Ceil => self.b.ins().ceil(a[0]),
                    MathFn::Trunc => self.b.ins().trunc(a[0]),
                    MathFn::RoundEven => self.b.ins().nearest(a[0]),
                    MathFn::Fma => self.b.ins().fma(a[0], a[1], a[2]),
                    MathFn::Copysign => self.b.ins().fcopysign(a[0], a[1]),
                    _ => self.call_rt_t(libm_ptr(f), &a, Some(F64)).unwrap(),
                }]
            }
            Rt::FBits => {
                let v = self.e1(&args[0]);
                vec![self.b.ins().bitcast(I64, MemFlagsData::new(), v)]
            }
            Rt::FFromBits => {
                let v = self.e1(&args[0]);
                vec![self.b.ins().bitcast(F64, MemFlagsData::new(), v)]
            }
            Rt::FileStatus => {
                let v = self.e(&args[0]);
                let ps = self.spill(&LTy::Str, &v);
                vec![self.call_rt(rt::alxj_file_status, &[ps], true).unwrap()]
            }
            Rt::NowNs => vec![self.call_rt(rt::alxj_now_ns, &[], true).unwrap()],
            Rt::RegionCur => vec![self.call_rt(rt::alxj_region_cur, &[], true).unwrap()],
            Rt::RegionMark => vec![self.call_rt(rt::alxj_region_mark, &[], true).unwrap()],
            Rt::RegionMarkLarges => vec![self.call_rt(rt::alxj_region_mark_larges, &[], true).unwrap()],
            Rt::RegionReset => {
                let m = self.e1(&args[0]);
                let l = self.e1(&args[1]);
                self.call_rt(rt::alxj_region_reset, &[m, l], false);
                vec![]
            }
            Rt::Errno => vec![self.call_rt(rt::alxj_errno, &[], true).unwrap()],
            Rt::Strerror | Rt::StrFromCstr => {
                let v = self.e1(&args[0]);
                let out = self.slot(16);
                self.call_rt(if r == Rt::Strerror { rt::alxj_strerror } else { rt::alxj_str_from_cstr }, &[out, v], false);
                self.load(&s, out, 0)
            }
            Rt::StrFromPtr => {
                let (p, n) = (self.e1(&args[0]), self.e1(&args[1]));
                let out = self.slot(16);
                self.call_rt(rt::alxj_str_from_ptr, &[out, p, n], false);
                self.load(&s, out, 0)
            }
            Rt::CapBegin => vec![self.call_rt(rt::alxj_cap_begin, &[], true).unwrap()],
            Rt::CapEnd => {
                let out = self.slot(16);
                self.call_rt(rt::alxj_cap_end, &[out], false);
                self.load(&s, out, 0)
            }
            Rt::FileRead => {
                let v = self.e(&args[0]);
                let ps = self.spill(&LTy::Str, &v);
                let out = self.slot(16);
                self.call_rt(rt::alxj_file_read_or_empty, &[out, ps], false);
                self.load(&s, out, 0)
            }
            Rt::StrFromBytes => {
                let a = self.e(&args[0]);
                let out = self.slot(16);
                self.call_rt(rt::alxj_str_from_bytes, &[out, a[0], a[1]], false);
                self.load(&s, out, 0)
            }
            Rt::U64ToS | Rt::RuneToS => {
                let v = self.e1(&args[0]);
                let out = self.slot(16);
                self.call_rt(if r == Rt::U64ToS { rt::alxj_u64_to_s } else { rt::alxj_rune_to_s }, &[out, v], false);
                self.load(&s, out, 0)
            }
            Rt::IntFmt => {
                let vs: Vec<Value> = args.iter().map(|a| self.e1(a)).collect();
                let out = self.slot(16);
                self.call_rt(rt::alxj_int_fmt, &[out, vs[0], vs[1], vs[2], vs[3]], false);
                self.load(&s, out, 0)
            }
            Rt::FToU64 => {
                let v = self.e1(&args[0]);
                let l = self.e1(&args[1]);
                vec![self.call_rt(rt::alxj_f_to_u64, &[v, l], true).unwrap()]
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
            Rt::FFmtE => {
                let v = self.e1(&args[0]);
                let d = self.e1(&args[1]);
                let u = self.e1(&args[2]);
                let out = self.slot(16);
                self.call_rt(rt::alxj_f_fmt_e, &[out, v, d, u], false);
                self.load(&s, out, 0)
            }
            Rt::StrPad => {
                let sv = self.e(&args[0]);
                let sp = self.spill(&s, &sv);
                let w = self.e1(&args[1]);
                let fl = self.e1(&args[2]);
                let out = self.slot(16);
                self.call_rt(rt::alxj_str_pad, &[out, sp, w, fl], false);
                self.load(&s, out, 0)
            }
            Rt::StrQuote => self.pint_call(rt::alxj_str_quote, Some(&s), &[&args[0]], None),
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
