//! `alx run`: front end → Cranelift JIT, in process (debug), or front end →
//! C → clang → run (`--release`, `--sanitize`, `alx build`). Everything built lands in
//! `.alx-cache/` next to the source file; nothing else is created.

use crate::{cgen, front, lower};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

const RT_H: &str = include_str!("../runtime/alx.h");
const RT_C: &str = include_str!("../runtime/alx.c");
const RT_BIG: &str = include_str!("../runtime/alx_big.c");
const TOMMATH: &str = include_str!("../runtime/libtommath/tommath_amalgam.c");

#[derive(Default)]
pub struct Options {
    pub release: bool,
    pub sanitize: bool,
    pub verbose: bool,
    pub expect: Option<String>,
    pub emit_c: Option<String>,
    pub emit_rust: Option<String>,
    pub out: Option<String>,
    /// Warnings are errors.
    pub strict: bool,
    /// The program's own arguments (`alx run file.alx a b`).
    pub args: Vec<String>,
}

/// Print a program's warnings; under `--strict` they fail the build.
fn report(sm: &alx::diag::SourceMap, p: &crate::tast::TProgram, o: &Options) -> bool {
    for w in &p.warnings {
        if o.strict {
            eprint!("{}", sm.render(w));
        } else {
            eprint!("{}", sm.render_warning(w));
        }
    }
    !(o.strict && !p.warnings.is_empty())
}

impl Options {
    fn mode(&self) -> &'static str {
        if self.sanitize {
            "san"
        } else if self.release {
            "rel"
        } else {
            "dbg"
        }
    }
    fn cflags(&self) -> Vec<&'static str> {
        let mut f = vec!["-std=gnu11", "-fwrapv", "-w", "-ffp-contract=off"];
        if self.sanitize {
            f.extend(["-O1", "-g", "-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-fno-omit-frame-pointer"]);
        } else if self.release {
            // Sections let the linker drop unused runtime code: a smaller
            // binary starts faster (about 0.2 ms on macOS).
            f.extend(["-O2", "-ffunction-sections", "-fdata-sections"]);
        } else {
            f.push("-O0");
        }
        f
    }
}

fn hash_of(parts: &[&str]) -> String {
    let mut h = DefaultHasher::new();
    for p in parts {
        p.hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

fn log(o: &Options, msg: &str) {
    if o.verbose {
        eprintln!("alx: {msg}");
    }
}

fn cache_dir(src: &Path) -> PathBuf {
    src.parent().unwrap_or(Path::new(".")).join(".alx-cache")
}

/// Compile a C file to an object if the cached one is missing.
fn cached_object(o: &Options, cache: &Path, name: &str, source: &str, include_dir: &Path, extra: &str) -> Result<PathBuf, String> {
    let mut cflags = o.cflags();
    // libtommath relies on dead-code elimination of unused platform paths.
    if name == "big" && !o.sanitize {
        cflags.retain(|f| !f.starts_with("-O"));
        cflags.push("-O2");
    }
    let flags = cflags.join(" ");
    let h = hash_of(&[source, RT_H, &flags, extra]);
    let obj = cache.join(format!("{name}-{}-{h}.o", o.mode()));
    if obj.exists() {
        log(o, &format!("{name}: cached"));
        return Ok(obj);
    }
    log(o, &format!("{name}: compiling"));
    let c = cache.join(format!("{name}-{h}.c"));
    std::fs::write(&c, source).map_err(|e| e.to_string())?;
    let st = Command::new("clang").args(&cflags).arg("-I").arg(include_dir).arg("-c").arg(&c).arg("-o").arg(&obj).status().map_err(|e| format!("cannot run clang: {e}"))?;
    if !st.success() {
        return Err(format!("clang failed on {}", c.display()));
    }
    Ok(obj)
}

pub fn check_only(file: &str) -> ExitCode {
    match frontend(file) {
        Ok(_) => ExitCode::SUCCESS,
        Err(msg) => {
            eprint!("{msg}");
            ExitCode::from(1)
        }
    }
}

/// `alx explain mem`: where each allocation lives, and why (R7).
pub fn explain_mem(file: &str) -> ExitCode {
    match frontend(file) {
        Ok((l, p)) => {
            let files: Vec<u32> = (0..l.sm.files.len() as u32).collect();
            print!("{}", alx::regions::explain(&p, &l.sm, &files));
            ExitCode::SUCCESS
        }
        Err(msg) => {
            eprint!("{msg}");
            ExitCode::from(1)
        }
    }
}

fn frontend(file: &str) -> Result<(front::Loaded, crate::tast::TProgram), String> {
    let l = front::load(Path::new(file), file).map_err(|(sm, d)| sm.render(&d))?;
    let externs = front::lib_defs(&l);
    let p = front::check_program(&l, externs).map_err(|d| l.sm.render(&d))?;
    Ok((l, p))
}

/// Compile each separable imported package to a cached object; return
/// extern definitions (from the generated headers) and the objects to link.
fn libraries(l: &front::Loaded, o: &Options, cache: &Path, inc: &Path) -> Result<(Vec<crate::check::DefInfo>, Vec<PathBuf>), String> {
    let mut externs = vec![];
    let mut objs = vec![];
    let ver = concat!(env!("CARGO_PKG_VERSION"), "-", env!("CARGO_PKG_NAME"));
    let me = std::env::current_exe().ok().and_then(|p| std::fs::metadata(p).ok()).and_then(|m| m.modified().ok()).map(|t| format!("{t:?}")).unwrap_or_default();
    for (i, pkg) in l.pkgs.iter().enumerate() {
        if !front::separable(pkg) {
            continue;
        }
        let req = &pkg.path;
        let flags = o.cflags().join(" ");
        let h = hash_of(&[&pkg.source, RT_H, ver, &me, &format!("{:?}", pkg.module.overflow), &flags]);
        let name = req.replace(['/', '.'], "_");
        let obj = cache.join(format!("lib-{name}-{}-{h}.o", o.mode()));
        let hdr = cache.join(format!("lib-{name}-{h}.alxh"));
        if obj.exists() && hdr.exists() {
            log(o, &format!("{req}: cached"));
        } else {
            log(o, &format!("{req}: compiling"));
            let prefix = format!("alx_lib_{}_{name}", &h[..8]);
            let (tp, exports) = front::check_library(l, i, &prefix).map_err(|d| l.sm.render(&d))?;
            let lp = lower::lower(&tp, &l.sm, &lower::Opts { release: o.release });
            let c = cgen::emit(&lp);
            let c_path = cache.join(format!("lib-{name}-{}-{h}.c", o.mode()));
            std::fs::write(&c_path, c).map_err(|e| e.to_string())?;
            let st = Command::new("clang").args(o.cflags()).arg("-I").arg(inc).arg("-c").arg(&c_path).arg("-o").arg(&obj).status().map_err(|e| e.to_string())?;
            if !st.success() {
                return Err(format!("clang failed on {}", c_path.display()));
            }
            std::fs::write(&hdr, front::header(&tp, &exports, pkg.module.overflow)).map_err(|e| e.to_string())?;
        }
        let text = std::fs::read_to_string(&hdr).map_err(|e| e.to_string())?;
        let span = l.main.imports.iter().find(|m| m.path.trim_end_matches('/') == pkg.path).map_or_else(Default::default, |m| m.span);
        externs.extend(front::parse_header(&text, l.main.overflow, span, &pkg.path)?);
        objs.push(obj);
    }
    Ok((externs, objs))
}

/// Build the program; returns the binary path.
fn build(file: &str, o: &Options) -> Result<PathBuf, ExitCode> {
    let l = match front::load(Path::new(file), file) {
        Ok(l) => l,
        Err((sm, d)) => {
            eprint!("{}", sm.render(&d));
            return Err(ExitCode::from(1));
        }
    };
    build_loaded(l, file, o)
}

fn build_loaded(l: front::Loaded, file: &str, o: &Options) -> Result<PathBuf, ExitCode> {
    let cache = cache_dir(Path::new(file));
    let fail = |m: String| {
        eprintln!("alx: {m}");
        ExitCode::from(3)
    };
    std::fs::create_dir_all(&cache).map_err(|e| fail(e.to_string()))?;
    let inc = cache.join("include");
    std::fs::create_dir_all(&inc).map_err(|e| fail(e.to_string()))?;
    let h = inc.join("alx.h");
    if std::fs::read_to_string(&h).ok().as_deref() != Some(RT_H) {
        std::fs::write(&h, RT_H).map_err(|e| fail(e.to_string()))?;
    }
    let (externs, lib_objs) = match libraries(&l, o, &cache, &inc) {
        Ok(x) => x,
        Err(msg) => {
            eprint!("{msg}");
            if !msg.ends_with('\n') {
                eprintln!();
            }
            return Err(ExitCode::from(1));
        }
    };
    let p = match front::check_program(&l, externs) {
        Ok(p) => p,
        Err(d) => {
            eprint!("{}", l.sm.render(&d));
            return Err(ExitCode::from(1));
        }
    };
    if !report(&l.sm, &p, o) {
        return Err(ExitCode::from(1));
    }
    let lp = lower::lower(&p, &l.sm, &lower::Opts { release: o.release });
    let c = cgen::emit(&lp);
    if let Some(path) = &o.emit_c {
        let _ = std::fs::write(path, &c);
    }
    if let Some(path) = &o.emit_rust {
        // The oracle sees the whole program from source, packages included.
        match front::check_program(&l, vec![]) {
            Ok(whole) => {
                let lw = lower::lower(&whole, &l.sm, &lower::Opts { release: o.release });
                let _ = std::fs::write(path, crate::rgen::emit(&lw));
            }
            Err(d) => eprint!("{}", l.sm.render(&d)),
        }
    }
    let rt = cached_object(o, &cache, "rt", RT_C, &inc, "").map_err(fail)?;
    let mut objs = vec![rt];
    objs.extend(lib_objs);
    if lp.uses_pint {
        let t = inc.join("tommath_amalgam.c");
        if std::fs::metadata(&t).map(|m| m.len() as usize) .ok() != Some(TOMMATH.len()) {
            std::fs::write(&t, TOMMATH).map_err(|e| fail(e.to_string()))?;
        }
        // libtommath is always optimized: debug builds shouldn't pay for -O0 bignums.
        let big = cached_object(o, &cache, "big", RT_BIG, &inc, TOMMATH).map_err(fail)?;
        objs.push(big);
    }
    let stem = Path::new(file).file_stem().and_then(|s| s.to_str()).unwrap_or("prog");
    let prog_c = cache.join(format!("{stem}-{}.c", o.mode()));
    std::fs::write(&prog_c, &c).map_err(|e| fail(e.to_string()))?;
    let bin = match &o.out {
        Some(p) => PathBuf::from(p),
        None => cache.join(format!("{stem}-{}", o.mode())),
    };
    let mut cmd = Command::new("clang");
    cmd.args(o.cflags()).arg("-I").arg(&inc).arg(&prog_c).args(&objs).arg("-o").arg(&bin).arg("-lpthread").arg("-lm");
    if o.release && !o.sanitize {
        cmd.arg(if cfg!(target_os = "macos") { "-Wl,-dead_strip" } else { "-Wl,--gc-sections" });
    }
    let uses_raylib = lp.externs.iter().any(|x| {
        matches!(x.sym.as_str(), "InitWindow" | "CloseWindow" | "BeginDrawing" | "EndDrawing" | "ClearBackground" | "WindowShouldClose")
    });
    if uses_raylib || std::env::var_os("ALX_RAYLIB").is_some() {
        let rl_path = Path::new("raylib/src");
        let ws_path = Path::new("/home/j/alexandrite/raylib/src");
        if rl_path.exists() {
            cmd.arg(format!("-L{}", rl_path.display()));
            if let Ok(canon) = rl_path.canonicalize() {
                cmd.arg(format!("-Wl,-rpath,{}", canon.display()));
            } else {
                cmd.arg(format!("-Wl,-rpath,{}", rl_path.display()));
            }
        } else if ws_path.exists() {
            cmd.arg(format!("-L{}", ws_path.display()));
            cmd.arg(format!("-Wl,-rpath,{}", ws_path.display()));
        }
        cmd.arg("-lraylib");
        if !cfg!(target_os = "macos") {
            cmd.arg("-lGL").arg("-lX11");
        }
    }
    // `#[link("sqlite3")] extern def ...`: each library once, after the objects.
    let mut libs: Vec<&str> = vec![];
    for x in &lp.externs {
        if let Some(l) = x.lib.as_deref().filter(|l| !libs.contains(l)) {
            libs.push(l);
        }
    }
    for l in libs {
        cmd.arg(format!("-l{l}"));
    }
    if let Ok(ldflags) = std::env::var("ALX_LDFLAGS") {
        for arg in ldflags.split_whitespace() {
            cmd.arg(arg);
        }
    }
    log(o, &format!("{stem}: compiling"));
    let st = cmd.status().map_err(|e| fail(format!("cannot run clang: {e}")))?;
    if !st.success() {
        return Err(fail(format!("clang failed on {}", prog_c.display())));
    }
    Ok(bin)
}

pub fn build_only(file: &str, o: &Options) -> ExitCode {
    match build(file, o) {
        Ok(bin) => {
            println!("{}", bin.display());
            ExitCode::SUCCESS
        }
        Err(c) => c,
    }
}

/// `100ms`, `2s`, `1.5s`, `250us`, `1000ns`, or bare seconds.
pub fn parse_duration_ns(s: &str) -> Option<i64> {
    let s = s.trim();
    let (num, mult) = if let Some(n) = s.strip_suffix("ms") {
        (n, 1e6)
    } else if let Some(n) = s.strip_suffix("us") {
        (n, 1e3)
    } else if let Some(n) = s.strip_suffix("ns") {
        (n, 1.0)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1e9)
    } else {
        (s, 1e9)
    };
    let v: f64 = num.parse().ok()?;
    (v > 0.0).then_some((v * mult) as i64)
}

/// `alx test [path]`: the tests of a directory (or one `_test.alx` file),
/// compiled together with the package beside them and run by a generated
/// runner. JIT by default, the C backend with `--release`.
pub fn test(target: &str, t: &front::TestOpts, o: &Options) -> ExitCode {
    let path = Path::new(target);
    let (dir, only) = if path.is_dir() { (path.to_path_buf(), None) } else { (path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")).to_path_buf(), Some(path)) };
    let shown = if only.is_some() { path.parent().map(|p| p.to_string_lossy().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| ".".into()) } else { target.trim_end_matches('/').to_string() };
    let shown = if shown.is_empty() { "/".to_string() } else { shown };
    if only.is_some() && !target.ends_with("_test.alx") {
        eprintln!("alx: `{target}` is not a `*_test.alx` file or a directory");
        return ExitCode::from(2);
    }
    let (l, all) = match front::load_tests(&dir, &shown, only, t) {
        Ok((l, n, total)) => (l, n == total),
        Err((sm, d)) => {
            eprint!("{}", sm.render(&d));
            return ExitCode::from(1);
        }
    };
    // Warnings are errors in tests (P3).
    // Bodies that aren't run aren't checked, so only a full run can judge unused imports.
    let strict = Options { strict: all, release: o.release, sanitize: o.sanitize, verbose: o.verbose, ..Default::default() };
    // The package testing reads the flags from the environment (alx has
    // no mutable package state for Go's flag variables).
    let mut env = vec![("ALX_TESTING", "1".to_string()), ("ALX_TEST_BENCHTIME_NS", t.bench_ns.to_string())];
    if t.short {
        env.push(("ALX_TEST_SHORT", "1".into()));
    }
    if o.verbose {
        env.push(("ALX_TEST_VERBOSE", "1".into()));
    }
    if let Some(r) = &t.run {
        env.push(("ALX_TEST_RUN", r.clone()));
    }
    if o.release || o.sanitize {
        let file = dir.join("alx_test.alx").to_string_lossy().to_string();
        let bin = match build_loaded(l, &file, &strict) {
            Ok(b) => b,
            Err(c) => return c,
        };
        let bin = std::fs::canonicalize(&bin).unwrap_or(bin);
        // Tests run in the package's directory, as Go's do (testdata/ paths).
        return match Command::new(&bin).current_dir(&dir).envs(env.iter().map(|(k, v)| (k, v))).status() {
            Ok(s) => ExitCode::from(s.code().unwrap_or(1) as u8),
            Err(e) => {
                eprintln!("alx: cannot run {}: {e}", bin.display());
                ExitCode::from(3)
            }
        };
    }
    // Tests run in the package's directory, as Go's do (testdata/ paths).
    if let Err(e) = std::env::set_current_dir(&dir) {
        eprintln!("alx: cannot enter {}: {e}", dir.display());
        return ExitCode::from(3);
    }
    for (k, v) in &env {
        // SAFETY: the compiler is single-threaded here; the JIT program
        // (whose tasks read the environment) hasn't started yet.
        unsafe { std::env::set_var(k, v) };
    }
    run_loaded(l, &strict)
}

/// `alx run` without --release/--sanitize: whole program from source,
/// compiled in memory by the Cranelift JIT, run in this process.
fn run_jit(file: &str, o: &Options) -> ExitCode {
    let l = match front::load(Path::new(file), file) {
        Ok(l) => l,
        Err((sm, d)) => {
            eprint!("{}", sm.render(&d));
            return ExitCode::from(1);
        }
    };
    run_loaded(l, o)
}

fn run_loaded(l: front::Loaded, o: &Options) -> ExitCode {
    let p = match front::check_program(&l, front::lib_defs(&l)) {
        Ok(p) => p,
        Err(d) => {
            eprint!("{}", l.sm.render(&d));
            return ExitCode::from(1);
        }
    };
    if !report(&l.sm, &p, o) {
        return ExitCode::from(1);
    }
    // Release lowering: it only drops checks the prover has shown can't fail
    // (debug C builds keep them, which tests the prover).
    let lp = lower::lower(&p, &l.sm, &lower::Opts { release: true });
    log(o, "jit");
    let Some(want) = &o.expect else {
        let argv: Vec<String> = std::iter::once(l.sm.files[0].name.clone()).chain(o.args.iter().cloned()).collect();
        crate::jit::set_args(&argv);
        return match crate::jit::run(&lp) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("alx: {e}");
                ExitCode::from(3)
            }
        };
    };
    // --expect: run in a child whose stdout we capture.
    let mut fds = [0; 2];
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            eprintln!("alx: pipe failed");
            return ExitCode::from(3);
        }
        let pid = libc::fork();
        if pid == 0 {
            libc::close(fds[0]);
            libc::dup2(fds[1], 1);
            libc::close(fds[1]);
            let code = match crate::jit::run(&lp) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("alx: {e}");
                    3
                }
            };
            libc::_exit(code);
        }
        libc::close(fds[1]);
        use std::io::Read;
        use std::os::fd::FromRawFd;
        let mut got = String::new();
        let _ = std::fs::File::from_raw_fd(fds[0]).read_to_string(&mut got);
        let mut status = 0;
        libc::waitpid(pid, &mut status, 0);
        print!("{got}");
        let ok = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
        if got.trim() == want.trim() && ok {
            ExitCode::SUCCESS
        } else {
            eprintln!("alx: expected `{}`, got `{}`", want.trim(), got.trim());
            ExitCode::from(1)
        }
    }
}

pub fn run(file: &str, o: &Options) -> ExitCode {
    if !o.release && !o.sanitize && o.emit_c.is_none() && o.emit_rust.is_none() && std::env::var_os("ALX_NO_JIT").is_none() {
        return run_jit(file, o);
    }
    let bin = match build(file, o) {
        Ok(b) => b,
        Err(c) => return c,
    };
    let cwd_bin = if bin.is_relative() { Path::new(".").join(&bin) } else { bin.clone() };
    match &o.expect {
        None => {
            use std::os::unix::process::CommandExt;
            let err = Command::new(&cwd_bin).args(&o.args).exec();
            eprintln!("alx: cannot run {}: {err}", bin.display());
            ExitCode::from(3)
        }
        Some(want) => {
            let out = match Command::new(&cwd_bin).args(&o.args).stderr(Stdio::inherit()).output() {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("alx: cannot run {}: {e}", bin.display());
                    return ExitCode::from(3);
                }
            };
            let got = String::from_utf8_lossy(&out.stdout);
            print!("{got}");
            if got.trim() == want.trim() && out.status.success() {
                ExitCode::SUCCESS
            } else {
                eprintln!("alx: expected `{}`, got `{}`", want.trim(), got.trim());
                ExitCode::from(1)
            }
        }
    }
}
