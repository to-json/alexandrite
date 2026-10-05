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
        /// Write back bytes a C function changed (extern calls borrow a [Byte]
        /// as a plain byte buffer; the oracle holds every integer as i64).
        pub fn store_bytes(&self, b: &[u8]) where T: From<u8> {
            let mut g = self.buf.lock().unwrap();
            for (i, x) in b.iter().enumerate().take(self.len) {
                g[self.off + i] = T::from(*x);
            }
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
    pub fn str_join(a: Sl<Str>, sep: Str) -> Str {
        let mut out: Vec<u8> = vec![];
        for (i, x) in a.to_vec().iter().enumerate() {
            if i > 0 {
                out.extend_from_slice(&sep.0);
            }
            out.extend_from_slice(&x.0);
        }
        Str(out.into())
    }
    pub fn str_split(s: Str, sep: Str) -> Sl<Str> {
        Sl::from(str_split_v(s, sep))
    }
    fn str_split_v(s: Str, sep: Str) -> Vec<Str> {
        let mut out = vec![];
        let (b, p) = (&s.0[..], &sep.0[..]);
        if p.is_empty() {
            let mut i = 0;
            while i < b.len() {
                let n = utf8_seq_len(&b[i..]);
                out.push(Str::lit(&b[i..i + n]));
                i += n;
            }
            return out;
        }
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
        out
    }
    /// The length of the UTF-8 sequence at the start of `b` (1 for an invalid byte).
    fn utf8_seq_len(b: &[u8]) -> usize {
        let c = b[0];
        let n = if c < 0x80 { 1 } else if c >= 0xF0 { 4 } else if c >= 0xE0 { 3 } else if c >= 0xC0 { 2 } else { 1 };
        if n > b.len() || b[1..n].iter().any(|x| x & 0xC0 != 0x80) { 1 } else { n }
    }
    pub fn str_index(s: Str, sub: Str, from: i64) -> i64 {
        let from = from.max(0) as usize;
        if from > s.0.len() {
            return -1;
        }
        if sub.0.is_empty() {
            return from as i64;
        }
        match s.0[from..].windows(sub.0.len()).position(|w| w == &sub.0[..]) {
            Some(i) => (from + i) as i64,
            None => -1,
        }
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
    /// The valid UTF-8 sequence length at i, or 1 (decodes as U+FFFD).
    pub fn str_charlen(s: &Str, i: i64) -> i64 {
        let p = &s.0[i as usize..];
        match std::str::from_utf8(&p[..p.len().min(4)]) {
            Ok(t) => t.chars().next().map_or(1, |c| c.len_utf8() as i64),
            Err(e) if e.valid_up_to() > 0 => {
                let t = std::str::from_utf8(&p[..e.valid_up_to()]).unwrap();
                t.chars().next().map_or(1, |c| c.len_utf8() as i64)
            }
            Err(_) => 1,
        }
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

    // ---- C foreign functions (`extern def`) ----
    thread_local! {
        /// errno as it was right after the last extern call on this thread.
        static ERRNO: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
    }
    #[cfg(target_os = "macos")]
    unsafe extern "C" {
        #[link_name = "__error"]
        fn errno_loc() -> *mut i32;
    }
    #[cfg(not(target_os = "macos"))]
    unsafe extern "C" {
        #[link_name = "__errno_location"]
        fn errno_loc() -> *mut i32;
    }
    pub fn clear_errno() {
        unsafe { *errno_loc() = 0 };
    }
    pub fn save_errno() {
        let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        ERRNO.with(|c| c.set(e as i64));
    }
    pub fn errno() -> i64 {
        ERRNO.with(|c| c.get())
    }
    pub fn strerror_str(n: i64) -> Str {
        // std's formatting appends " (os error N)"; strip it to get C's text.
        let s = std::io::Error::from_raw_os_error(n as i32).to_string();
        let t = match s.rfind(" (os error") {
            Some(i) => &s[..i],
            None => &s,
        };
        Str::lit(t.as_bytes())
    }
    pub fn str_from_cstr(p: i64) -> Str {
        if p == 0 {
            return Str::lit(b"");
        }
        unsafe { Str::lit(std::ffi::CStr::from_ptr(p as usize as *const std::ffi::c_char).to_bytes()) }
    }
    pub fn str_from_ptr(p: i64, n: i64) -> Str {
        if p == 0 || n <= 0 {
            return Str::lit(b"");
        }
        unsafe { Str::lit(std::slice::from_raw_parts(p as usize as *const u8, n as usize)) }
    }
    unsafe extern "C" {
        #[link_name = "open"]
        fn libc_open(path: *const std::ffi::c_char, flags: i32, ...) -> i32;
        #[link_name = "fcntl"]
        fn libc_fcntl(fd: i32, cmd: i32, ...) -> i32;
    }
    // The C runtime's non-variadic wrappers (see alx.h).
    pub unsafe fn shim_alx_sys_open(path: *const std::ffi::c_char, flags: i32, mode: i32) -> i32 {
        unsafe { libc_open(path, flags, mode as u32) }
    }
    pub unsafe fn shim_alx_sys_fcntl(fd: i32, cmd: i32, arg: i64) -> i32 {
        unsafe { libc_fcntl(fd, cmd, arg as std::ffi::c_long) }
    }
    pub unsafe fn shim_alx_sys_const(name: *const std::ffi::c_char) -> i64 {
        let n = unsafe { std::ffi::CStr::from_ptr(name) }.to_bytes();
        super::sys_consts().iter().find(|(k, _)| k.as_bytes() == n).map_or(-1, |(_, v)| *v)
    }

    // The C runtime's program arguments, sleeping and clocks (alx.h).
    fn args_c() -> &'static [std::ffi::CString] {
        static A: std::sync::OnceLock<Vec<std::ffi::CString>> = std::sync::OnceLock::new();
        A.get_or_init(|| std::env::args().map(|a| std::ffi::CString::new(a).unwrap_or_default()).collect())
    }
    pub unsafe fn shim_alx_argc() -> i64 {
        args_c().len() as i64
    }
    pub unsafe fn shim_alx_argv(i: i64) -> *mut std::ffi::c_void {
        args_c().get(i as usize).map_or(std::ptr::null_mut(), |c| c.as_ptr() as *mut std::ffi::c_void)
    }
    pub unsafe fn shim_alx_sleep_ns(ns: i64) {
        if ns > 0 {
            std::thread::sleep(std::time::Duration::from_nanos(ns as u64));
        }
    }
    pub unsafe fn shim_alx_wall_ns() -> i64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as i64)
    }
    pub unsafe fn shim_alx_mono_ns() -> i64 {
        static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        T0.get_or_init(std::time::Instant::now).elapsed().as_nanos() as i64
    }
    #[repr(C)]
    struct Tm {
        f: [i32; 9],
        gmtoff: i64,
        zone: *const std::ffi::c_char,
    }
    unsafe extern "C" {
        fn localtime_r(t: *const i64, out: *mut Tm) -> *mut Tm;
    }
    fn local_tm(sec: i64) -> Option<Tm> {
        let mut tm = Tm { f: [0; 9], gmtoff: 0, zone: std::ptr::null() };
        let r = unsafe { localtime_r(&sec, &mut tm) };
        if r.is_null() { None } else { Some(tm) }
    }
    pub unsafe fn shim_alx_local_offset(sec: i64) -> i64 {
        local_tm(sec).map_or(0, |t| t.gmtoff)
    }
    pub unsafe fn shim_alx_local_zone(sec: i64) -> *mut std::ffi::c_void {
        match local_tm(sec) {
            Some(t) if !t.zone.is_null() => t.zone as *mut std::ffi::c_void,
            _ => c"UTC".as_ptr() as *mut std::ffi::c_void,
        }
    }

    // The C runtime's file and directory shims (alx.h), over std.
    fn cpath(p: *const std::ffi::c_char) -> std::path::PathBuf {
        use std::os::unix::ffi::OsStrExt;
        std::path::PathBuf::from(std::ffi::OsStr::from_bytes(unsafe { std::ffi::CStr::from_ptr(p) }.to_bytes()))
    }
    fn stat_fill(m: &std::fs::Metadata, out: *mut u8) {
        use std::os::unix::fs::MetadataExt;
        let v: [i64; 6] = [m.mode() as i64, m.size() as i64, m.mtime() * 1_000_000_000 + m.mtime_nsec(), m.atime() * 1_000_000_000 + m.atime_nsec(), m.ino() as i64, m.nlink() as i64];
        unsafe { std::ptr::copy_nonoverlapping(v.as_ptr() as *const u8, out, 48) };
    }
    fn neg_errno(e: &std::io::Error) -> i64 {
        -(e.raw_os_error().unwrap_or(5) as i64)
    }
    pub unsafe fn shim_alx_sys_stat(path: *const std::ffi::c_char, out: *mut u8, follow: i32) -> i32 {
        let p = cpath(path);
        match if follow != 0 { std::fs::metadata(p) } else { std::fs::symlink_metadata(p) } {
            Ok(m) => {
                stat_fill(&m, out);
                0
            }
            Err(e) => neg_errno(&e) as i32,
        }
    }
    pub unsafe fn shim_alx_sys_fstat(fd: i32, out: *mut u8) -> i32 {
        use std::os::fd::FromRawFd;
        if fd < 0 { return -sysc("EBADF"); }
        let f = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(fd) });
        match f.metadata() {
            Ok(m) => {
                stat_fill(&m, out);
                0
            }
            Err(e) => neg_errno(&e) as i32,
        }
    }
    struct DirH {
        it: std::fs::ReadDir,
        cur: std::ffi::CString,
    }
    pub unsafe fn shim_alx_sys_dir_open(path: *const std::ffi::c_char) -> i64 {
        match std::fs::read_dir(cpath(path)) {
            Ok(it) => Box::into_raw(Box::new(DirH { it, cur: std::ffi::CString::default() })) as usize as i64,
            Err(e) => neg_errno(&e),
        }
    }
    pub unsafe fn shim_alx_sys_dir_next(h: i64, kind: *mut u8) -> *mut std::ffi::c_void {
        use std::os::unix::ffi::OsStrExt;
        let d = unsafe { &mut *(h as usize as *mut DirH) };
        for e in d.it.by_ref() {
            let Ok(e) = e else { continue };
            let name = e.file_name();
            let k = match e.file_type() {
                Ok(t) if t.is_file() => 1,
                Ok(t) if t.is_dir() => 2,
                Ok(t) if t.is_symlink() => 3,
                Ok(_) => 4,
                Err(_) => 0,
            };
            unsafe { *kind = k };
            d.cur = std::ffi::CString::new(name.as_bytes()).unwrap_or_default();
            return d.cur.as_ptr() as *mut std::ffi::c_void;
        }
        std::ptr::null_mut()
    }
    pub unsafe fn shim_alx_sys_dir_close(h: i64) {
        drop(unsafe { Box::from_raw(h as usize as *mut DirH) });
    }
    unsafe extern "C" {
        static environ: *const *const std::ffi::c_char;
    }
    pub unsafe fn shim_alx_environ(i: i64) -> *mut std::ffi::c_void {
        unsafe {
            if environ.is_null() {
                return std::ptr::null_mut();
            }
            for k in 0..=i {
                if (*environ.offset(k as isize)).is_null() {
                    return std::ptr::null_mut();
                }
            }
            *environ.offset(i as isize) as *mut std::ffi::c_void
        }
    }

    // L3: the C runtime's event loop and socket shims (alx.h), over std::net.
    // Tasks are OS threads here, so waiting is a plain poll(2).
    #[repr(C)]
    struct PollFd {
        fd: i32,
        events: i16,
        revents: i16,
    }
    unsafe extern "C" {
        fn poll(fds: *mut PollFd, n: std::ffi::c_ulong, ms: i32) -> i32;
    }
    // Processes (std os/exec): the C runtime's spawn/wait/pipe shims.
    fn sysc(name: &str) -> i32 {
        super::sys_consts().iter().find(|(k, _)| *k == name).map_or(-1, |(_, v)| *v as i32)
    }
    unsafe fn nul_list(p: *const u8, n: i64) -> Vec<std::ffi::OsString> {
        use std::os::unix::ffi::OsStrExt;
        let mut out = vec![];
        let mut p = p;
        for _ in 0..n {
            let c = unsafe { std::ffi::CStr::from_ptr(p as *const std::ffi::c_char) };
            out.push(std::ffi::OsStr::from_bytes(c.to_bytes()).to_os_string());
            p = unsafe { p.add(c.to_bytes().len() + 1) };
        }
        out
    }
    unsafe extern "C" {
        fn pipe(fds: *mut i32) -> i32;
        fn wait4(pid: i32, status: *mut i32, options: i32, ru: *mut i64) -> i32;
    }
    /// A child's stdio from `fd` (negative: inherit): a close-on-exec
    /// duplicate, or the negated errno if it can't be duplicated.
    fn child_stdio(fd: i64) -> Result<std::process::Stdio, i64> {
        use std::os::fd::FromRawFd;
        if fd < 0 {
            return Ok(std::process::Stdio::inherit());
        }
        let d = unsafe { libc_fcntl(fd as i32, sysc("F_DUPFD_CLOEXEC"), 3) };
        if d < 0 {
            return Err(neg_errno(&std::io::Error::last_os_error()));
        }
        Ok(std::process::Stdio::from(unsafe { std::fs::File::from_raw_fd(d) }))
    }
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn shim_alx_sys_spawn(argv: *mut u8, argc: i64, env: *mut u8, envc: i64, dir: *const std::ffi::c_char, fd0: i64, fd1: i64, fd2: i64) -> i64 {
        let av = unsafe { nul_list(argv, argc) };
        let mut cmd = std::process::Command::new(&av[0]);
        cmd.args(&av[1..]);
        if envc >= 0 {
            cmd.env_clear();
            for kv in unsafe { nul_list(env, envc) } {
                let kv = kv.to_string_lossy().into_owned();
                let (k, v) = kv.split_once('=').unwrap_or((&kv, ""));
                cmd.env(k, v);
            }
        }
        let d = cpath(dir);
        if !d.as_os_str().is_empty() {
            cmd.current_dir(d);
        }
        let (i0, i1, i2) = match (child_stdio(fd0), child_stdio(fd1), child_stdio(fd2)) {
            (Ok(a), Ok(b), Ok(c)) => (a, b, c),
            (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return e,
        };
        cmd.stdin(i0).stdout(i1).stderr(i2);
        match cmd.spawn() {
            Ok(c) => c.id() as i64,
            Err(e) => neg_errno(&e),
        }
    }
    pub unsafe fn shim_alx_sys_wait(pid: i64, out: *mut u8) -> i64 {
        let mut st = 0i32;
        let mut ru = [0i64; 18];
        loop {
            if unsafe { wait4(pid as i32, &mut st, 0, ru.as_mut_ptr()) } >= 0 {
                break;
            }
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return neg_errno(&e);
            }
        }
        // struct timeval: tv_sec (8 bytes), tv_usec (an int32 on macOS).
        let us = |i: usize| (ru[i] & 0xffff_ffff) as i32 as i64;
        let rss = if cfg!(target_os = "macos") { ru[4] } else { ru[4] * 1024 };
        let sig = st & 0x7f;
        let killed = sig != 0 && sig != 0x7f;
        let v = [killed as i64, if killed { sig as i64 } else { ((st >> 8) & 0xff) as i64 }, ru[0] * 1_000_000_000 + us(1) * 1000, ru[2] * 1_000_000_000 + us(3) * 1000, rss];
        unsafe { std::ptr::copy_nonoverlapping(v.as_ptr() as *const u8, out, 40) };
        0
    }
    pub unsafe fn shim_alx_sys_exec(argv: *mut u8, argc: i64, env: *mut u8, envc: i64, dir: *const std::ffi::c_char, fd0: i64, fd1: i64, fd2: i64) -> i64 {
        use std::os::unix::process::CommandExt;
        let av = unsafe { nul_list(argv, argc) };
        let mut cmd = std::process::Command::new(&av[0]);
        cmd.args(&av[1..]);
        if envc >= 0 {
            cmd.env_clear();
            for kv in unsafe { nul_list(env, envc) } {
                let kv = kv.to_string_lossy().into_owned();
                let (k, v) = kv.split_once('=').unwrap_or((&kv, ""));
                cmd.env(k, v);
            }
        }
        let d = cpath(dir);
        if !d.as_os_str().is_empty() {
            cmd.current_dir(d);
        }
        let (i0, i1, i2) = match (child_stdio(fd0), child_stdio(fd1), child_stdio(fd2)) {
            (Ok(a), Ok(b), Ok(c)) => (a, b, c),
            (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return e,
        };
        cmd.stdin(i0).stdout(i1).stderr(i2);
        use std::io::Write;
        let _ = std::io::stdout().flush();
        neg_errno(&cmd.exec())
    }
    pub unsafe fn shim_alx_sys_pipe(out: *mut u8, nonblock: i64) -> i64 {
        let mut p = [0i32; 2];
        if unsafe { pipe(p.as_mut_ptr()) } != 0 {
            return neg_errno(&std::io::Error::last_os_error());
        }
        unsafe {
            libc_fcntl(p[0], sysc("F_SETFD"), sysc("FD_CLOEXEC"));
            libc_fcntl(p[1], sysc("F_SETFD"), sysc("FD_CLOEXEC"));
            if nonblock != 0 {
                let fl = libc_fcntl(p[0], sysc("F_GETFL"));
                libc_fcntl(p[0], sysc("F_SETFL"), fl | sysc("O_NONBLOCK"));
            }
        }
        let v = [p[0] as i64, p[1] as i64];
        unsafe { std::ptr::copy_nonoverlapping(v.as_ptr() as *const u8, out, 16) };
        0
    }
    pub unsafe fn shim_alx_sys_poll2(a: i64, b: i64) -> i64 {
        let mut fds = [PollFd { fd: a as i32, events: 1, revents: 0 }, PollFd { fd: b as i32, events: 1, revents: 0 }];
        while unsafe { poll(fds.as_mut_ptr(), 2, -1) } < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return neg_errno(&e);
            }
        }
        0
    }
    pub unsafe fn shim_alx_fd_wait(fd: i64, mode: i64) -> i64 {
        // In slices: closing a descriptor in another thread doesn't wake a
        // poll on it (macOS), so check now and then that it's still open.
        loop {
            let mut p = PollFd { fd: fd as i32, events: if mode == 1 { 1 } else { 4 }, revents: 0 };
            let r = unsafe { poll(&mut p, 1, 50) };
            if r > 0 {
                return 0;
            }
            if r < 0 {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() != Some(4) {
                    return neg_errno(&e);
                }
            }
            // F_GETFD (1) fails with EBADF once the descriptor is closed.
            if unsafe { libc_fcntl(fd as i32, 1) } < 0 {
                return 0;
            }
        }
    }
    pub unsafe fn shim_alx_fd_close(fd: i64) -> i64 {
        if unsafe { close(fd as i32) } == 0 { 0 } else { neg_errno(&std::io::Error::last_os_error()) }
    }
    fn net_err(e: &std::io::Error) -> i64 {
        match e.raw_os_error() {
            Some(c) => -(c as i64),
            None => -100000,
        }
    }
    fn put_addr(a: std::net::SocketAddr, out: *mut u8) -> i64 {
        let s = a.to_string();
        unsafe {
            std::ptr::copy_nonoverlapping(s.as_ptr(), out, s.len());
            *out.add(s.len()) = 0;
        }
        s.len() as i64
    }
    fn host_str(h: *const std::ffi::c_char) -> String {
        unsafe { std::ffi::CStr::from_ptr(h) }.to_string_lossy().into_owned()
    }
    pub unsafe fn shim_alx_sock_listen(host: *const std::ffi::c_char, port: i64, _backlog: i64) -> i64 {
        use std::os::fd::IntoRawFd;
        let mut h = host_str(host);
        if h.is_empty() {
            h = "0.0.0.0".into();
        }
        match std::net::TcpListener::bind((h.as_str(), port as u16)).and_then(|l| l.set_nonblocking(true).map(|_| l)) {
            Ok(l) => l.into_raw_fd() as i64,
            Err(e) => net_err(&e),
        }
    }
    pub unsafe fn shim_alx_sock_accept(fd: i64, out: *mut u8) -> i64 {
        use std::os::fd::{FromRawFd, IntoRawFd};
        if fd < 0 { return -(sysc("EBADF") as i64); }
        let l = std::mem::ManuallyDrop::new(unsafe { std::net::TcpListener::from_raw_fd(fd as i32) });
        match l.accept() {
            Ok((s, a)) => {
                let _ = s.set_nonblocking(true);
                put_addr(a, out);
                s.into_raw_fd() as i64
            }
            Err(e) => net_err(&e),
        }
    }
    pub unsafe fn shim_alx_sock_connect(host: *const std::ffi::c_char, port: i64) -> i64 {
        use std::net::ToSocketAddrs;
        use std::os::fd::IntoRawFd;
        let mut h = host_str(host);
        if h.is_empty() {
            h = "127.0.0.1".into();
        }
        let addrs: Vec<_> = match (h.as_str(), port as u16).to_socket_addrs() {
            Ok(a) => a.collect(),
            Err(_) => return -100000,
        };
        let Some(a) = addrs.iter().find(|a| a.is_ipv4()).or(addrs.first()) else { return -100000 };
        match std::net::TcpStream::connect(a).and_then(|s| s.set_nonblocking(true).map(|_| s)) {
            Ok(s) => s.into_raw_fd() as i64,
            Err(e) => net_err(&e),
        }
    }
    fn with_stream<T>(fd: i64, f: impl FnOnce(&std::net::TcpStream) -> T) -> T {
        use std::os::fd::FromRawFd;
        let s = std::mem::ManuallyDrop::new(unsafe { std::net::TcpStream::from_raw_fd(fd as i32) });
        f(&s)
    }
    pub unsafe fn shim_alx_sock_error(fd: i64) -> i64 {
        if fd < 0 { return sysc("EBADF") as i64; }
        with_stream(fd, |s| match s.take_error() {
            Ok(None) => 0,
            Ok(Some(e)) => e.raw_os_error().unwrap_or(5) as i64,
            Err(e) => e.raw_os_error().unwrap_or(5) as i64,
        })
    }
    pub unsafe fn shim_alx_sock_local_addr(fd: i64, out: *mut u8) -> i64 {
        if fd < 0 { return -(sysc("EBADF") as i64); }
        with_stream(fd, |s| match s.local_addr() {
            Ok(a) => put_addr(a, out),
            Err(e) => net_err(&e),
        })
    }
    pub unsafe fn shim_alx_sock_peer_addr(fd: i64, out: *mut u8) -> i64 {
        if fd < 0 { return -(sysc("EBADF") as i64); }
        with_stream(fd, |s| match s.peer_addr() {
            Ok(a) => put_addr(a, out),
            Err(e) => net_err(&e),
        })
    }
    pub unsafe fn shim_alx_sock_set_nodelay(fd: i64, on: i64) -> i64 {
        if fd < 0 { return -(sysc("EBADF") as i64); }
        with_stream(fd, |s| match s.set_nodelay(on != 0) {
            Ok(()) => 0,
            Err(e) => net_err(&e),
        })
    }
    pub unsafe fn shim_alx_sock_shutdown(fd: i64, how: i64) -> i64 {
        if fd < 0 { return -(sysc("EBADF") as i64); }
        let h = match how {
            0 => std::net::Shutdown::Read,
            1 => std::net::Shutdown::Write,
            _ => std::net::Shutdown::Both,
        };
        with_stream(fd, |s| match s.shutdown(h) {
            Ok(()) => 0,
            Err(e) => net_err(&e),
        })
    }
    pub unsafe fn shim_alx_sock_lookup(host: *const std::ffi::c_char, out: *mut u8, n: i64) -> i64 {
        use std::net::ToSocketAddrs;
        let h = host_str(host);
        let Ok(addrs) = (h.as_str(), 0u16).to_socket_addrs() else { return -100000 };
        let mut len = 0usize;
        for a in addrs {
            let s = a.ip().to_string();
            if len + s.len() + 1 > n as usize {
                break;
            }
            unsafe {
                std::ptr::copy_nonoverlapping(s.as_ptr(), out.add(len), s.len());
                *out.add(len + s.len()) = b'\n';
            }
            len += s.len() + 1;
        }
        len as i64
    }

    // Signals (std os/signal): the C runtime's self-pipe watchers (alx.c).
    unsafe extern "C" {
        fn signal(sig: i32, handler: usize) -> usize;
        fn write(fd: i32, buf: *const u8, n: usize) -> isize;
    }
    const SIGW: usize = 64;
    static SIG_MASK: [std::sync::atomic::AtomicU64; SIGW] = [const { std::sync::atomic::AtomicU64::new(0) }; SIGW];
    static SIG_WFD: [std::sync::atomic::AtomicI32; SIGW] = [const { std::sync::atomic::AtomicI32::new(-1) }; SIGW];
    /// Handlers running right now: `unwatch` waits for none before it closes a pipe.
    static SIG_BUSY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    // (read fds per slot, handled, ignored)
    static SIG_STATE: std::sync::Mutex<([i32; SIGW], u64, u64)> = std::sync::Mutex::new(([-1; SIGW], 0, 0));
    extern "C" fn sig_handler(sig: i32) {
        use std::sync::atomic::Ordering::SeqCst;
        // write(2) may set errno: the interrupted code must not see that.
        let saved = unsafe { *errno_loc() };
        SIG_BUSY.fetch_add(1, SeqCst);
        let bit = 1u64 << sig;
        for i in 0..SIGW {
            if SIG_MASK[i].load(SeqCst) & bit != 0 {
                let fd = SIG_WFD[i].load(SeqCst);
                let b = sig as u8;
                if fd >= 0 {
                    unsafe { write(fd, &b, 1) };
                }
            }
        }
        SIG_BUSY.fetch_sub(1, SeqCst);
        unsafe { *errno_loc() = saved };
    }
    fn sig_catchable(s: i64) -> bool {
        s != sysc("SIGKILL") as i64 && s != sysc("SIGSTOP") as i64
    }
    pub unsafe fn shim_alx_sig_watch(mask: i64) -> i64 {
        use std::sync::atomic::Ordering::SeqCst;
        let mut p = [0i32; 2];
        if unsafe { pipe(p.as_mut_ptr()) } != 0 {
            return neg_errno(&std::io::Error::last_os_error());
        }
        for fd in p {
            unsafe {
                libc_fcntl(fd, sysc("F_SETFD"), sysc("FD_CLOEXEC"));
                let fl = libc_fcntl(fd, sysc("F_GETFL"));
                libc_fcntl(fd, sysc("F_SETFL"), fl | sysc("O_NONBLOCK"));
            }
        }
        let mut st = SIG_STATE.lock().unwrap();
        let Some(slot) = (0..SIGW).find(|&i| st.0[i] < 0) else {
            unsafe {
                close(p[0]);
                close(p[1]);
            }
            return -(sysc("EMFILE") as i64);
        };
        st.0[slot] = p[0];
        SIG_WFD[slot].store(p[1], SeqCst);
        SIG_MASK[slot].store(mask as u64, SeqCst);
        for s in 1..64 {
            let bit = 1u64 << s;
            if mask as u64 & bit != 0 && st.1 & bit == 0 && sig_catchable(s) {
                unsafe { signal(s as i32, sig_handler as usize) };
                st.1 |= bit;
                st.2 &= !bit;
            }
        }
        p[0] as i64
    }
    pub unsafe fn shim_alx_sig_unwatch(rfd: i64) -> i64 {
        use std::sync::atomic::Ordering::SeqCst;
        let mut st = SIG_STATE.lock().unwrap();
        for i in 0..SIGW {
            if st.0[i] == rfd as i32 {
                SIG_MASK[i].store(0, SeqCst);
                let w = SIG_WFD[i].swap(-1, SeqCst);
                if w >= 0 {
                    // A handler that read the old descriptor is still counted in.
                    while SIG_BUSY.load(SeqCst) != 0 {
                        std::hint::spin_loop();
                    }
                    unsafe { close(w) };
                }
                st.0[i] = -1;
            }
        }
        let live = SIG_MASK.iter().fold(0u64, |a, m| a | m.load(SeqCst));
        for s in 1..64 {
            let bit = 1u64 << s;
            if st.1 & bit != 0 && live & bit == 0 {
                unsafe { signal(s, 0) };
                st.1 &= !bit;
            }
        }
        0
    }
    pub unsafe fn shim_alx_sig_reset(mask: i64, how: i64) -> i64 {
        use std::sync::atomic::Ordering::SeqCst;
        let mut st = SIG_STATE.lock().unwrap();
        for m in &SIG_MASK {
            m.fetch_and(!(mask as u64), SeqCst);
        }
        for s in 1..64 {
            let bit = 1u64 << s;
            if mask as u64 & bit == 0 || !sig_catchable(s) {
                continue;
            }
            if how != 0 {
                unsafe { signal(s as i32, 1) };
                st.2 |= bit;
            } else {
                if st.1 & bit != 0 {
                    unsafe { signal(s as i32, 0) };
                }
                st.2 &= !bit;
            }
            st.1 &= !bit;
        }
        0
    }
    pub unsafe fn shim_alx_sig_ignored(sig: i64) -> i64 {
        if !(1..64).contains(&sig) {
            return 0;
        }
        let st = SIG_STATE.lock().unwrap();
        if (st.2 >> sig) & 1 != 0 {
            return 1;
        }
        if (st.1 >> sig) & 1 != 0 {
            return 0;
        }
        // Ask without changing it: set SIG_IGN, look at the old one, put it back.
        let old = unsafe { signal(sig as i32, 1) };
        unsafe { signal(sig as i32, old) };
        (old == 1) as i64
    }

    // Users and groups (std os/user): the C runtime's lookups, over the
    // reentrant libc calls. struct passwd / group layouts per platform.
    #[repr(C)]
    #[cfg(target_os = "macos")]
    struct Passwd {
        name: *const std::ffi::c_char,
        passwd: *const std::ffi::c_char,
        uid: u32,
        gid: u32,
        change: i64,
        class: *const std::ffi::c_char,
        gecos: *const std::ffi::c_char,
        dir: *const std::ffi::c_char,
        shell: *const std::ffi::c_char,
        expire: i64,
    }
    #[repr(C)]
    #[cfg(not(target_os = "macos"))]
    struct Passwd {
        name: *const std::ffi::c_char,
        passwd: *const std::ffi::c_char,
        uid: u32,
        gid: u32,
        gecos: *const std::ffi::c_char,
        dir: *const std::ffi::c_char,
        shell: *const std::ffi::c_char,
    }
    #[repr(C)]
    struct Group {
        name: *const std::ffi::c_char,
        passwd: *const std::ffi::c_char,
        gid: u32,
        mem: *const *const std::ffi::c_char,
    }
    unsafe extern "C" {
        fn getpwuid_r(uid: u32, pw: *mut Passwd, buf: *mut u8, n: usize, res: *mut *mut Passwd) -> i32;
        fn getpwnam_r(name: *const std::ffi::c_char, pw: *mut Passwd, buf: *mut u8, n: usize, res: *mut *mut Passwd) -> i32;
        fn getgrgid_r(gid: u32, gr: *mut Group, buf: *mut u8, n: usize, res: *mut *mut Group) -> i32;
        fn getgrnam_r(name: *const std::ffi::c_char, gr: *mut Group, buf: *mut u8, n: usize, res: *mut *mut Group) -> i32;
        fn getgrouplist(name: *const std::ffi::c_char, gid: u32, groups: *mut u32, n: *mut i32) -> i32;
    }
    fn cstr_or_empty(p: *const std::ffi::c_char) -> Vec<u8> {
        if p.is_null() { vec![] } else { unsafe { std::ffi::CStr::from_ptr(p) }.to_bytes().to_vec() }
    }
    unsafe fn put_fields(out: *mut u8, n: i64, fs: &[Vec<u8>]) -> i64 {
        let need: usize = fs.iter().map(|f| f.len() + 1).sum();
        if need as i64 > n {
            return -(sysc("ERANGE") as i64);
        }
        let mut at = 0;
        for f in fs {
            unsafe {
                std::ptr::copy_nonoverlapping(f.as_ptr(), out.add(at), f.len());
                *out.add(at + f.len()) = 0;
            }
            at += f.len() + 1;
        }
        at as i64
    }
    pub unsafe fn shim_alx_user_lookup(kind: i64, key: *const std::ffi::c_char, out: *mut u8, n: i64) -> i64 {
        let k = unsafe { std::ffi::CStr::from_ptr(key) };
        let num: u32 = k.to_str().ok().and_then(|s| s.parse().ok()).unwrap_or(u32::MAX);
        let mut bl = 16384usize;
        loop {
            let mut buf = vec![0u8; bl];
            let (rc, r) = if kind <= 1 {
                let mut pw: Passwd = unsafe { std::mem::zeroed() };
                let mut res: *mut Passwd = std::ptr::null_mut();
                let rc = unsafe { if kind == 0 { getpwuid_r(num, &mut pw, buf.as_mut_ptr(), bl, &mut res) } else { getpwnam_r(key, &mut pw, buf.as_mut_ptr(), bl, &mut res) } };
                let mut r = 0;
                if rc == 0 && !res.is_null() {
                    let mut gecos = cstr_or_empty(pw.gecos);
                    if let Some(i) = gecos.iter().position(|&c| c == b',') {
                        gecos.truncate(i);
                    }
                    let fs = [pw.uid.to_string().into_bytes(), pw.gid.to_string().into_bytes(), cstr_or_empty(pw.name), gecos, cstr_or_empty(pw.dir)];
                    r = unsafe { put_fields(out, n, &fs) };
                }
                (rc, r)
            } else {
                let mut gr: Group = unsafe { std::mem::zeroed() };
                let mut res: *mut Group = std::ptr::null_mut();
                let rc = unsafe { if kind == 2 { getgrgid_r(num, &mut gr, buf.as_mut_ptr(), bl, &mut res) } else { getgrnam_r(key, &mut gr, buf.as_mut_ptr(), bl, &mut res) } };
                let mut r = 0;
                if rc == 0 && !res.is_null() {
                    let fs = [gr.gid.to_string().into_bytes(), cstr_or_empty(gr.name)];
                    r = unsafe { put_fields(out, n, &fs) };
                }
                (rc, r)
            };
            if rc == sysc("ERANGE") && bl < (1 << 22) {
                bl *= 4;
                continue;
            }
            let quiet = [0, sysc("ENOENT"), sysc("ESRCH"), sysc("EBADF"), sysc("EPERM")];
            return if quiet.contains(&rc) { r } else { -(rc as i64) };
        }
    }
    pub unsafe fn shim_alx_user_groups(name: *const std::ffi::c_char, gid: i64, out: *mut u8, n: i64) -> i64 {
        let mut cap = 64i32;
        loop {
            let mut gs = vec![0u32; cap as usize];
            let mut cnt = cap;
            let rc = unsafe { getgrouplist(name, gid as u32, gs.as_mut_ptr(), &mut cnt) };
            if rc < 0 && cap < 65536 {
                cap *= 4;
                continue;
            }
            for i in 0..(cnt.min(n as i32)) as usize {
                unsafe { std::ptr::copy_nonoverlapping((gs[i] as i64).to_ne_bytes().as_ptr(), out.add(8 * i), 8) };
            }
            return cnt as i64;
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
    /// `%.Ne` as Go (and C) print it: at least two exponent digits.
    pub fn fmt_e_string(x: f64, digits: i64, upper: bool) -> String {
        if x.is_nan() {
            return "NaN".into();
        }
        if x.is_infinite() {
            return (if x < 0.0 { "-Inf" } else { "+Inf" }).into();
        }
        let r = format!("{:.*e}", digits.max(0) as usize, x);
        let (m, e) = r.split_once('e').unwrap();
        let (sign, digs) = match e.strip_prefix('-') {
            Some(d) => ('-', d),
            None => ('+', e),
        };
        let out = format!("{m}e{sign}{digs:0>2}");
        if upper { out.to_uppercase() } else { out }
    }
    /// Pad to `width` runes: flags 1 = on the right, 2 = zeros after a sign.
    pub fn pad_bytes(s: &[u8], width: i64, flags: i64) -> Vec<u8> {
        let runes = s.iter().filter(|b| (**b & 0xC0) != 0x80).count() as i64;
        if runes >= width {
            return s.to_vec();
        }
        let pad = (width - runes) as usize;
        let mut out = Vec::with_capacity(s.len() + pad);
        if flags & 1 != 0 {
            out.extend_from_slice(s);
            out.extend(std::iter::repeat_n(b' ', pad));
        } else if flags & 2 != 0 {
            let sign = usize::from(matches!(s.first(), Some(b'-' | b'+' | b' ')));
            out.extend_from_slice(&s[..sign]);
            out.extend(std::iter::repeat_n(b'0', pad));
            out.extend_from_slice(&s[sign..]);
        } else {
            out.extend(std::iter::repeat_n(b' ', pad));
            out.extend_from_slice(s);
        }
        out
    }
    /// Go's strconv.Quote (see alx_str_quote in the C runtime).
    pub fn quote_bytes(s: &[u8]) -> Vec<u8> {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut o = vec![b'"'];
        let mut i = 0;
        while i < s.len() {
            let c = s[i];
            if c < 0x80 {
                let e: Option<&[u8]> = match c {
                    7 => Some(b"\\a"),
                    8 => Some(b"\\b"),
                    12 => Some(b"\\f"),
                    b'\n' => Some(b"\\n"),
                    b'\r' => Some(b"\\r"),
                    b'\t' => Some(b"\\t"),
                    11 => Some(b"\\v"),
                    b'\\' => Some(b"\\\\"),
                    b'"' => Some(b"\\\""),
                    _ => None,
                };
                match e {
                    Some(e) => o.extend_from_slice(e),
                    None if c < 0x20 || c == 0x7f => o.extend_from_slice(&[b'\\', b'x', HEX[(c >> 4) as usize], HEX[(c & 15) as usize]]),
                    None => o.push(c),
                }
                i += 1;
                continue;
            }
            let n = match std::str::from_utf8(&s[i..(i + 4).min(s.len())]) {
                Ok(t) => t.chars().next().map_or(0, char::len_utf8),
                Err(e) if e.valid_up_to() > 0 => std::str::from_utf8(&s[i..i + e.valid_up_to()]).unwrap().chars().next().map_or(0, char::len_utf8),
                Err(_) => 0,
            };
            if n == 0 {
                o.extend_from_slice(&[b'\\', b'x', HEX[(c >> 4) as usize], HEX[(c & 15) as usize]]);
                i += 1;
                continue;
            }
            if n == 2 && c == 0xC2 && s[i + 1] < 0xA0 {
                let r = s[i + 1];
                o.extend_from_slice(&[b'\\', b'u', b'0', b'0', HEX[(r >> 4) as usize], HEX[(r & 15) as usize]]);
            } else {
                o.extend_from_slice(&s[i..i + n]);
            }
            i += n;
        }
        o.push(b'"');
        o
    }
    pub fn f_fmt_e(x: f64, digits: i64, upper: bool) -> Str {
        Str::lit(fmt_e_string(x, digits, upper).as_bytes())
    }
    pub fn str_pad(s: Str, width: i64, flags: i64) -> Str {
        Str(pad_bytes(&s.0, width, flags).into())
    }
    pub fn str_quote(s: Str) -> Str {
        Str(quote_bytes(&s.0).into())
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
    pub fn print_str(s: Str) {
        use std::io::Write;
        let _ = std::io::stdout().write_all(&s.0);
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
        /// Str#to_i in promote mode: Ruby's rules, any number of digits.
        pub fn from_str_ruby(s: &Str) -> PInt {
            let b: &[u8] = &s.0;
            let mut i = 0;
            while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n') {
                i += 1;
            }
            let mut neg = false;
            if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
                neg = b[i] == b'-';
                i += 1;
            }
            let mut r = PInt::from_i64(0);
            let ten = PInt::from_i64(10);
            while i < b.len() && b[i].is_ascii_digit() {
                r = r.mul(&ten).add(&PInt::from_i64((b[i] - b'0') as i64));
                i += 1;
            }
            if neg { PInt::from_i64(0).sub(&r) } else { r }
        }
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
            // A divisor of ±1 is exact at any size (MIN / -1 promotes).
            if o.mag == [1] {
                return PInt { neg: (self.neg != o.neg) && !self.mag.is_empty(), mag: self.mag.clone() };
            }
            PInt::from_i64(div(self.to_i64(loc), o.to_i64(loc), loc))
        }
        pub fn rem(&self, o: &PInt, loc: &str) -> PInt {
            PInt::from_i64(rem(self.to_i64(loc), o.to_i64(loc), loc))
        }
    }
}

use rt::*;
