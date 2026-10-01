//! `alx run`: front end → C → clang → run. Everything built lands in
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
        let mut f = vec!["-std=gnu11", "-fwrapv", "-w"];
        if self.sanitize {
            f.extend(["-O1", "-g", "-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-fno-omit-frame-pointer"]);
        } else if self.release {
            f.push("-O2");
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

fn frontend(file: &str) -> Result<(front::Loaded, crate::tast::TProgram), String> {
    let l = front::load(Path::new(file), file).map_err(|(sm, d)| sm.render(&d))?;
    let externs = front::lib_defs(&l);
    let p = front::check_program(&l, externs).map_err(|d| l.sm.render(&d))?;
    Ok((l, p))
}

/// Compile each required library to a cached object; return extern
/// definitions (from the generated headers) and the objects to link.
fn libraries(l: &front::Loaded, o: &Options, cache: &Path, inc: &Path) -> Result<(Vec<crate::check::DefInfo>, Vec<PathBuf>), String> {
    let mut externs = vec![];
    let mut objs = vec![];
    let ver = concat!(env!("CARGO_PKG_VERSION"), "-", env!("CARGO_PKG_NAME"));
    let me = std::env::current_exe().ok().and_then(|p| std::fs::metadata(p).ok()).and_then(|m| m.modified().ok()).map(|t| format!("{t:?}")).unwrap_or_default();
    for (i, (req, path, m)) in l.libs.iter().enumerate() {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let flags = o.cflags().join(" ");
        let h = hash_of(&[&text, RT_H, ver, &me, &format!("{:?}", m.overflow), &flags]);
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
            std::fs::write(&hdr, front::header(&tp, &exports, m.overflow)).map_err(|e| e.to_string())?;
        }
        let text = std::fs::read_to_string(&hdr).map_err(|e| e.to_string())?;
        let span = l.main.requires[i].1;
        externs.extend(front::parse_header(&text, l.main.overflow, span)?);
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
    let lp = lower::lower(&p, &l.sm, &lower::Opts { release: o.release });
    let c = cgen::emit(&lp);
    if let Some(path) = &o.emit_c {
        let _ = std::fs::write(path, &c);
    }
    if let Some(path) = &o.emit_rust {
        // The oracle sees the whole program from source, libraries included.
        match front::check_program(&l, front::lib_defs(&l)) {
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
    cmd.args(o.cflags()).arg("-I").arg(&inc).arg(&prog_c).args(&objs).arg("-o").arg(&bin).arg("-lpthread");
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

pub fn run(file: &str, o: &Options) -> ExitCode {
    let bin = match build(file, o) {
        Ok(b) => b,
        Err(c) => return c,
    };
    let cwd_bin = if bin.is_relative() { Path::new(".").join(&bin) } else { bin.clone() };
    match &o.expect {
        None => {
            use std::os::unix::process::CommandExt;
            let err = Command::new(&cwd_bin).exec();
            eprintln!("alx: cannot run {}: {err}", bin.display());
            ExitCode::from(3)
        }
        Some(want) => {
            let out = match Command::new(&cwd_bin).stderr(Stdio::inherit()).output() {
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
