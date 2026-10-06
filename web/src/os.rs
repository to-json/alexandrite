//! The C library in the browser: what `extern def`s in std (os, crypto/rand,
//! maphash, math/rand) get when a program runs in a page, in place of libc.
//!
//! The policy (GO-VS-RUBY D77, like Go's js/wasm without Node):
//!   - randomness (`getentropy`) comes from `crypto.getRandomValues`;
//!   - stdout and stderr (fds 1 and 2) are the run's output; stdin is empty;
//!   - there is no file system and no current directory: every other file
//!     operation fails with ENOSYS ("function not implemented");
//!   - the environment starts empty and lives in memory (setenv works);
//!   - `os.args` is the program's name alone; `exit` ends the run;
//!   - getpid/getuid/... are -1, the host name is "js", nothing is a terminal.
//! Errno and the `sys_const` table use Linux's numbers.
//!
//! An extern the browser has is one listed in `EXTERNS` with that exact
//! signature (wasm value types); anything else is refused at compile time
//! when reachable (`suspend::prepare`).

use crate::rt::{bytes, new_bytes, ret_str, sh, stop, Stop};
use std::cell::Cell;
use wasm_bindgen::prelude::*;

/// (C symbol, runtime function, signature: params `>` results, as in `wasmgen::RT`;
/// j = i64, i = i32: every integer is an i64 and a Bool an i32; a `Str` is
/// jj, a `[Byte]` jjj.)
pub const EXTERNS: &[(&str, &str, &str)] = &[
    ("getentropy", "alxr_getentropy", "jjjj>j"),
    ("read", "alxr_read", "jjjjj>j"),
    ("write", "alxr_write", "jjjjj>j"),
    ("pread", "alxr_nosys_fd_buf_off", "jjjjjj>j"),
    ("pwrite", "alxr_nosys_fd_buf_off", "jjjjjj>j"),
    ("close", "alxr_close", "j>j"),
    ("lseek", "alxr_lseek", "jjj>j"),
    ("fsync", "alxr_nosys_fd", "j>j"),
    ("ftruncate", "alxr_nosys_fd_int", "jj>j"),
    ("fchmod", "alxr_nosys_fd_int", "jj>j"),
    ("truncate", "alxr_nosys_path_int", "jjj>j"),
    ("chmod", "alxr_nosys_path_int", "jjj>j"),
    ("mkdir", "alxr_nosys_path_int", "jjj>j"),
    ("rmdir", "alxr_nosys_path", "jj>j"),
    ("unlink", "alxr_nosys_path", "jj>j"),
    ("chdir", "alxr_nosys_path", "jj>j"),
    ("rename", "alxr_nosys_path2", "jjjj>j"),
    ("link", "alxr_nosys_path2", "jjjj>j"),
    ("symlink", "alxr_nosys_path2", "jjjj>j"),
    ("readlink", "alxr_nosys_readlink", "jjjjjj>j"),
    ("getcwd", "alxr_getcwd", "jjjj>j"),
    ("gethostname", "alxr_gethostname", "jjjj>j"),
    ("getpid", "alxr_minus_one", ">j"),
    ("getppid", "alxr_minus_one", ">j"),
    ("getuid", "alxr_minus_one", ">j"),
    ("getgid", "alxr_minus_one", ">j"),
    ("getenv", "alxr_getenv", "jj>j"),
    ("setenv", "alxr_setenv", "jjjjj>j"),
    ("unsetenv", "alxr_unsetenv", "jj>j"),
    ("alx_environ", "alxr_environ", "j>j"),
    ("exit", "alxr_exit", "j>"),
    ("isatty", "alxr_isatty", "j>i"),
    ("alx_sys_const", "alxr_sys_const", "jj>j"),
    ("alx_sys_open", "alxr_sys_open", "jjjj>j"),
    ("alx_sys_stat", "alxr_sys_stat", "jjjjjj>j"),
    ("alx_sys_fstat", "alxr_sys_fstat", "jjjj>j"),
    ("alx_sys_dir_open", "alxr_sys_dir_open", "jj>j"),
    ("alx_sys_dir_next", "alxr_sys_dir_next", "jjjj>j"),
    ("alx_sys_dir_close", "alxr_sys_dir_close", "j>"),
    ("alx_argc", "alxr_argc", ">j"),
    ("alx_argv", "alxr_argv", "j>j"),
];

/// The runtime functions behind `EXTERNS` (each once), for the program's imports.
pub fn rt_funcs() -> Vec<(&'static str, &'static str)> {
    let mut v: Vec<(&str, &str)> = vec![];
    for (_, f, s) in EXTERNS {
        if !v.iter().any(|(g, _)| g == f) {
            v.push((f, s));
        }
    }
    v
}

const ENOENT: i64 = 2;
const EBADF: i64 = 9;
const EINVAL: i64 = 22;
const ESPIPE: i64 = 29;
const ENOSYS: i64 = 38;

thread_local! {
    /// errno: set by a failing call here, read by `C.errno` (`alxr_errno`).
    static ERRNO: Cell<i64> = const { Cell::new(0) };
}

fn fail(code: i64) -> i64 {
    ERRNO.with(|e| e.set(code));
    -1
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_errno() -> i64 {
    ERRNO.with(|e| e.get())
}

/// `Str.from_ptr(p, n)`: n bytes at p.
#[unsafe(no_mangle)]
pub extern "C" fn alxr_str_from_ptr(p: i64, n: i64) {
    ret_str(bytes(p, n))
}

fn buf<'a>(p: i64, n: i64) -> &'a mut [u8] {
    if n <= 0 { &mut [] } else { unsafe { std::slice::from_raw_parts_mut(p as usize as *mut u8, n as usize) } }
}

/// A NUL-terminated copy (a `Ptr` for `Str.from_cstr`).
fn cstr(s: &[u8]) -> i64 {
    let mut b = s.to_vec();
    b.push(0);
    new_bytes(&b)
}

// ---------- randomness ----------

#[wasm_bindgen(inline_js = "
export function random_bytes(n) {
  const b = new Uint8Array(n);
  for (let i = 0; i < n; i += 65536) crypto.getRandomValues(b.subarray(i, Math.min(n, i + 65536)));
  return b;
}")]
extern "C" {
    // A fresh array: getRandomValues refuses views of shared memory (the threaded build).
    fn random_bytes(n: u32) -> Vec<u8>;
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_getentropy(p: i64, len: i64, _cap: i64, n: i64) -> i64 {
    if n < 0 || n > len {
        return fail(EINVAL);
    }
    buf(p, n).copy_from_slice(&random_bytes(n as u32));
    0
}

// ---------- descriptors: 0 is empty, 1 and 2 are the output ----------

#[unsafe(no_mangle)]
pub extern "C" fn alxr_read(fd: i64, _p: i64, _len: i64, _cap: i64, _n: i64) -> i64 {
    if fd == 0 { 0 } else { fail(EBADF) }
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_write(fd: i64, p: i64, len: i64, _cap: i64, n: i64) -> i64 {
    let n = n.clamp(0, len.max(0));
    let text = String::from_utf8_lossy(bytes(p, n)).into_owned();
    match fd {
        1 => sh().out.push_str(&text),
        2 => sh().err.push_str(&text),
        _ => return fail(EBADF),
    }
    n
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_close(fd: i64) -> i64 {
    if (0..=2).contains(&fd) { 0 } else { fail(EBADF) }
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_lseek(fd: i64, _off: i64, _whence: i64) -> i64 {
    fail(if (0..=2).contains(&fd) { ESPIPE } else { EBADF })
}

fn nosys_fd(fd: i64) -> i64 {
    fail(if (0..=2).contains(&fd) { ENOSYS } else { EBADF })
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_nosys_fd(fd: i64) -> i64 {
    nosys_fd(fd)
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_nosys_fd_int(fd: i64, _a: i64) -> i64 {
    nosys_fd(fd)
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_nosys_fd_i64(fd: i64, _a: i64) -> i64 {
    nosys_fd(fd)
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_nosys_fd_buf_off(fd: i64, _p: i64, _len: i64, _cap: i64, _n: i64, _off: i64) -> i64 {
    nosys_fd(fd)
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_sys_fstat(fd: i64, _p: i64, _len: i64, _cap: i64) -> i64 {
    -(if (0..=2).contains(&fd) { ENOSYS } else { EBADF })
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_isatty(_fd: i64) -> i32 {
    0
}

// ---------- paths: no file system ----------

#[unsafe(no_mangle)]
pub extern "C" fn alxr_nosys_path(_p: i64, _n: i64) -> i64 {
    fail(ENOSYS)
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_nosys_path_int(_p: i64, _n: i64, _a: i64) -> i64 {
    fail(ENOSYS)
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_nosys_path_i64(_p: i64, _n: i64, _a: i64) -> i64 {
    fail(ENOSYS)
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_nosys_path2(_p: i64, _n: i64, _q: i64, _m: i64) -> i64 {
    fail(ENOSYS)
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_nosys_readlink(_p: i64, _n: i64, _b: i64, _len: i64, _cap: i64, _k: i64) -> i64 {
    fail(ENOSYS)
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_sys_open(_p: i64, _n: i64, _flags: i64, _mode: i64) -> i64 {
    fail(ENOSYS)
}

// These three return -errno (as the C runtime's do).
#[unsafe(no_mangle)]
pub extern "C" fn alxr_sys_stat(_p: i64, _n: i64, _b: i64, _len: i64, _cap: i64, _follow: i64) -> i64 {
    -ENOSYS
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_sys_dir_open(_p: i64, _n: i64) -> i64 {
    -ENOSYS
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_sys_dir_next(_h: i64, _b: i64, _len: i64, _cap: i64) -> i64 {
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_sys_dir_close(_h: i64) {}

/// No current directory (Go's js/wasm: Getwd fails with ENOSYS too).
#[unsafe(no_mangle)]
pub extern "C" fn alxr_getcwd(_p: i64, _len: i64, _cap: i64, _n: i64) -> i64 {
    fail(ENOSYS);
    0
}

/// "js", as Go's os.Hostname on js/wasm.
#[unsafe(no_mangle)]
pub extern "C" fn alxr_gethostname(p: i64, len: i64, _cap: i64, n: i64) -> i64 {
    let name = b"js\0";
    if n < name.len() as i64 || len < name.len() as i64 {
        return fail(EINVAL);
    }
    buf(p, name.len() as i64).copy_from_slice(name);
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_minus_one() -> i64 {
    -1
}

// ---------- environment (in memory, empty at the start of a run) ----------

#[unsafe(no_mangle)]
pub extern "C" fn alxr_getenv(p: i64, n: i64) -> i64 {
    let k = bytes(p, n);
    let v = sh().env.iter().find(|(x, _)| x.as_slice() == k).map(|(_, v)| v.clone());
    v.map_or(0, |v| cstr(&v))
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_setenv(kp: i64, kn: i64, vp: i64, vn: i64, overwrite: i64) -> i64 {
    let (k, v) = (bytes(kp, kn), bytes(vp, vn));
    if k.is_empty() || k.contains(&b'=') {
        return fail(EINVAL);
    }
    let mut s = sh();
    match s.env.iter_mut().find(|(x, _)| x.as_slice() == k) {
        Some(e) if overwrite != 0 => e.1 = v.to_vec(),
        Some(_) => {}
        None => s.env.push((k.to_vec(), v.to_vec())),
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_unsetenv(p: i64, n: i64) -> i64 {
    let k = bytes(p, n);
    if k.is_empty() || k.contains(&b'=') {
        return fail(EINVAL);
    }
    sh().env.retain(|(x, _)| x.as_slice() != k);
    0
}

/// The i-th "KEY=value", or NULL past the end.
#[unsafe(no_mangle)]
pub extern "C" fn alxr_environ(i: i64) -> i64 {
    let e = usize::try_from(i).ok().and_then(|i| sh().env.get(i).cloned());
    e.map_or(0, |(k, v)| cstr(&[k.as_slice(), b"=", v.as_slice()].concat()))
}

// ---------- the process ----------

#[unsafe(no_mangle)]
pub extern "C" fn alxr_argc() -> i64 {
    1
}

/// `os.args`: the program's name alone (the file name `compile` was given).
#[unsafe(no_mangle)]
pub extern "C" fn alxr_argv(i: i64) -> i64 {
    if i != 0 {
        return 0;
    }
    let name = sh().prog.clone();
    cstr(name.as_bytes())
}

/// Ends the run: status 0 ends it normally, anything else as a failure.
#[unsafe(no_mangle)]
pub extern "C" fn alxr_exit(code: i64) {
    stop(if code == 0 { Stop::Exit0 } else { Stop::Exit1 })
}

/// The constants `alx_sys_const` names (Linux's values).
#[unsafe(no_mangle)]
pub extern "C" fn alxr_sys_const(p: i64, n: i64) -> i64 {
    let name = std::str::from_utf8(bytes(p, n)).unwrap_or("");
    SYS_CONSTS.iter().find(|(k, _)| *k == name).map_or(-1, |(_, v)| *v)
}

const SYS_CONSTS: &[(&str, i64)] = &[
    ("O_RDONLY", 0), ("O_WRONLY", 1), ("O_RDWR", 2), ("O_CREAT", 0o100), ("O_EXCL", 0o200), ("O_TRUNC", 0o1000),
    ("O_APPEND", 0o2000), ("O_NONBLOCK", 0o4000), ("O_CLOEXEC", 0o2000000), ("O_DIRECTORY", 0o200000),
    ("O_SYNC", 0o4010000), ("O_NOFOLLOW", 0o400000),
    ("SEEK_SET", 0), ("SEEK_CUR", 1), ("SEEK_END", 2),
    ("F_GETFD", 1), ("F_SETFD", 2), ("F_GETFL", 3), ("F_SETFL", 4), ("FD_CLOEXEC", 1), ("F_DUPFD_CLOEXEC", 1030),
    ("EPERM", 1), ("ENOENT", ENOENT), ("ESRCH", 3), ("EINTR", 4), ("EIO", 5), ("ENXIO", 6), ("E2BIG", 7),
    ("ENOEXEC", 8), ("EBADF", EBADF), ("ECHILD", 10), ("EAGAIN", 11), ("EWOULDBLOCK", 11), ("ENOMEM", 12),
    ("EACCES", 13), ("EFAULT", 14), ("EBUSY", 16), ("EEXIST", 17), ("EXDEV", 18), ("ENODEV", 19),
    ("ENOTDIR", 20), ("EISDIR", 21), ("EINVAL", EINVAL), ("ENFILE", 23), ("EMFILE", 24), ("ENOTTY", 25),
    ("ETXTBSY", 26), ("EFBIG", 27), ("ENOSPC", 28), ("ESPIPE", ESPIPE), ("EROFS", 30), ("EMLINK", 31),
    ("EPIPE", 32), ("EDOM", 33), ("ERANGE", 34), ("ENAMETOOLONG", 36), ("ENOSYS", ENOSYS), ("ENOTEMPTY", 39),
    ("ELOOP", 40), ("EOVERFLOW", 75), ("ENOTSUP", 95), ("EAFNOSUPPORT", 97), ("EADDRINUSE", 98),
    ("EADDRNOTAVAIL", 99), ("ENETUNREACH", 101), ("ECONNABORTED", 103), ("ECONNRESET", 104), ("ENOBUFS", 105),
    ("ENOTCONN", 107), ("ETIMEDOUT", 110), ("ECONNREFUSED", 111), ("EHOSTUNREACH", 113), ("EINPROGRESS", 115),
    ("ESTALE", 116), ("EDQUOT", 122),
    ("S_IFMT", 0o170000), ("S_IFREG", 0o100000), ("S_IFDIR", 0o40000), ("S_IFLNK", 0o120000), ("S_IFIFO", 0o10000),
    ("S_IFCHR", 0o20000), ("S_IFBLK", 0o60000), ("S_IFSOCK", 0o140000),
    ("S_IRWXU", 0o700), ("S_IRUSR", 0o400), ("S_IWUSR", 0o200), ("S_IXUSR", 0o100), ("S_IRWXG", 0o70), ("S_IRWXO", 0o7),
    ("S_ISUID", 0o4000), ("S_ISGID", 0o2000), ("S_ISVTX", 0o1000), ("S_IRGRP", 0o40), ("S_IWGRP", 0o20),
    ("S_IXGRP", 0o10), ("S_IROTH", 0o4), ("S_IWOTH", 0o2), ("S_IXOTH", 0o1),
    ("CLOCK_REALTIME", 0), ("CLOCK_MONOTONIC", 1),
    ("SIGHUP", 1), ("SIGINT", 2), ("SIGQUIT", 3), ("SIGILL", 4), ("SIGTRAP", 5), ("SIGABRT", 6), ("SIGBUS", 7),
    ("SIGFPE", 8), ("SIGKILL", 9), ("SIGUSR1", 10), ("SIGSEGV", 11), ("SIGUSR2", 12), ("SIGPIPE", 13),
    ("SIGALRM", 14), ("SIGTERM", 15), ("SIGCHLD", 17), ("SIGCONT", 18), ("SIGSTOP", 19), ("SIGTSTP", 20),
    ("SIGTTIN", 21), ("SIGTTOU", 22), ("SIGURG", 23), ("SIGXCPU", 24), ("SIGXFSZ", 25), ("SIGVTALRM", 26),
    ("SIGPROF", 27), ("SIGWINCH", 28), ("SIGIO", 29), ("SIGSYS", 31),
];
