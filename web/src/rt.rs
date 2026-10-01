//! The Alexandrite runtime for the browser: runtime/alx.c and alx_big.c,
//! ported to Rust. These functions are exported from the compiler's own wasm
//! module; generated programs import them (and its memory) directly, so a
//! call costs the same as a call within one module.
//!
//! ABI (generated code ↔ runtime): only i64/i32 arguments. Pointers are
//! byte addresses in this module's memory, carried as i64 (memory layouts
//! match the native ones: pointers occupy 8 bytes). Aggregate results are
//! written to `RET` and read back from fixed addresses by generated code.
//!
//! Memory is one region per run, as in v0 native: a bump arena, dropped
//! wholesale when the next run starts.

use num_bigint::BigInt;
use num_integer::Integer;
use num_traits::{One, Signed, ToPrimitive, Zero};
use std::cell::UnsafeCell;
use std::collections::HashMap;

/// Single-threaded global state (wasm32 without threads).
pub struct G<T>(UnsafeCell<T>);
unsafe impl<T> Sync for G<T> {}
impl<T> G<T> {
    #[allow(clippy::mut_from_ref)]
    fn get(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }
}

/// Aggregate results: up to three words.
pub static RET: G<[i64; 4]> = G(UnsafeCell::new([0; 4]));

pub fn ret_addr() -> i64 {
    RET.0.get() as usize as i64
}

#[derive(Default)]
pub struct State {
    chunks: Vec<Vec<u8>>,
    cur: usize,
    end: usize,
    bigs: Vec<Box<BigInt>>,
    pub out: String,
    pub err: String,
    /// The pending error of a fallible call: (tag, detail, loc).
    error: (u8, String, String),
    pub files: HashMap<String, Vec<u8>>,
    /// Kept alive for the program's lifetime: string literals and locations.
    pub consts: Vec<Box<[u8]>>,
    /// 10**k, filled on demand.
    p10: Vec<BigInt>,
}

pub static ST: G<Option<State>> = G(UnsafeCell::new(None));

pub fn st() -> &'static mut State {
    ST.get().get_or_insert_with(State::default)
}

const CHUNK: usize = 1 << 20;

/// Start a run: drop the previous run's memory and output.
pub fn reset() {
    let s = st();
    s.chunks.clear();
    s.cur = 0;
    s.end = 0;
    s.bigs.clear();
    s.out.clear();
    s.err.clear();
    s.error = (0, String::new(), String::new());
}

/// A thrown run: what JS sees. stderr already holds the message.
#[derive(Clone, Copy)]
pub enum Stop {
    Abort,
    Exit1,
}

fn stop(how: Stop) -> ! {
    wasm_bindgen::throw_str(match how {
        Stop::Abort => "alx:abort",
        Stop::Exit1 => "alx:exit1",
    })
}

// ---------- memory ----------

fn alloc(n: usize) -> usize {
    let n = (n.max(1) + 7) & !7;
    let s = st();
    if s.end - s.cur < n {
        let size = n.max(CHUNK);
        let mut v = Vec::<u8>::with_capacity(size);
        let p = v.as_mut_ptr() as usize;
        s.chunks.push(v);
        if n >= CHUNK {
            // A big block of its own; keep bumping in the current chunk.
            return p;
        }
        s.cur = p;
        s.end = p + size;
    }
    let p = s.cur;
    s.cur += n;
    p
}

fn bytes<'a>(p: i64, n: i64) -> &'a [u8] {
    if n <= 0 { &[] } else { unsafe { std::slice::from_raw_parts(p as usize as *const u8, n as usize) } }
}

fn new_bytes(b: &[u8]) -> i64 {
    let p = alloc(b.len());
    unsafe { std::ptr::copy_nonoverlapping(b.as_ptr(), p as *mut u8, b.len()) };
    p as i64
}

fn ret(words: &[i64]) {
    RET.get()[..words.len()].copy_from_slice(words);
}

fn ret_str(b: &[u8]) {
    ret(&[new_bytes(b), b.len() as i64]);
}

fn word(p: usize, i: usize) -> &'static mut i64 {
    unsafe { &mut *((p + 8 * i) as *mut i64) }
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_alloc(n: i64) -> i64 {
    alloc(n.max(0) as usize) as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_zalloc(n: i64) -> i64 {
    let p = alloc(n.max(0) as usize);
    unsafe { std::ptr::write_bytes(p as *mut u8, 0, n.max(0) as usize) };
    p as i64
}

/// Grow storage to `cap` elements of `esz` bytes, keeping the first `len`.
#[unsafe(no_mangle)]
pub extern "C" fn alxr_grow(p: i64, len: i64, cap: i64, esz: i64) -> i64 {
    let np = alloc((cap * esz) as usize);
    unsafe { std::ptr::copy_nonoverlapping(p as usize as *const u8, np as *mut u8, (len * esz) as usize) };
    np as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_copy(p: i64, len: i64, esz: i64) -> i64 {
    alxr_grow(p, len, len.max(1), esz)
}

// ---------- panics and errors ----------

fn loc(lp: i64, ln: i64) -> String {
    String::from_utf8_lossy(bytes(lp, ln)).into_owned()
}

pub fn panic_msg(what: &str, at: &str) -> ! {
    st().err.push_str(&format!("alexandrite: {what} at {at}\n"));
    stop(Stop::Abort)
}

fn overflow_at(at: &str) -> ! {
    st().err.push_str(&format!("alexandrite: overflow at {at}\nhint: add `#![overflow(promote)]` to this file to promote to bignums\n"));
    stop(Stop::Abort)
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_panic(mp: i64, mn: i64, lp: i64, ln: i64) {
    panic_msg(&loc(mp, mn), &loc(lp, ln))
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_overflow(lp: i64, ln: i64) {
    overflow_at(&loc(lp, ln))
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_err_overflow(lp: i64, ln: i64) {
    st().error = (1, String::new(), loc(lp, ln));
}

/// An uncaught error at the top level: print it and exit 1.
#[unsafe(no_mangle)]
pub extern "C" fn alxr_die() {
    let s = st();
    let (tag, detail, at) = s.error.clone();
    let msg = match tag {
        1 => format!("error: overflow ({at})\n"),
        2 => format!("File.read: no such file `{detail}` ({at})\n"),
        _ => format!("error ({at})\n"),
    };
    s.err.push_str(&msg);
    stop(Stop::Exit1)
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_file_read(p: i64, n: i64, lp: i64, ln: i64) -> i32 {
    let path = String::from_utf8_lossy(bytes(p, n)).into_owned();
    let s = st();
    let key = path.trim_start_matches("./").to_string();
    match s.files.get(&key) {
        Some(data) => {
            let data = data.clone();
            ret_str(&data);
            1
        }
        None => {
            s.error = (2, path, loc(lp, ln));
            0
        }
    }
}

// ---------- integers ----------

fn try_pow(mut a: i64, mut b: i64) -> Option<i64> {
    if b < 0 {
        return None;
    }
    let mut acc: i64 = 1;
    while b > 0 {
        if b & 1 == 1 {
            acc = acc.checked_mul(a)?;
        }
        b >>= 1;
        if b > 0 {
            a = a.checked_mul(a)?;
        }
    }
    Some(acc)
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_pow(a: i64, b: i64, lp: i64, ln: i64) -> i64 {
    if b < 0 {
        panic_msg("negative exponent", &loc(lp, ln));
    }
    try_pow(a, b).unwrap_or_else(|| overflow_at(&loc(lp, ln)))
}

/// Ok flag; the value in RET[0].
#[unsafe(no_mangle)]
pub extern "C" fn alxr_try_pow(a: i64, b: i64) -> i32 {
    match try_pow(a, b) {
        Some(r) => {
            ret(&[r]);
            1
        }
        None => 0,
    }
}

/// a * b; RET[0] = 1 if it overflowed.
#[unsafe(no_mangle)]
pub extern "C" fn alxr_mul_chk(a: i64, b: i64) -> i64 {
    let (r, o) = a.overflowing_mul(b);
    ret(&[o as i64]);
    r
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_isqrt(n: i64, lp: i64, ln: i64) -> i64 {
    if n < 0 {
        panic_msg("Int.sqrt of a negative number", &loc(lp, ln));
    }
    let mut x = (n as f64).sqrt() as i64;
    while x > 0 && x > n / x {
        x -= 1;
    }
    while (x + 1) <= n / (x + 1) {
        x += 1;
    }
    x
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_digits(v: i64, lp: i64, ln: i64) {
    if v < 0 {
        panic_msg("`digits` of a negative number", &loc(lp, ln));
    }
    let mut ds = vec![];
    let mut v = v;
    loop {
        ds.push(v % 10);
        v /= 10;
        if v == 0 {
            break;
        }
    }
    ret_words_arr(&ds);
}

fn ret_words_arr(ws: &[i64]) {
    let p = alloc(8 * ws.len().max(1));
    for (i, w) in ws.iter().enumerate() {
        *word(p, i) = *w;
    }
    ret(&[p as i64, ws.len() as i64, ws.len() as i64]);
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_int_to_s(v: i64) {
    ret_str(v.to_string().as_bytes());
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_int_ndigits(v: i64) -> i64 {
    let mut u = v.unsigned_abs();
    let mut n = if v < 0 { 2 } else { 1 };
    while u >= 10 {
        u /= 10;
        n += 1;
    }
    n
}

// ---------- strings ----------

fn charlen(s: &[u8], i: usize) -> usize {
    let c = s[i];
    let n = if c < 0x80 {
        1
    } else if c < 0xE0 {
        2
    } else if c < 0xF0 {
        3
    } else {
        4
    };
    n.min(s.len() - i)
}

fn rev(s: &[u8]) -> Vec<u8> {
    let mut out = vec![0; s.len()];
    let mut o = s.len();
    let mut i = 0;
    while i < s.len() {
        let n = charlen(s, i);
        o -= n;
        out[o..o + n].copy_from_slice(&s[i..i + n]);
        i += n;
    }
    out
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_str_rev(p: i64, n: i64) {
    ret_str(&rev(bytes(p, n)));
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_str_is_pal(p: i64, n: i64) -> i32 {
    let s = bytes(p, n);
    if s.is_ascii() { s.iter().eq(s.iter().rev()) as i32 } else { (rev(s) == s) as i32 }
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_str_eq(ap: i64, an: i64, bp: i64, bn: i64) -> i32 {
    (bytes(ap, an) == bytes(bp, bn)) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_str_cmp(ap: i64, an: i64, bp: i64, bn: i64) -> i32 {
    bytes(ap, an).cmp(bytes(bp, bn)) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_str_delete(p: i64, n: i64, cp: i64, cn: i64) {
    let chars = bytes(cp, cn);
    let out: Vec<u8> = bytes(p, n).iter().copied().filter(|b| !chars.contains(b)).collect();
    ret_str(&out);
}

/// Arr_Str: elements are (ptr, len) pairs pointing into `s`.
#[unsafe(no_mangle)]
pub extern "C" fn alxr_str_split(p: i64, n: i64, sp: i64, sn: i64) {
    let (s, sep) = (bytes(p, n), bytes(sp, sn));
    if sep.is_empty() {
        panic_msg("`split` with an empty separator", "runtime");
    }
    let mut parts: Vec<(i64, i64)> = vec![];
    let (mut start, mut i) = (0usize, 0usize);
    while i + sep.len() <= s.len() {
        if &s[i..i + sep.len()] == sep {
            parts.push((p + start as i64, (i - start) as i64));
            i += sep.len();
            start = i;
        } else {
            i += 1;
        }
    }
    parts.push((p + start as i64, (s.len() - start) as i64));
    while parts.last().is_some_and(|x| x.1 == 0) {
        parts.pop();
    }
    let a = alloc(16 * parts.len().max(1));
    for (k, (pp, ln)) in parts.iter().enumerate() {
        *word(a, 2 * k) = *pp;
        *word(a, 2 * k + 1) = *ln;
    }
    ret(&[a as i64, parts.len() as i64, parts.len() as i64]);
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_str_to_i(p: i64, n: i64) -> i64 {
    let s = bytes(p, n);
    let mut i = 0;
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n') {
        i += 1;
    }
    let mut neg = false;
    if i < s.len() && (s[i] == b'-' || s[i] == b'+') {
        neg = s[i] == b'-';
        i += 1;
    }
    let mut v: i64 = 0;
    while i < s.len() && s[i].is_ascii_digit() {
        v = v.checked_mul(10).and_then(|v| v.checked_add((s[i] - b'0') as i64)).unwrap_or_else(|| overflow_at("String#to_i"));
        i += 1;
    }
    if neg { -v } else { v }
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_str_charlen(p: i64, n: i64, i: i64) -> i64 {
    charlen(bytes(p, n), i as usize) as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_str_sub(p: i64, _n: i64, i: i64, k: i64) {
    ret(&[p + i, k]);
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_sort_i64(p: i64, len: i64) {
    if len > 1 {
        unsafe { std::slice::from_raw_parts_mut(p as usize as *mut i64, len as usize) }.sort_unstable();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_sort_str(p: i64, len: i64) {
    if len > 1 {
        let v = unsafe { std::slice::from_raw_parts_mut(p as usize as *mut [i64; 2], len as usize) };
        v.sort_unstable_by(|a, b| bytes(a[0], a[1]).cmp(bytes(b[0], b[1])));
    }
}

// ---------- output ----------

#[unsafe(no_mangle)]
pub extern "C" fn alxr_puts_i64(v: i64) {
    let s = st();
    s.out.push_str(&v.to_string());
    s.out.push('\n');
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_puts_str(p: i64, n: i64) {
    let b = bytes(p, n);
    let s = st();
    s.out.push_str(&String::from_utf8_lossy(b));
    if b.last() != Some(&b'\n') {
        s.out.push('\n');
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_puts_bool(b: i32) {
    st().out.push_str(if b != 0 { "true\n" } else { "false\n" });
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_puts_unit() {
    st().out.push('\n');
}

// ---------- bignums (promote mode) ----------
// A PInt is (v, big): `big` is 0 for a small value, else a pointer to a
// BigInt owned by the run.

fn big_of(v: i64, b: i64) -> BigInt {
    if b == 0 { BigInt::from(v) } else { unsafe { (*(b as usize as *const BigInt)).clone() } }
}

fn pint(x: BigInt) -> [i64; 2] {
    match x.to_i64() {
        Some(v) => [v, 0],
        None => {
            let bx = Box::new(x);
            let p = &*bx as *const BigInt as usize as i64;
            st().bigs.push(bx);
            [0, p]
        }
    }
}

fn ret_p(x: BigInt) {
    ret(&pint(x));
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_p_add(av: i64, ab: i64, bv: i64, bb: i64) {
    if ab == 0 && bb == 0 {
        if let Some(r) = av.checked_add(bv) {
            return ret(&[r, 0]);
        }
    }
    ret_p(big_of(av, ab) + big_of(bv, bb));
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_p_sub(av: i64, ab: i64, bv: i64, bb: i64) {
    if ab == 0 && bb == 0 {
        if let Some(r) = av.checked_sub(bv) {
            return ret(&[r, 0]);
        }
    }
    ret_p(big_of(av, ab) - big_of(bv, bb));
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_p_mul(av: i64, ab: i64, bv: i64, bb: i64) {
    if ab == 0 && bb == 0 {
        if let Some(r) = av.checked_mul(bv) {
            return ret(&[r, 0]);
        }
    }
    ret_p(big_of(av, ab) * big_of(bv, bb));
}

fn divmod(av: i64, ab: i64, bv: i64, bb: i64, lp: i64, ln: i64) -> (BigInt, BigInt) {
    let b = big_of(bv, bb);
    if b.is_zero() {
        panic_msg("division by zero", &loc(lp, ln));
    }
    big_of(av, ab).div_rem(&b) // truncating, as in Go
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_p_div(av: i64, ab: i64, bv: i64, bb: i64, lp: i64, ln: i64) {
    ret_p(divmod(av, ab, bv, bb, lp, ln).0);
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_p_rem(av: i64, ab: i64, bv: i64, bb: i64, lp: i64, ln: i64) {
    ret_p(divmod(av, ab, bv, bb, lp, ln).1);
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_p_pow(av: i64, ab: i64, bv: i64, bb: i64, lp: i64, ln: i64) {
    if bb != 0 || bv < 0 || bv > i32::MAX as i64 {
        panic_msg("exponent out of range", &loc(lp, ln));
    }
    if ab == 0 {
        if let Some(r) = try_pow(av, bv) {
            return ret(&[r, 0]);
        }
    }
    ret_p(num_traits::pow(big_of(av, ab), bv as usize));
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_p_cmp(av: i64, ab: i64, bv: i64, bb: i64) -> i32 {
    if ab == 0 && bb == 0 {
        return av.cmp(&bv) as i32;
    }
    big_of(av, ab).cmp(&big_of(bv, bb)) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_p_even(v: i64, b: i64) -> i32 {
    if b == 0 { (v & 1 == 0) as i32 } else { big_of(v, b).is_even() as i32 }
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_p_to_i64(v: i64, b: i64, lp: i64, ln: i64) -> i64 {
    if b != 0 {
        overflow_at(&loc(lp, ln));
    }
    v
}

fn p_string(v: i64, b: i64) -> String {
    if b == 0 { v.to_string() } else { big_of(v, b).to_string() }
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_p_to_s(v: i64, b: i64) {
    ret_str(p_string(v, b).as_bytes());
}

/// `a.to_s.size` without building the string.
#[unsafe(no_mangle)]
pub extern "C" fn alxr_p_ndigits(v: i64, b: i64) -> i64 {
    if b == 0 {
        return alxr_int_ndigits(v);
    }
    let x = big_of(v, b);
    let m = x.abs();
    let bits = m.bits();
    let mut d = ((bits - 1) as f64 * std::f64::consts::LOG10_2) as usize + 1;
    while d > 1 && m < *p10(d - 1) {
        d -= 1;
    }
    while m >= *p10(d) {
        d += 1;
    }
    d as i64 + x.is_negative() as i64
}

fn p10(k: usize) -> &'static BigInt {
    let t = &mut st().p10;
    if t.is_empty() {
        t.push(BigInt::one());
    }
    while t.len() <= k {
        let next = t.last().unwrap() * 10u32;
        t.push(next);
    }
    &st().p10[k]
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_p_digits(v: i64, b: i64, lp: i64, ln: i64) {
    let x = big_of(v, b);
    if x.is_negative() {
        panic_msg("`digits` of a negative number", &loc(lp, ln));
    }
    let s = if x.is_zero() { "0".to_string() } else { x.to_string() };
    let n = s.len();
    let a = alloc(16 * n);
    for (k, c) in s.bytes().rev().enumerate() {
        *word(a, 2 * k) = (c - b'0') as i64;
        *word(a, 2 * k + 1) = 0;
    }
    ret(&[a as i64, n as i64, n as i64]);
}

// ---------- floats ----------

/// A Float as Go's fmt prints it (see alx_f_to_s in alx.c).
fn go_float(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x < 0.0 { "-Inf".into() } else { "+Inf".into() };
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    let e = format!("{:e}", x.abs());
    let (mant, exp) = e.split_once('e').unwrap();
    let exp10: i32 = exp.parse().unwrap();
    let digits: Vec<char> = mant.chars().filter(|c| *c != '.').collect();
    let n = digits.len() as i32;
    let decpt = exp10 + 1;
    let mut out = String::new();
    if x < 0.0 {
        out.push('-');
    }
    if exp10 < -4 || exp10 >= 6 {
        out.push(digits[0]);
        if n > 1 {
            out.push('.');
            out.extend(&digits[1..]);
        }
        out.push_str(&format!("e{}{:02}", if exp10 < 0 { '-' } else { '+' }, exp10.abs()));
    } else if decpt <= 0 {
        out.push_str("0.");
        for _ in 0..-decpt {
            out.push('0');
        }
        out.extend(&digits);
    } else {
        for i in 0..decpt {
            out.push(if i < n { digits[i as usize] } else { '0' });
        }
        if n > decpt {
            out.push('.');
            out.extend(&digits[decpt as usize..]);
        }
    }
    out
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_puts_f64(x: f64) {
    let s = st();
    s.out.push_str(&go_float(x));
    s.out.push('\n');
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_f_to_s(x: f64) {
    ret_str(go_float(x).as_bytes());
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_f_fmt(x: f64, digits: i64) {
    let s = if x.is_nan() {
        "NaN".to_string()
    } else if x.is_infinite() {
        (if x < 0.0 { "-Inf" } else { "+Inf" }).to_string()
    } else {
        format!("{:.*}", digits.clamp(0, 40) as usize, x)
    };
    ret_str(s.as_bytes());
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_f_to_i(x: f64, lp: i64, ln: i64) -> i64 {
    if x.is_nan() || x.is_infinite() {
        panic_msg("Float#to_i of NaN or Infinity", &loc(lp, ln));
    }
    if x >= 9223372036854775808.0 || x < -9223372036854775808.0 {
        panic_msg("Float#to_i: out of Int range", &loc(lp, ln));
    }
    x as i64
}

/// Concatenate `n` strings stored as (ptr, len) pairs at `p`.
#[unsafe(no_mangle)]
pub extern "C" fn alxr_str_cat(p: i64, n: i64) {
    let mut out = vec![];
    for k in 0..n as usize {
        out.extend_from_slice(bytes(*word(p as usize, 2 * k), *word(p as usize, 2 * k + 1)));
    }
    ret_str(&out);
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_puts_pint(v: i64, b: i64) {
    let s = st();
    s.out.push_str(&p_string(v, b));
    s.out.push('\n');
}

