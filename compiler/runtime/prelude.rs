// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
// Prelude for the Rust oracle backend: the same runtime surface as alx.h,
// in safe Rust, so rustc checks every emitted program.
#![feature(gen_blocks)]
#![allow(unused, non_snake_case, unreachable_code, unused_mut, unused_variables, unused_assignments, unused_labels, while_true, dead_code, unused_parens)]

mod rt {
    use std::cell::RefCell;
    use std::cmp::Ordering;
    use std::rc::Rc;
    use std::sync::Arc;

    #[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Debug)]
    pub struct Str(pub Arc<[u8]>);

    impl Str {
        pub fn lit(b: &[u8]) -> Str {
            Str(Arc::from(b))
        }
        pub fn len(&self) -> i64 {
            self.0.len() as i64
        }
    }

    #[derive(Clone, Copy, Default, Debug)]
    pub struct AlxRange {
        pub lo: i64,
        pub hi: i64,
        pub excl: bool,
    }

    /// Enumerator: a shared, resumable iterator.
    pub struct Gen<T>(pub Rc<RefCell<Box<dyn Iterator<Item = T>>>>);
    impl<T> Clone for Gen<T> {
        fn clone(&self) -> Self {
            Gen(self.0.clone())
        }
    }
    impl<T: 'static> Default for Gen<T> {
        fn default() -> Self {
            Gen(Rc::new(RefCell::new(Box::new(std::iter::empty()))))
        }
    }
    impl<T: 'static> Gen<T> {
        pub fn new(it: impl Iterator<Item = T> + 'static) -> Self {
            Gen(Rc::new(RefCell::new(Box::new(it))))
        }
        pub fn next(&self) -> Option<T> {
            self.0.borrow_mut().next()
        }
    }

    thread_local! {
        /// True on threads started by `task_spawn`.
        static IN_TASK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    /// A runtime failure. On the main thread: flush stdout, print `text` to
    /// stderr and abort. On a task thread: print nothing and unwind with
    /// `text` as the payload; `task_spawn` catches it and `Task::wait`
    /// reports it as `Err(text)`.
    pub fn fail(text: String) -> ! {
        if IN_TASK.with(|t| t.get()) {
            std::panic::resume_unwind(Box::new(text))
        }
        use std::io::Write;
        let _ = std::io::stdout().flush();
        eprintln!("{text}");
        std::process::abort()
    }
    pub fn panic(what: &str, loc: &str) -> ! {
        fail(format!("alexandrite: {what} at {loc}"))
    }
    pub fn panic_str(s: Str) -> ! {
        fail(format!("alexandrite: {}", String::from_utf8_lossy(&s.0)))
    }
    pub fn overflow(loc: &str) -> ! {
        fail(format!("alexandrite: overflow at {loc}\nhint: add `#![overflow(promote)]` to this file to promote to bignums"))
    }
    pub fn die_str(s: Str) -> ! {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let mut e = std::io::stderr();
        let _ = e.write_all(&s.0);
        let _ = e.write_all(b"\n");
        std::process::exit(1)
    }
    /// A Go slice: a window (off, len, cap) onto shared storage. Clones share
    /// the storage; `push` writes in place while there's capacity, else copies.
    #[derive(Debug)]
    pub struct Sl<T> {
        buf: Arc<std::sync::Mutex<Vec<T>>>,
        off: usize,
        len: usize,
        cap: usize,
    }
    impl<T> Clone for Sl<T> {
        fn clone(&self) -> Self {
            Sl { buf: self.buf.clone(), off: self.off, len: self.len, cap: self.cap }
        }
    }
    impl<T> Default for Sl<T> {
        fn default() -> Self {
            Sl::from(vec![])
        }
    }
    impl<T> From<Vec<T>> for Sl<T> {
        fn from(v: Vec<T>) -> Self {
            let n = v.len();
            Sl { buf: Arc::new(std::sync::Mutex::new(v)), off: 0, len: n, cap: n }
        }
    }
    impl<T: Clone> Sl<T> {
        pub fn with_cap(c: usize) -> Self {
            Sl { buf: Arc::new(std::sync::Mutex::new(Vec::with_capacity(c))), off: 0, len: 0, cap: c }
        }
        pub fn len(&self) -> usize {
            self.len
        }
        pub fn get(&self, i: usize) -> T {
            self.buf.lock().unwrap()[self.off + i].clone()
        }
        pub fn with<R>(&self, i: usize, f: impl FnOnce(&mut T) -> R) -> R {
            f(&mut self.buf.lock().unwrap()[self.off + i])
        }
        pub fn to_vec(&self) -> Vec<T> {
            self.buf.lock().unwrap()[self.off..self.off + self.len].to_vec()
        }
        /// `a[s, n]`: shares storage, capacity 0 (appending copies).
        pub fn slice(&self, s: usize, n: usize) -> Self {
            Sl { buf: self.buf.clone(), off: self.off + s, len: n, cap: 0 }
        }
        pub fn push(&mut self, x: T) {
            if self.len < self.cap {
                let mut b = self.buf.lock().unwrap();
                let at = self.off + self.len;
                if at < b.len() {
                    b[at] = x;
                } else {
                    b.push(x);
                }
            } else {
                let mut v = self.to_vec();
                let c = (self.len * 2).max(4);
                v.reserve(c - v.len());
                v.push(x);
                *self = Sl { buf: Arc::new(std::sync::Mutex::new(v)), off: 0, len: self.len, cap: c };
            }
            self.len += 1;
        }
    }

    impl<T: Ord> Sl<T> {
        pub fn sort(&mut self) {
            self.buf.lock().unwrap()[self.off..self.off + self.len].sort();
        }
    }

    pub fn idx(i: i64, n: usize, loc: &str) -> usize {
        if i < 0 || i as u64 >= n as u64 {
            panic("index out of bounds", loc)
        }
        i as usize
    }

    pub fn add(a: i64, b: i64, loc: &str) -> i64 {
        a.checked_add(b).unwrap_or_else(|| overflow(loc))
    }
    pub fn sub(a: i64, b: i64, loc: &str) -> i64 {
        a.checked_sub(b).unwrap_or_else(|| overflow(loc))
    }
    pub fn mul(a: i64, b: i64, loc: &str) -> i64 {
        a.checked_mul(b).unwrap_or_else(|| overflow(loc))
    }
    pub fn div(a: i64, b: i64, loc: &str) -> i64 {
        if b == 0 {
            panic("division by zero", loc)
        }
        try_div(a, b).unwrap_or_else(|| overflow(loc))
    }
    pub fn rem(a: i64, b: i64, loc: &str) -> i64 {
        if b == 0 {
            panic("division by zero", loc)
        }
        try_rem(a, b).unwrap()
    }
    pub fn pow(a: i64, b: i64, loc: &str) -> i64 {
        if b < 0 {
            panic("negative exponent", loc)
        }
        try_pow(a, b).unwrap_or_else(|| overflow(loc))
    }
    pub fn neg(a: i64, loc: &str) -> i64 {
        a.checked_neg().unwrap_or_else(|| overflow(loc))
    }
    pub fn try_div(a: i64, b: i64) -> Option<i64> {
        if b == 0 {
            return None;
        }
        a.checked_div(b) // truncates, as in Go
    }
    pub fn try_rem(a: i64, b: i64) -> Option<i64> {
        if b == 0 {
            return None;
        }
        if b == -1 {
            return Some(0);
        }
        Some(a % b) // sign of the dividend, as in Go
    }
    pub fn try_pow(a: i64, b: i64) -> Option<i64> {
        if b < 0 || b > u32::MAX as i64 {
            return None;
        }
        a.checked_pow(b as u32)
    }
    pub fn sat_add(a: i64, b: i64) -> i64 {
        a.saturating_add(b)
    }
    pub fn even(a: i64) -> bool {
        a & 1 == 0
    }
    pub fn isqrt(n: i64, loc: &str) -> i64 {
        if n < 0 {
            panic("Int.sqrt of a negative number", loc)
        }
        n.isqrt()
    }
    pub fn digits(v: i64, loc: &str) -> Sl<i64> {
        Sl::from(digits_v(v, loc))
    }
    fn digits_v(mut v: i64, loc: &str) -> Vec<i64> {
        if v < 0 {
            panic("`digits` of a negative number", loc)
        }
        let mut d = vec![];
        loop {
            d.push(v % 10);
            v /= 10;
            if v == 0 {
                return d;
            }
        }
    }

    pub fn int_to_s(v: i64) -> Str {
        Str::lit(v.to_string().as_bytes())
    }
    pub fn str_rev(s: Str) -> Str {
        let t = String::from_utf8_lossy(&s.0).chars().rev().collect::<String>();
        Str::lit(t.as_bytes())
    }
    pub fn str_delete(s: Str, chars: Str) -> Str {
        Str(s.0.iter().copied().filter(|b| !chars.0.contains(b)).collect::<Vec<u8>>().into())
    }
    pub fn str_split(s: Str, sep: Str) -> Sl<Str> {
        Sl::from(str_split_v(s, sep))
    }
    fn str_split_v(s: Str, sep: Str) -> Vec<Str> {
        let mut out = vec![];
        let (b, p) = (&s.0[..], &sep.0[..]);
        let mut start = 0;
        let mut i = 0;
        while i + p.len() <= b.len() {
            if &b[i..i + p.len()] == p {
                out.push(Str::lit(&b[start..i]));
                i += p.len();
                start = i;
            } else {
                i += 1;
            }
        }
        out.push(Str::lit(&b[start..]));
        while out.last().is_some_and(|x| x.0.is_empty()) {
            out.pop();
        }
        out
    }
    pub fn str_to_i(s: Str) -> i64 {
        let t = String::from_utf8_lossy(&s.0);
        let t = t.trim_start();
        let (neg, rest) = match t.as_bytes().first() {
            Some(b'-') => (true, &t[1..]),
            Some(b'+') => (false, &t[1..]),
            _ => (false, t),
        };
        let mut v: i64 = 0;
        for c in rest.bytes().take_while(u8::is_ascii_digit) {
            v = v.checked_mul(10).and_then(|v| v.checked_add((c - b'0') as i64)).unwrap_or_else(|| overflow("String#to_i"));
        }
        if neg { -v } else { v }
    }
    pub fn str_charlen(s: &Str, i: i64) -> i64 {
        let c = s.0[i as usize];
        let n = if c < 0x80 { 1 } else if c < 0xE0 { 2 } else if c < 0xF0 { 3 } else { 4 };
        n.min(s.0.len() as i64 - i)
    }
    pub fn str_sub(s: &Str, i: i64, n: i64) -> Str {
        Str::lit(&s.0[i as usize..(i + n) as usize])
    }
    pub fn str_byte(s: &Str, i: i64) -> i64 {
        s.0[i as usize] as i64
    }
    pub fn file_status(p: Str) -> i64 {
        match std::fs::read(String::from_utf8_lossy(&p.0).to_string()) {
            Ok(_) => 0,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 1,
            Err(_) => 2,
        }
    }
    pub fn file_read_or_empty(p: Str) -> Str {
        match std::fs::read(String::from_utf8_lossy(&p.0).to_string()) {
            Ok(b) => Str(b.into()),
            Err(_) => Str::lit(b""),
        }
    }

    pub fn now_ns() -> i64 {
        static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        T0.get_or_init(std::time::Instant::now).elapsed().as_nanos() as i64
    }
    unsafe extern "C" {
        fn dup(fd: i32) -> i32;
        fn dup2(a: i32, b: i32) -> i32;
        fn close(fd: i32) -> i32;
    }
    /// (saved stdout fd, capture file path) while a capture is active.
    static CAP: std::sync::Mutex<Option<(i32, std::path::PathBuf)>> = std::sync::Mutex::new(None);
    /// Redirect file descriptor 1 into a temporary file (process-wide).
    pub fn cap_begin() -> i64 {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        let _ = std::io::stdout().flush();
        let mut g = CAP.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_some() {
            return 0;
        }
        let path = std::env::temp_dir().join(format!("alx-cap-{}", std::process::id()));
        if let Ok(f) = std::fs::File::create(&path) {
            unsafe {
                let saved = dup(1);
                dup2(f.as_raw_fd(), 1);
                *g = Some((saved, path));
            }
        }
        0
    }
    pub fn cap_end() -> Str {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let mut g = CAP.lock().unwrap_or_else(|e| e.into_inner());
        let Some((saved, path)) = g.take() else { return Str::lit(b"") };
        unsafe {
            dup2(saved, 1);
            close(saved);
        }
        let b = std::fs::read(&path).unwrap_or_default();
        let _ = std::fs::remove_file(&path);
        Str(b.into())
    }

    pub fn arr_new<T: Clone>(n: i64, fill: T, loc: &str) -> Sl<T> {
        if n < 0 {
            panic("negative array size", loc)
        }
        Sl::from(vec![fill; n as usize])
    }
    pub fn cap(n: i64) -> usize {
        n.max(0) as usize
    }

    /// A Float as Go's fmt prints it (see alx_f_to_s in alx.c).
    pub fn f_to_s(x: f64) -> Str {
        Str::lit(go_float(x).as_bytes())
    }
    pub fn go_float(x: f64) -> String {
        if x.is_nan() {
            return "NaN".into();
        }
        if x.is_infinite() {
            return if x < 0.0 { "-Inf".into() } else { "+Inf".into() };
        }
        if x == 0.0 {
            return if x.is_sign_negative() { "-0".into() } else { "0".into() };
        }
        // `{:e}` is the shortest round-trip form: d[.ddd]e[-]X
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
    pub fn f_fmt(x: f64, digits: i64) -> Str {
        let s = if x.is_nan() {
            "NaN".to_string()
        } else if x.is_infinite() {
            (if x < 0.0 { "-Inf" } else { "+Inf" }).to_string()
        } else {
            format!("{:.*}", digits as usize, x)
        };
        Str::lit(s.as_bytes())
    }
    // math block: libm for the std/math intrinsics (the same C library the C backend calls).
    mod cm {
    unsafe extern "C" {
        pub fn sin(x: f64) -> f64; pub fn cos(x: f64) -> f64; pub fn tan(x: f64) -> f64;
        pub fn asin(x: f64) -> f64; pub fn acos(x: f64) -> f64; pub fn atan(x: f64) -> f64; pub fn atan2(y: f64, x: f64) -> f64;
        pub fn sinh(x: f64) -> f64; pub fn cosh(x: f64) -> f64; pub fn tanh(x: f64) -> f64;
        pub fn asinh(x: f64) -> f64; pub fn acosh(x: f64) -> f64; pub fn atanh(x: f64) -> f64;
        pub fn exp(x: f64) -> f64; pub fn exp2(x: f64) -> f64; pub fn expm1(x: f64) -> f64;
        pub fn log(x: f64) -> f64; pub fn log2(x: f64) -> f64; pub fn log10(x: f64) -> f64; pub fn log1p(x: f64) -> f64;
        pub fn pow(x: f64, y: f64) -> f64; pub fn cbrt(x: f64) -> f64; pub fn hypot(x: f64, y: f64) -> f64;
        pub fn floor(x: f64) -> f64; pub fn ceil(x: f64) -> f64; pub fn trunc(x: f64) -> f64; pub fn round(x: f64) -> f64; pub fn rint(x: f64) -> f64;
        pub fn fmod(x: f64, y: f64) -> f64; pub fn remainder(x: f64, y: f64) -> f64; pub fn fma(x: f64, y: f64, z: f64) -> f64;
        pub fn nextafter(x: f64, y: f64) -> f64; pub fn copysign(x: f64, y: f64) -> f64;
        pub fn erf(x: f64) -> f64; pub fn erfc(x: f64) -> f64; pub fn tgamma(x: f64) -> f64; pub fn lgamma(x: f64) -> f64;
    }
    }
    pub fn m_sin(x: f64) -> f64 { unsafe { cm::sin(x) } }
    pub fn m_cos(x: f64) -> f64 { unsafe { cm::cos(x) } }
    pub fn m_tan(x: f64) -> f64 { unsafe { cm::tan(x) } }
    pub fn m_asin(x: f64) -> f64 { unsafe { cm::asin(x) } }
    pub fn m_acos(x: f64) -> f64 { unsafe { cm::acos(x) } }
    pub fn m_atan(x: f64) -> f64 { unsafe { cm::atan(x) } }
    pub fn m_atan2(x: f64, y: f64) -> f64 { unsafe { cm::atan2(x, y) } }
    pub fn m_sinh(x: f64) -> f64 { unsafe { cm::sinh(x) } }
    pub fn m_cosh(x: f64) -> f64 { unsafe { cm::cosh(x) } }
    pub fn m_tanh(x: f64) -> f64 { unsafe { cm::tanh(x) } }
    pub fn m_asinh(x: f64) -> f64 { unsafe { cm::asinh(x) } }
    pub fn m_acosh(x: f64) -> f64 { unsafe { cm::acosh(x) } }
    pub fn m_atanh(x: f64) -> f64 { unsafe { cm::atanh(x) } }
    pub fn m_exp(x: f64) -> f64 { unsafe { cm::exp(x) } }
    pub fn m_exp2(x: f64) -> f64 { unsafe { cm::exp2(x) } }
    pub fn m_expm1(x: f64) -> f64 { unsafe { cm::expm1(x) } }
    pub fn m_log(x: f64) -> f64 { unsafe { cm::log(x) } }
    pub fn m_log2(x: f64) -> f64 { unsafe { cm::log2(x) } }
    pub fn m_log10(x: f64) -> f64 { unsafe { cm::log10(x) } }
    pub fn m_log1p(x: f64) -> f64 { unsafe { cm::log1p(x) } }
    pub fn m_pow(x: f64, y: f64) -> f64 { unsafe { cm::pow(x, y) } }
    pub fn m_cbrt(x: f64) -> f64 { unsafe { cm::cbrt(x) } }
    pub fn m_hypot(x: f64, y: f64) -> f64 { unsafe { cm::hypot(x, y) } }
    pub fn m_floor(x: f64) -> f64 { unsafe { cm::floor(x) } }
    pub fn m_ceil(x: f64) -> f64 { unsafe { cm::ceil(x) } }
    pub fn m_trunc(x: f64) -> f64 { unsafe { cm::trunc(x) } }
    pub fn m_round(x: f64) -> f64 { unsafe { cm::round(x) } }
    pub fn m_rint(x: f64) -> f64 { unsafe { cm::rint(x) } }
    pub fn m_fmod(x: f64, y: f64) -> f64 { unsafe { cm::fmod(x, y) } }
    pub fn m_remainder(x: f64, y: f64) -> f64 { unsafe { cm::remainder(x, y) } }
    pub fn m_fma(x: f64, y: f64, z: f64) -> f64 { unsafe { cm::fma(x, y, z) } }
    pub fn m_nextafter(x: f64, y: f64) -> f64 { unsafe { cm::nextafter(x, y) } }
    pub fn m_copysign(x: f64, y: f64) -> f64 { unsafe { cm::copysign(x, y) } }
    pub fn m_erf(x: f64) -> f64 { unsafe { cm::erf(x) } }
    pub fn m_erfc(x: f64) -> f64 { unsafe { cm::erfc(x) } }
    pub fn m_tgamma(x: f64) -> f64 { unsafe { cm::tgamma(x) } }
    pub fn m_lgamma(x: f64) -> f64 { unsafe { cm::lgamma(x) } }
    pub fn f_to_i(x: f64, loc: &str) -> i64 {
        if x.is_nan() || x.is_infinite() {
            panic("Float#to_i of NaN or Infinity", loc)
        }
        if x >= 9223372036854775808.0 || x < -9223372036854775808.0 {
            panic("Float#to_i: out of Int range", loc)
        }
        x as i64
    }
    pub fn str_cat(parts: Vec<Str>) -> Str {
        let mut v = Vec::new();
        for p in &parts {
            v.extend_from_slice(&p.0);
        }
        Str::lit(&v)
    }

    /// Go's %x %X %o %b.
    pub fn int_fmt(v: i64, base: i64, upper: bool, is_u64: bool) -> Str {
        let neg = !is_u64 && v < 0;
        let m: u64 = if neg { (v as u64).wrapping_neg() } else { v as u64 };
        let mut s = match base {
            16 => format!("{m:x}"),
            8 => format!("{m:o}"),
            _ => format!("{m:b}"),
        };
        if upper {
            s = s.to_uppercase();
        }
        if neg {
            s.insert(0, '-');
        }
        Str::lit(s.as_bytes())
    }
    pub fn f_to_u64(x: f64, loc: &str) -> i64 {
        if x.is_nan() || x.is_infinite() {
            panic("Float#to_u64 of NaN or Infinity", loc)
        }
        if x < 0.0 || x >= 18446744073709551616.0 {
            panic("conversion overflow: the value doesn't fit U64", loc)
        }
        (x as u64) as i64
    }
    pub fn rune_to_s(r: i64) -> Str {
        let c = u32::try_from(r).ok().and_then(char::from_u32).unwrap_or('\u{FFFD}');
        Str::lit(c.to_string().as_bytes())
    }

    pub fn puts_i64(v: i64) {
        println!("{v}");
    }
    pub fn puts_str(s: Str) {
        use std::io::Write;
        let mut o = std::io::stdout();
        let _ = o.write_all(&s.0);
        if s.0.last() != Some(&b'\n') {
            let _ = o.write_all(b"\n");
        }
    }
    pub fn puts_bool(b: bool) {
        println!("{b}");
    }
    pub fn puts_pint(v: PInt) {
        println!("{}", v.to_string());
    }

    pub fn pmap<T: Clone + Sync, R: Clone + Send + Default>(xs: &Sl<T>, f: fn(T) -> R) -> Sl<R> {
        Sl::from(pmap_v(&xs.to_vec(), f))
    }
    fn pmap_v<T: Clone + Sync, R: Clone + Send + Default>(xs: &[T], f: fn(T) -> R) -> Vec<R> {
        let mut out = vec![R::default(); xs.len()];
        let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
        let chunk = xs.len().div_ceil(workers).max(1);
        std::thread::scope(|s| {
            for (src, dst) in xs.chunks(chunk).zip(out.chunks_mut(chunk)) {
                s.spawn(move || {
                    for (x, o) in src.iter().zip(dst) {
                        *o = f(x.clone());
                    }
                });
            }
        });
        out
    }

    // ---------- tasks and channels ----------

    use std::collections::VecDeque;
    use std::sync::{Condvar, Mutex, MutexGuard};

    fn lk<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A lock handle (alexandrite's `Mutex[T]` keeps its value beside it).
    #[derive(Clone)]
    pub struct AlxLock(Arc<(Mutex<bool>, Condvar)>);
    impl Default for AlxLock {
        fn default() -> Self {
            AlxLock(Arc::new((Mutex::new(false), Condvar::new())))
        }
    }
    impl AlxLock {
        pub fn lock(&self) {
            let mut held = lk(&self.0.0);
            while *held {
                held = self.0.1.wait(held).unwrap_or_else(|e| e.into_inner());
            }
            *held = true;
        }
        pub fn unlock(&self) {
            *lk(&self.0.0) = false;
            self.0.1.notify_one();
        }
    }
    /// An atomic cell handle.
    #[derive(Clone, Default)]
    pub struct AlxAtomic(Arc<std::sync::atomic::AtomicI64>);
    impl AlxAtomic {
        pub fn new(v: i64) -> Self {
            AlxAtomic(Arc::new(std::sync::atomic::AtomicI64::new(v)))
        }
        pub fn load(&self) -> i64 {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
        pub fn store(&self, v: i64) {
            self.0.store(v, std::sync::atomic::Ordering::SeqCst)
        }
        pub fn add(&self, v: i64) -> i64 {
            self.0.fetch_add(v, std::sync::atomic::Ordering::SeqCst).wrapping_add(v)
        }
        pub fn swap(&self, v: i64) -> i64 {
            self.0.swap(v, std::sync::atomic::Ordering::SeqCst)
        }
        pub fn cas(&self, old: i64, new: i64) -> bool {
            self.0.compare_exchange(old, new, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst).is_ok()
        }
    }

    struct TaskInner<T> {
        res: Mutex<Option<Result<T, Str>>>,
        cv: Condvar,
    }
    /// Handle to a spawned task; clones refer to the same task.
    pub struct Task<T>(Arc<TaskInner<T>>);
    impl<T> Clone for Task<T> {
        fn clone(&self) -> Self {
            Task(self.0.clone())
        }
    }
    /// The zero value: a task that already failed ("wait on a task that was never started").
    impl<T> Default for Task<T> {
        fn default() -> Self {
            Task(Arc::new(TaskInner {
                res: Mutex::new(Some(Err(Str::lit(b"alexandrite: wait on a task that was never started")))),
                cv: Condvar::new(),
            }))
        }
    }
    impl<T: Clone> Task<T> {
        /// Block until the task finishes: Ok(result) or Err(panic message).
        /// Repeatable.
        pub fn wait(&self) -> Result<T, Str> {
            let mut g = lk(&self.0.res);
            loop {
                if let Some(r) = &*g {
                    return r.clone();
                }
                g = self.0.cv.wait(g).unwrap_or_else(|e| e.into_inner());
            }
        }
    }

    fn panic_text(p: Box<dyn std::any::Any + Send>) -> String {
        match p.downcast::<String>() {
            Ok(s) => *s,
            Err(p) => match p.downcast::<&'static str>() {
                Ok(s) => format!("alexandrite: internal error: {s}"),
                Err(_) => "alexandrite: internal error".to_string(),
            },
        }
    }

    /// Run `f(env)` on a new thread. Alexandrite panics in the task unwind
    /// quietly (see `fail`) and become the task's `Err`; Rust's own panic
    /// message is suppressed on task threads by a hook installed once.
    pub fn task_spawn<E: Send + 'static, T: Send + 'static>(f: fn(E) -> T, env: E) -> Task<T> {
        static HOOK: std::sync::Once = std::sync::Once::new();
        HOOK.call_once(|| {
            let prev = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                if !IN_TASK.with(|t| t.get()) {
                    prev(info)
                }
            }));
        });
        let inner = Arc::new(TaskInner { res: Mutex::new(None), cv: Condvar::new() });
        let me = inner.clone();
        std::thread::spawn(move || {
            IN_TASK.with(|t| t.set(true));
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(env)));
            let r = r.map_err(|p| Str::lit(panic_text(p).as_bytes()));
            *lk(&me.res) = Some(r);
            me.cv.notify_all();
        });
        Task(inner)
    }

    struct ChanState<T> {
        buf: VecDeque<T>,
        cap: usize,
        closed: bool,
        /// Receivers blocked in `recv` or in a blocking `select`.
        recv_waiting: usize,
        /// Values ever taken / ever deposited by an unbuffered send (tickets).
        taken: u64,
        deposited: u64,
    }
    struct ChanInner<T> {
        st: Mutex<ChanState<T>>,
        cv: Condvar,
    }
    /// Channel handle; clones refer to the same channel.
    pub struct Chan<T>(Arc<ChanInner<T>>);
    impl<T> Clone for Chan<T> {
        fn clone(&self) -> Self {
            Chan(self.0.clone())
        }
    }
    /// The zero value: a closed, unbuffered channel (send panics, recv gives ok = false).
    impl<T> Default for Chan<T> {
        fn default() -> Self {
            let c = Chan::new(0);
            lk(&c.0.st).closed = true;
            c
        }
    }

    /// Every channel operation bumps this and wakes blocked selects.
    static EPOCH: (Mutex<u64>, Condvar) = (Mutex::new(0), Condvar::new());
    fn bump() {
        *lk(&EPOCH.0) += 1;
        EPOCH.1.notify_all();
    }
    fn epoch() -> u64 {
        *lk(&EPOCH.0)
    }
    fn epoch_wait(seen: u64) {
        let g = lk(&EPOCH.0);
        if *g == seen {
            // The timeout is a safety net, not the mechanism.
            let _ = EPOCH.1.wait_timeout(g, std::time::Duration::from_millis(20));
        }
    }
    fn closed_send() -> ! {
        fail("alexandrite: send on a closed channel".to_string())
    }

    impl<T> Chan<T> {
        pub fn new(cap: i64) -> Chan<T> {
            Chan(Arc::new(ChanInner {
                st: Mutex::new(ChanState { buf: VecDeque::new(), cap: cap.max(0) as usize, closed: false, recv_waiting: 0, taken: 0, deposited: 0 }),
                cv: Condvar::new(),
            }))
        }
        pub fn len(&self) -> i64 {
            let g = lk(&self.0.st);
            if g.cap == 0 { 0 } else { g.buf.len() as i64 }
        }
        pub fn close(&self, loc: &str) {
            let mut g = lk(&self.0.st);
            if g.closed {
                drop(g);
                panic("close of a closed channel", loc);
            }
            g.closed = true;
            drop(g);
            self.0.cv.notify_all();
            bump();
        }
        /// After depositing into an unbuffered channel: wait until taken.
        fn await_taken(&self, mut g: MutexGuard<'_, ChanState<T>>, ticket: u64) {
            loop {
                if g.taken > ticket {
                    return;
                }
                if g.closed {
                    // Undelivered: withdraw it and fail like Go's blocked sender.
                    g.buf.clear();
                    drop(g);
                    closed_send();
                }
                g = self.0.cv.wait(g).unwrap_or_else(|e| e.into_inner());
            }
        }
        pub fn send(&self, x: T, loc: &str) {
            let mut g = lk(&self.0.st);
            if g.cap == 0 {
                loop {
                    if g.closed {
                        drop(g);
                        panic("send on a closed channel", loc);
                    }
                    if g.buf.is_empty() {
                        break;
                    }
                    g = self.0.cv.wait(g).unwrap_or_else(|e| e.into_inner());
                }
                g.buf.push_back(x);
                let ticket = g.deposited;
                g.deposited += 1;
                self.0.cv.notify_all();
                bump();
                self.await_taken(g, ticket);
            } else {
                loop {
                    if g.closed {
                        drop(g);
                        panic("send on a closed channel", loc);
                    }
                    if g.buf.len() < g.cap {
                        break;
                    }
                    g = self.0.cv.wait(g).unwrap_or_else(|e| e.into_inner());
                }
                g.buf.push_back(x);
                drop(g);
                self.0.cv.notify_all();
                bump();
            }
        }
        /// Non-blocking send for `select`: Err(x) if not ready. (Unbuffered:
        /// ready only when a receiver is waiting; then blocks until taken.)
        fn try_send(&self, x: T) -> Result<(), T> {
            let mut g = lk(&self.0.st);
            if g.closed {
                drop(g);
                closed_send();
            }
            if g.cap == 0 {
                if !(g.buf.is_empty() && g.recv_waiting > 0) {
                    return Err(x);
                }
                g.buf.push_back(x);
                let ticket = g.deposited;
                g.deposited += 1;
                self.0.cv.notify_all();
                bump();
                self.await_taken(g, ticket);
            } else {
                if g.buf.len() >= g.cap {
                    return Err(x);
                }
                g.buf.push_back(x);
                drop(g);
                self.0.cv.notify_all();
                bump();
            }
            Ok(())
        }
        fn pop(&self, g: &mut MutexGuard<'_, ChanState<T>>) -> Option<T> {
            let x = g.buf.pop_front();
            if x.is_some() {
                g.taken += 1;
            }
            x
        }
        /// Blocking receive: Some(value), or None once closed and drained.
        pub fn recv(&self) -> Option<T> {
            let mut g = lk(&self.0.st);
            let mut registered = false;
            loop {
                if let Some(x) = self.pop(&mut g) {
                    if registered {
                        g.recv_waiting -= 1;
                    }
                    drop(g);
                    self.0.cv.notify_all();
                    bump();
                    return Some(x);
                }
                if g.closed {
                    if registered {
                        g.recv_waiting -= 1;
                    }
                    return None;
                }
                if !registered {
                    registered = true;
                    g.recv_waiting += 1;
                    // Unbuffered senders in `select` poll recv_waiting.
                    drop(g);
                    bump();
                    g = lk(&self.0.st);
                    continue;
                }
                g = self.0.cv.wait(g).unwrap_or_else(|e| e.into_inner());
            }
        }
        /// Non-blocking receive for `select`: Some(Some(v)) / Some(None) =
        /// closed and drained / None = not ready.
        fn try_recv(&self) -> Option<Option<T>> {
            let mut g = lk(&self.0.st);
            if let Some(x) = self.pop(&mut g) {
                drop(g);
                self.0.cv.notify_all();
                bump();
                return Some(Some(x));
            }
            if g.closed { Some(None) } else { None }
        }
        fn reg(&self, d: i64) {
            let mut g = lk(&self.0.st);
            g.recv_waiting = (g.recv_waiting as i64 + d) as usize;
        }
    }

    /// One case of a `select`, type-erased.
    pub trait SelCase {
        /// Try to run the case; true if it ran.
        fn try_run(&mut self) -> bool;
        /// Register (+1) / deregister (-1) as a blocked receiver.
        fn reg(&self, d: i64) {}
    }
    pub struct SelSend<T>(Chan<T>, Option<T>);
    impl<T> SelSend<T> {
        pub fn new(ch: Chan<T>, x: T) -> Self {
            SelSend(ch, Some(x))
        }
    }
    impl<T> SelCase for SelSend<T> {
        fn try_run(&mut self) -> bool {
            let x = self.1.take().unwrap();
            match self.0.try_send(x) {
                Ok(()) => true,
                Err(x) => {
                    self.1 = Some(x);
                    false
                }
            }
        }
    }
    pub struct SelRecv<T>(Chan<T>, Option<Option<T>>);
    impl<T> SelRecv<T> {
        pub fn new(ch: Chan<T>) -> Self {
            SelRecv(ch, None)
        }
        /// Some(value), or None if the channel was closed and drained.
        pub fn result(&mut self) -> Option<T> {
            self.1.take().unwrap()
        }
    }
    impl<T> SelCase for SelRecv<T> {
        fn try_run(&mut self) -> bool {
            match self.0.try_recv() {
                Some(r) => {
                    self.1 = Some(r);
                    true
                }
                None => false,
            }
        }
        fn reg(&self, d: i64) {
            self.0.reg(d)
        }
    }

    fn rand_u64() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        static S: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);
        let mut x = S.load(Relaxed);
        loop {
            let mut y = x;
            y ^= y << 13;
            y ^= y >> 7;
            y ^= y << 17;
            match S.compare_exchange_weak(x, y, Relaxed, Relaxed) {
                Ok(_) => return y,
                Err(c) => x = c,
            }
        }
    }

    /// Run one ready case (random among ready ones); returns its index, or
    /// `cases.len()` if none was ready and `default` is set; else blocks.
    pub fn select(cases: &mut [&mut dyn SelCase], default: bool) -> i64 {
        let n = cases.len();
        let mut order: Vec<usize> = (0..n).collect();
        for i in (1..n).rev() {
            order.swap(i, (rand_u64() % (i as u64 + 1)) as usize);
        }
        let mut registered = false;
        loop {
            let seen = epoch();
            for &i in &order {
                if cases[i].try_run() {
                    if registered {
                        for c in cases.iter() {
                            c.reg(-1);
                        }
                    }
                    return i as i64;
                }
            }
            if default {
                return n as i64;
            }
            if !registered {
                // Announce ourselves as a receiver (unbuffered senders need
                // one), wake pollers, and re-poll with a fresh snapshot.
                registered = true;
                for c in cases.iter() {
                    c.reg(1);
                }
                bump();
                continue;
            }
            epoch_wait(seen);
        }
    }

    // ---------- bignums: sign + magnitude in base 1e9 ----------

    const BASE: u64 = 1_000_000_000;

    #[derive(Clone, Default, Debug, PartialEq, Eq)]
    pub struct PInt {
        neg: bool,
        /// Little-endian base-1e9 digits; empty = zero.
        mag: Vec<u32>,
    }

    fn trim(m: &mut Vec<u32>) {
        while m.last() == Some(&0) {
            m.pop();
        }
    }
    fn cmp_mag(a: &[u32], b: &[u32]) -> Ordering {
        if a.len() != b.len() {
            return a.len().cmp(&b.len());
        }
        for i in (0..a.len()).rev() {
            if a[i] != b[i] {
                return a[i].cmp(&b[i]);
            }
        }
        Ordering::Equal
    }
    fn add_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
        let mut out = Vec::with_capacity(a.len().max(b.len()) + 1);
        let mut carry = 0u64;
        for i in 0..a.len().max(b.len()) {
            let s = carry + *a.get(i).unwrap_or(&0) as u64 + *b.get(i).unwrap_or(&0) as u64;
            out.push((s % BASE) as u32);
            carry = s / BASE;
        }
        if carry > 0 {
            out.push(carry as u32);
        }
        out
    }
    /// a - b where |a| >= |b|.
    fn sub_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
        let mut out = Vec::with_capacity(a.len());
        let mut borrow = 0i64;
        for i in 0..a.len() {
            let mut d = a[i] as i64 - borrow - *b.get(i).unwrap_or(&0) as i64;
            borrow = 0;
            if d < 0 {
                d += BASE as i64;
                borrow = 1;
            }
            out.push(d as u32);
        }
        trim(&mut out);
        out
    }
    fn mul_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
        if a.is_empty() || b.is_empty() {
            return vec![];
        }
        let mut out = vec![0u64; a.len() + b.len()];
        for (i, &x) in a.iter().enumerate() {
            let mut carry = 0u64;
            for (j, &y) in b.iter().enumerate() {
                let cur = out[i + j] + x as u64 * y as u64 + carry;
                out[i + j] = cur % BASE;
                carry = cur / BASE;
            }
            let mut k = i + b.len();
            while carry > 0 {
                let cur = out[k] + carry;
                out[k] = cur % BASE;
                carry = cur / BASE;
                k += 1;
            }
        }
        let mut m: Vec<u32> = out.into_iter().map(|d| d as u32).collect();
        trim(&mut m);
        m
    }

    impl PInt {
        pub fn from_i64(v: i64) -> PInt {
            let mut mag = vec![];
            let mut u = v.unsigned_abs();
            while u > 0 {
                mag.push((u % BASE) as u32);
                u /= BASE;
            }
            PInt { neg: v < 0, mag }
        }
        pub fn to_i64(&self, loc: &str) -> i64 {
            let mut u: i128 = 0;
            for &d in self.mag.iter().rev() {
                u = u * BASE as i128 + d as i128;
                if u > i64::MAX as i128 + 1 {
                    overflow(loc)
                }
            }
            let v = if self.neg { -u } else { u };
            i64::try_from(v).unwrap_or_else(|_| overflow(loc))
        }
        pub fn add(&self, o: &PInt) -> PInt {
            if self.neg == o.neg {
                return PInt { neg: self.neg, mag: add_mag(&self.mag, &o.mag) };
            }
            match cmp_mag(&self.mag, &o.mag) {
                Ordering::Less => PInt { neg: o.neg, mag: sub_mag(&o.mag, &self.mag) },
                _ => {
                    let mag = sub_mag(&self.mag, &o.mag);
                    PInt { neg: self.neg && !mag.is_empty(), mag }
                }
            }
        }
        pub fn sub(&self, o: &PInt) -> PInt {
            self.add(&PInt { neg: !o.neg && !o.mag.is_empty(), mag: o.mag.clone() })
        }
        pub fn mul(&self, o: &PInt) -> PInt {
            let mag = mul_mag(&self.mag, &o.mag);
            PInt { neg: (self.neg != o.neg) && !mag.is_empty(), mag }
        }
        pub fn pow(&self, e: &PInt, loc: &str) -> PInt {
            let mut e = e.to_i64(loc);
            if e < 0 {
                panic("negative exponent", loc)
            }
            let mut base = self.clone();
            let mut acc = PInt::from_i64(1);
            while e > 0 {
                if e & 1 == 1 {
                    acc = acc.mul(&base);
                }
                e >>= 1;
                if e > 0 {
                    base = base.mul(&base);
                }
            }
            acc
        }
        pub fn cmp(&self, o: &PInt) -> Ordering {
            match (self.neg, o.neg) {
                (false, true) => Ordering::Greater,
                (true, false) => Ordering::Less,
                (false, false) => cmp_mag(&self.mag, &o.mag),
                (true, true) => cmp_mag(&o.mag, &self.mag),
            }
        }
        pub fn even(&self) -> bool {
            self.mag.first().is_none_or(|d| d % 2 == 0)
        }
        pub fn to_string(&self) -> String {
            if self.mag.is_empty() {
                return "0".into();
            }
            let mut s = String::new();
            if self.neg {
                s.push('-');
            }
            s.push_str(&self.mag.last().unwrap().to_string());
            for d in self.mag.iter().rev().skip(1) {
                s.push_str(&format!("{d:09}"));
            }
            s
        }
        pub fn to_s(&self) -> Str {
            Str::lit(self.to_string().as_bytes())
        }
        pub fn digits(&self, loc: &str) -> Vec<PInt> {
            if self.neg {
                panic("`digits` of a negative number", loc)
            }
            self.to_string().bytes().rev().map(|b| PInt::from_i64((b - b'0') as i64)).collect()
        }
        pub fn div(&self, o: &PInt, loc: &str) -> PInt {
            PInt::from_i64(div(self.to_i64(loc), o.to_i64(loc), loc))
        }
        pub fn rem(&self, o: &PInt, loc: &str) -> PInt {
            PInt::from_i64(rem(self.to_i64(loc), o.to_i64(loc), loc))
        }
    }
}

use rt::*;
