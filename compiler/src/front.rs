//! Front end: load, parse (with imported packages), check, prove.

use crate::ast::{import_name, Import, Module, NodeId, Overflow, TestDecl, TestKind};
use std::collections::{HashMap, HashSet};
use crate::check::{DefInfo, World};
use crate::diag::{Diag, SourceMap, Span};
use crate::analyze::{Item, Phase};
use crate::tast::TProgram;
use crate::{lexer, parser, prove};
use std::path::{Path, PathBuf};

pub struct Loaded {
    pub sm: SourceMap,
    pub main: Module,
    /// Imported packages (in dependency order), declarations qualified.
    pub pkgs: Vec<Package>,
    /// Each file's overflow mode (directives are per file).
    pub overflow: HashMap<u32, Overflow>,
}

pub struct Package {
    /// The import path, also the prefix of every name it declares.
    pub path: String,
    pub module: Module,
    /// All its source text (for build caching).
    pub source: String,
}

pub fn parse_file(sm: &mut SourceMap, display: String, text: String, next_id: &mut NodeId) -> Result<Module, Diag> {
    parse_file_at(sm, display, None, text, next_id)
}

/// `parse_file`, recording the file's real path (see `SourceFile::path`).
pub fn parse_file_at(sm: &mut SourceMap, display: String, path: Option<PathBuf>, text: String, next_id: &mut NodeId) -> Result<Module, Diag> {
    let f = sm.add_at(display, path, text.clone());
    let toks = lexer::lex(f, &text)?;
    parser::parse(f, &toks, next_id)
}

/// Parse a file keeping going after syntax errors: the (partial) module and one
/// error per broken top-level declaration, at most `parser::MAX_ERRORS`. A lexer
/// error ends the file with that one error (the module is empty). The first error is
/// the one `parse_file_at` returns; the module is complete only when the list is empty.
pub fn parse_file_recovering(sm: &mut SourceMap, display: String, path: Option<PathBuf>, text: String, next_id: &mut NodeId) -> (Option<Module>, Vec<Diag>) {
    let f = sm.add_at(display, path, text.clone());
    match lexer::lex(f, &text) {
        Err(d) => (None, vec![d]),
        Ok(toks) => {
            let (m, errs) = parser::parse_recovering(f, &toks, next_id);
            (Some(m), errs)
        }
    }
}

/// The builtin declarations (`Complex`), parsed with every program.
const BUILTINS: &str = include_str!("builtin.alx");

/// Merge the builtin declarations into the main module.
fn add_builtins(sm: &mut SourceMap, next_id: &mut NodeId, main: &mut Module, overflow: &mut HashMap<u32, Overflow>) -> Result<(), Diag> {
    let b = parse_file(sm, "<builtin>".into(), BUILTINS.to_string(), next_id)?;
    overflow.insert(b.file, b.overflow);
    merge_decls(main, b);
    Ok(())
}

pub fn load(path: &Path, display: &str) -> Result<Loaded, (SourceMap, Diag)> {
    let list = |p: &Path| -> std::io::Result<Vec<PathBuf>> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(p)?.filter_map(|e| e.ok().map(|e| e.path())).collect();
        v.sort();
        Ok(v)
    };
    load_with(path, display, &|p| std::fs::read_to_string(p), &list)
}

type ReadFn<'a> = &'a dyn Fn(&Path) -> std::io::Result<String>;

/// A parsed `alx.mod`.
#[derive(Default, Debug)]
struct ModFile {
    module: Option<String>,
    requires: Vec<(String, String)>,
    /// (module, optional version, local directory)
    replaces: Vec<(String, Option<String>, String)>,
}

/// Parse `alx.mod`: `module P`, `require P vX.Y.Z`, `replace P [vX.Y.Z] => DIR`,
/// one per line or in `require ( ... )` / `replace ( ... )` blocks; `#` comments.
fn parse_mod(text: &str, file: &Path) -> Result<ModFile, Diag> {
    let mut m = ModFile::default();
    let mut block: Option<String> = None;
    for (n, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let bad = |why: &str| Diag::new(Span::default(), format!("{}:{}: {why}", file.display(), n + 1));
        let toks: Vec<String> = line.split_whitespace().map(|s| s.trim_matches('"').to_string()).collect();
        let (dir, args): (String, &[String]) = match &block {
            Some(b) => {
                if toks[0] == ")" {
                    block = None;
                    continue;
                }
                (b.clone(), &toks[..])
            }
            None => {
                if toks.len() == 2 && toks[1] == "(" && (toks[0] == "require" || toks[0] == "replace") {
                    block = Some(toks[0].clone());
                    continue;
                }
                (toks[0].clone(), &toks[1..])
            }
        };
        match dir.as_str() {
            "module" if block.is_none() && args.len() == 1 => m.module = Some(args[0].clone()),
            "require" if args.len() == 2 => m.requires.push((args[0].clone(), args[1].clone())),
            "replace" => match args {
                [p, a, t] if a == "=>" => m.replaces.push((p.clone(), None, t.clone())),
                [p, v, a, t] if a == "=>" => m.replaces.push((p.clone(), Some(v.clone()), t.clone())),
                _ => return Err(bad("malformed `replace` (want `replace PATH [VERSION] => DIR`)")),
            },
            "module" => return Err(bad("malformed `module` (want `module PATH`)")),
            "require" => return Err(bad("malformed `require` (want `require PATH VERSION`)")),
            other => return Err(bad(&format!("unknown directive `{other}`"))),
        }
    }
    Ok(m)
}

/// `vMAJOR.MINOR.PATCH[-pre][+build]` as a sortable key; a pre-release sorts
/// below its release.
fn semver_key(v: &str) -> (u64, u64, u64, bool, String) {
    let v = v.strip_prefix('v').unwrap_or(v);
    let v = v.split('+').next().unwrap_or(v);
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) => (c, Some(p.to_string())),
        None => (v, None),
    };
    let mut it = core.split('.').map(|x| x.parse::<u64>().unwrap_or(0));
    let (a, b, c) = (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0));
    (a, b, c, pre.is_none(), pre.unwrap_or_default())
}

/// The main module and its selected dependencies.
struct Mods {
    root: PathBuf,
    module: Option<String>,
    /// (module path, selected version, directory, replaced locally)
    deps: Vec<(String, String, PathBuf, bool)>,
}

fn is_inside(path: &str, m: &str) -> bool {
    path == m || path.strip_prefix(m).is_some_and(|r| r.starts_with('/'))
}

fn mod_cache() -> Option<PathBuf> {
    if let Ok(c) = std::env::var("ALX_MODCACHE") {
        return Some(PathBuf::from(c));
    }
    std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".alx").join("mod"))
}

/// Where module `path` at `version` lives: its `replace` target, else the
/// module cache.
fn mod_dir(path: &str, version: &str, root: &Path, replaces: &[(String, Option<String>, String)]) -> (PathBuf, bool) {
    let r = replaces.iter().find(|(p, v, _)| p == path && v.as_deref() == Some(version)).or_else(|| replaces.iter().find(|(p, v, _)| p == path && v.is_none()));
    match r {
        Some((_, _, t)) => (root.join(t), true),
        None => (mod_cache().unwrap_or_else(|| PathBuf::from("<module cache>")).join(format!("{path}@{version}")), false),
    }
}

/// The module root (the nearest directory up with an `alx.mod`), the module
/// path it declares, and the dependencies chosen by minimal version
/// selection: each module at the maximum version required anywhere in the
/// requirement graph. Only the main module's `replace`s apply.
fn load_mods(dir: &Path, read: ReadFn) -> Result<Mods, Diag> {
    let mut d = Some(dir);
    let mut found = None;
    while let Some(x) = d {
        let f = x.join("alx.mod");
        if let Ok(text) = read(&f) {
            found = Some((x.to_path_buf(), parse_mod(&text, &f)?));
            break;
        }
        d = x.parent();
    }
    let Some((root, main)) = found else {
        return Ok(Mods { root: dir.to_path_buf(), module: None, deps: vec![] });
    };
    let mut selected: Vec<(String, String)> = vec![];
    let mut visited: HashSet<(String, String)> = HashSet::new();
    let mut work: Vec<(String, String)> = main.requires.clone();
    while let Some((p, v)) = work.pop() {
        match selected.iter_mut().find(|(sp, _)| *sp == p) {
            Some(s) => {
                if semver_key(&v) > semver_key(&s.1) {
                    s.1 = v.clone();
                }
            }
            None => selected.push((p.clone(), v.clone())),
        }
        if !visited.insert((p.clone(), v.clone())) {
            continue;
        }
        let (md, _) = mod_dir(&p, &v, &root, &main.replaces);
        let f = md.join("alx.mod");
        if let Ok(text) = read(&f) {
            work.extend(parse_mod(&text, &f)?.requires);
        }
    }
    let deps = selected
        .into_iter()
        .map(|(p, v)| {
            let (md, rep) = mod_dir(&p, &v, &root, &main.replaces);
            (p, v, md, rep)
        })
        .collect();
    Ok(Mods { root, module: main.module, deps })
}

/// The standard library's packages, embedded in the compiler.
mod stdlib {
    include!(concat!(env!("OUT_DIR"), "/std_files.rs"));
}

/// The embedded standard library's files.
fn std_files() -> &'static [(String, String)] {
    static FILES: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();
    FILES.get_or_init(|| stdlib::STD.iter().map(|(p, t)| (p.to_string(), t.to_string())).collect())
}

/// Where the embedded standard library appears as a directory.
pub const STD_DIR: &str = "$std";

/// `ALX_STD_DIR`: std is read from this directory, on demand, through the
/// loader's `read`/`list` (so unsaved editor buffers win), instead of the embedded copy.
fn std_root() -> Option<PathBuf> {
    std::env::var_os("ALX_STD_DIR").map(PathBuf::from)
}

/// The file on disk behind a `$std/rel` path (only with `ALX_STD_DIR`).
fn std_real(f: &Path) -> Option<PathBuf> {
    Some(std_root()?.join(f.strip_prefix(STD_DIR).ok()?))
}

/// The files of a std package directory (`$std/...` paths).
fn std_list(dir: &Path, list: &dyn Fn(&Path) -> std::io::Result<Vec<PathBuf>>) -> Option<std::io::Result<Vec<PathBuf>>> {
    let rel = dir.strip_prefix(STD_DIR).ok()?;
    if let Some(root) = std_root() {
        let r = list(&root.join(rel)).map(|fs| fs.into_iter().filter_map(|p| p.file_name().map(|n| Path::new(STD_DIR).join(rel).join(n))).collect());
        return Some(r);
    }
    let rel = rel.to_string_lossy().replace('\\', "/");
    let fs: Vec<PathBuf> = std_files()
        .iter()
        .filter(|(p, _)| p.rsplit_once('/').map(|(d, _)| d) == Some(rel.as_str()))
        .map(|(p, _)| Path::new(STD_DIR).join(p))
        .collect();
    Some(Ok(fs))
}

/// The directory `#[embed]` patterns of file `f` are relative to: its own,
/// or for a std package, its directory under `ALX_STD_DIR` (the embedded
/// standard library has no files beside it).
fn embed_dir(f: &Path) -> PathBuf {
    let d = f.parent().unwrap_or(Path::new(".")).to_path_buf();
    match (d.strip_prefix(STD_DIR), std_root()) {
        (Ok(rel), Some(root)) => root.join(rel),
        _ => d,
    }
}

/// A std file's text: through `read` from `ALX_STD_DIR`, else the embedded copy.
fn std_read(f: &Path, read: &dyn Fn(&Path) -> std::io::Result<String>) -> Option<std::io::Result<String>> {
    let rel = f.strip_prefix(STD_DIR).ok()?;
    if let Some(root) = std_root() {
        return Some(read(&root.join(rel)));
    }
    let rel = rel.to_string_lossy().replace('\\', "/");
    std_files().iter().find(|(p, _)| *p == rel).map(|(_, t)| Ok(t.to_string()))
}

/// Is `path` a package of the standard library?
pub fn is_std(path: &str) -> bool {
    if let Some(root) = std_root() {
        return std::fs::read_dir(root.join(path)).is_ok_and(|rd| rd.flatten().any(|e| e.path().extension().is_some_and(|x| x == "alx")));
    }
    let pre = format!("{path}/");
    std_files().iter().any(|(p, _)| p.strip_prefix(&pre).is_some_and(|f| !f.contains('/')))
}

/// Where an import path lives: inside the main module (its module path
/// prefix optional, or relative to the root), or inside the required module
/// with the longest matching path.
fn pkg_dir(path: &str, imp: &Import, mods: &Mods, list: &dyn Fn(&Path) -> std::io::Result<Vec<PathBuf>>) -> Result<PathBuf, Diag> {
    if let Some(m) = mods.module.as_deref() {
        if let Some(rest) = path.strip_prefix(m).and_then(|r| r.strip_prefix('/')) {
            return Ok(mods.root.join(rest));
        }
    }
    let best = mods.deps.iter().filter(|(m, ..)| is_inside(path, m)).max_by_key(|(m, ..)| m.len());
    match best {
        Some((m, v, dir, rep)) => {
            if list(dir).is_err() {
                let msg = if *rep {
                    format!("module {m} {v} is replaced by `{}`, which doesn't exist", dir.display())
                } else {
                    format!("module {m} {v} isn't in the module cache (`{}`); alx doesn't download modules yet \u{2014} add it there or use `replace`", dir.display())
                };
                return Err(Diag::new(imp.span, msg));
            }
            Ok(dir.join(path[m.len()..].trim_start_matches('/')))
        }
        // The packages Go's std vendors (golang.org/x/crypto/cryptobyte, ...)
        // are part of alx's std under their own paths.
        None if path.split('/').next().is_some_and(|s| s.contains('.')) && is_std(path) => Ok(Path::new(STD_DIR).join(path)),
        None if path.split('/').next().is_some_and(|s| s.contains('.')) => Err(Diag::new(imp.span, format!("no required module provides package `{path}`; add `require <module> <version>` to alx.mod"))),
        // The module's own directory wins; otherwise the standard library.
        None if is_std(path) && !list(&mods.root.join(path)).is_ok_and(|fs| fs.iter().any(|f| f.extension().is_some_and(|e| e == "alx"))) => Ok(Path::new(STD_DIR).join(path)),
        None => Ok(mods.root.join(path)),
    }
}

/// `load`, reading files through `read` and listing directories through
/// `list` (the browser has no file system).
pub fn load_with(path: &Path, display: &str, read: &dyn Fn(&Path) -> std::io::Result<String>, list: &dyn Fn(&Path) -> std::io::Result<Vec<PathBuf>>) -> Result<Loaded, (SourceMap, Diag)> {
    let mut sm = SourceMap::default();
    let mut next_id = 0;
    let text = match read(path) {
        Ok(t) => t,
        Err(e) => {
            let d = Diag::new(Span::default(), format!("cannot read `{display}`: {e}"));
            sm.add_at(display.into(), Some(path.to_path_buf()), String::new());
            return Err((sm, d));
        }
    };
    let mut main = match parse_file_at(&mut sm, display.to_string(), Some(path.to_path_buf()), text, &mut next_id) {
        Ok(m) => m,
        Err(d) => return Err((sm, d)),
    };
    if let Err(d) = crate::embed::resolve_with(&mut main, path.parent().unwrap_or(Path::new(".")), read) {
        return Err((sm, d));
    }
    if main.external_test {
        return Err((sm, not_external(main.file)));
    }
    if let Some(t) = main.tests.first() {
        return Err((sm, not_a_test_file(t)));
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    let mods = match load_mods(dir, read) {
        Ok(m) => m,
        Err(d) => return Err((sm, d)),
    };
    let mut overflow = HashMap::from([(main.file, main.overflow)]);
    let mut main = main;
    if let Err(d) = add_builtins(&mut sm, &mut next_id, &mut main, &mut overflow) {
        return Err((sm, d));
    }
    let mut pkgs: Vec<Package> = vec![];
    let mut visiting: Vec<String> = vec![];
    let imports = main.imports.clone();
    for imp in &imports {
        if let Err(d) = load_pkg(imp, &mods, &mut sm, &mut next_id, &mut pkgs, &mut visiting, &mut overflow, read, list, Path::new(display).parent().unwrap_or(Path::new(""))) {
            return Err((sm, d));
        }
    }
    Ok(Loaded { sm, main, pkgs, overflow })
}

#[allow(clippy::too_many_arguments)]
fn load_pkg(
    imp: &Import,
    mods: &Mods,
    sm: &mut SourceMap,
    next_id: &mut NodeId,
    pkgs: &mut Vec<Package>,
    visiting: &mut Vec<String>,
    overflow: &mut HashMap<u32, Overflow>,
    read: &dyn Fn(&Path) -> std::io::Result<String>,
    list: &dyn Fn(&Path) -> std::io::Result<Vec<PathBuf>>,
    shown_root: &Path,
) -> Result<(), Diag> {
    let path = imp.path.trim_end_matches('/').to_string();
    if pkgs.iter().any(|p| p.path == path) {
        return Ok(());
    }
    if visiting.contains(&path) {
        return Err(Diag::new(imp.span, format!("import cycle: {} -> {path}", visiting.join(" -> "))));
    }
    let dir = pkg_dir(&path, imp, mods, list)?;
    let files: Vec<PathBuf> = match std_list(&dir, list).unwrap_or_else(|| list(&dir)) {
        Ok(fs) => fs.into_iter().filter(|f| f.extension().is_some_and(|e| e == "alx") && !f.to_string_lossy().ends_with("_test.alx")).collect(),
        Err(_) => vec![],
    };
    if files.is_empty() {
        return Err(Diag::new(imp.span, format!("cannot find package `{path}` (looked for .alx files in `{}`)", dir.display())));
    }
    visiting.push(path.clone());
    let mut merged: Option<Module> = None;
    let mut source = String::new();
    for f in &files {
        let text = match std_read(f, read).unwrap_or_else(|| read(f)) {
            Ok(t) => t,
            Err(e) => return Err(Diag::new(imp.span, format!("cannot read `{}`: {e}", f.display()))),
        };
        source.push_str(&text);
        let shown = match f.strip_prefix(STD_DIR) {
            Ok(rel) => format!("std/{}", rel.display()),
            Err(_) => shown_root.join(f.strip_prefix(&mods.root).unwrap_or(f)).display().to_string(),
        };
        // The real path: std files map to ALX_STD_DIR (the embedded copy has none).
        let real = if f.starts_with(STD_DIR) { std_real(f) } else { Some(f.clone()) };
        let mut m = parse_file_at(sm, shown, real, text, next_id)?;
        crate::embed::resolve_with(&mut m, &embed_dir(f), read)?;
        if let Some(s) = m.main.iter().find(|s| !matches!(s.kind, crate::ast::StmtKind::Using(_))) {
            return Err(Diag::new(s.span, format!("a package (`{path}`) holds only declarations; move statements into a def")).note("package-level values are constants (`NAME = 42`), or, for state that changes, `NAME = Atomic.new(v)` / `NAME = Mutex.new(v)` (R11)"));
        }
        if m.external_test {
            return Err(not_external(m.file));
        }
        if let Some(t) = m.tests.first() {
            return Err(not_a_test_file(t));
        }
        overflow.insert(m.file, m.overflow);
        for i in &m.imports {
            load_pkg(i, mods, sm, next_id, pkgs, visiting, overflow, read, list, shown_root)?;
        }
        merged = Some(match merged {
            None => m,
            Some(mut acc) => {
                merge_decls(&mut acc, m);
                acc
            }
        });
    }
    visiting.pop();
    let mut m = merged.unwrap();
    qualify(&mut m, &path);
    pkgs.push(Package { path, module: m, source });
    Ok(())
}

fn merge_decls(acc: &mut Module, m: Module) {
    acc.imports.extend(m.imports);
    acc.public.extend(m.public);
    acc.defs.extend(m.defs);
    acc.structs.extend(m.structs);
    acc.enums.extend(m.enums);
    acc.ifaces.extend(m.ifaces);
    acc.refines.extend(m.refines);
    acc.consts.extend(m.consts);
}

fn not_a_test_file(t: &TestDecl) -> Diag {
    let kw = match t.kind {
        TestKind::Test => "test",
        TestKind::Bench => "bench",
        TestKind::Example => "example",
    };
    Diag::new(t.span, format!("`{kw}` blocks belong in a `*_test.alx` file"))
}

/// One test, benchmark or example of an `alx test` run.
pub struct TestItem {
    pub kind: TestKind,
    pub name: String,
    /// The def holding the body.
    pub func: String,
    pub outputs: Option<String>,
    /// The body takes a `testing.T` (a test) or `testing.B` (a benchmark).
    pub param: bool,
}

/// What `alx test` was asked for.
pub struct TestOpts {
    /// Only tests and examples whose name contains this.
    pub run: Option<String>,
    /// Benchmarks run only when set: `.` for all, else a substring of the name.
    pub bench: Option<String>,
    pub bench_ns: i64,
    /// `-short`: testing.short? is true.
    pub short: bool,
}

/// Prefix every name a package declares with its path (`geom.area`).
fn qualify(m: &mut Module, p: &str) {
    let q = |n: &str| format!("{p}.{n}");
    for d in &mut m.defs {
        d.name = q(&d.name);
    }
    for r in &mut m.refines {
        r.name = q(&r.name);
    }
    for s in &mut m.structs {
        s.name = q(&s.name);
    }
    for e in &mut m.enums {
        e.name = q(&e.name);
    }
    for i in &mut m.ifaces {
        i.name = q(&i.name);
    }
    for c in &mut m.consts {
        c.name = q(&c.name);
    }
    m.public = m.public.iter().map(|n| q(n)).collect();
}

/// A def's entry in the world: an `extern def` carries its C signature.
fn def_info(d: &Def, overflow: Overflow, pkg: &str) -> Result<DefInfo, Diag> {
    let external = if d.ffi.is_some() { Some(crate::check::ffi_sig(d)?) } else { None };
    Ok(DefInfo { def: d.clone(), overflow, external, pkg: pkg.to_string() })
}

/// Check the whole program, every imported package in the same world.
/// `externs`: the interface of packages compiled separately (whose
/// declarations are then left out).
pub fn check_program(l: &Loaded, externs: Vec<DefInfo>) -> Result<TProgram, Diag> {
    check_program_impl(l, externs, None).map(|p| p.expect("a normal check returns a program"))
}

/// Editor analysis (analyze.rs): `check_program` in collect mode. Every
/// problem goes to `out` in emission order (the first error is the one
/// `check_program` would return); `own` says which files belong to the unit,
/// whose defs are checked even when nothing calls them.
pub fn check_collect(l: &Loaded, own: &dyn Fn(u32) -> bool, out: &mut Vec<Item>) {
    if let Err(d) = check_program_impl(l, vec![], Some((own, out))) {
        // Only an early failure that has no collecting path (e.g. a duplicate def name).
        out.push(Item { diag: d, phase: Phase::Check, error: true });
    }
}

fn check_program_impl(l: &Loaded, externs: Vec<DefInfo>, mut collect: Option<(&dyn Fn(u32) -> bool, &mut Vec<Item>)>) -> Result<Option<TProgram>, Diag> {
    let ov = |file: u32| l.overflow.get(&file).copied().unwrap_or(Overflow::Abort);
    let ext: HashSet<String> = externs.iter().map(|d| d.pkg.clone()).collect();
    let ext_names: Vec<String> = externs.iter().map(|d| d.def.name.clone()).collect();
    let mut defs: Vec<DefInfo> = l.main.defs.iter().map(|d| def_info(d, ov(d.span.file), "")).collect::<Result<_, _>>()?;
    for p in l.pkgs.iter().filter(|p| !ext.contains(&p.path)) {
        for d in &p.module.defs {
            defs.push(def_info(d, ov(d.span.file), &p.path)?);
        }
    }
    defs.extend(externs);
    // T3: a package's public defs spell their parameter types.
    let mut pre: Vec<Diag> = vec![];
    for p in &l.pkgs {
        for d in p.module.defs.iter().filter(|d| d.public) {
            if let Some(q) = d.params.iter().find(|q| q.ty.is_none() && q.name != "self") {
                let e = Diag::new(q.span, format!("`pub def` parameters need types: `{}: Type`", q.name));
                if collect.is_none() {
                    return Err(e);
                }
                pre.push(e);
            }
        }
    }
    // Visibility and each package's imports.
    let mut public: HashSet<String> = l.pkgs.iter().flat_map(|p| p.module.public.iter().cloned()).collect();
    public.extend(l.pkgs.iter().flat_map(|p| p.module.defs.iter().filter(|d| d.public).map(|d| d.name.clone())));
    public.extend(ext_names);
    let mut imports: HashMap<String, HashMap<String, String>> = HashMap::new();
    imports.insert(String::new(), l.main.imports.iter().map(|i| (import_name(i), i.path.trim_end_matches('/').to_string())).collect());
    for p in &l.pkgs {
        imports.insert(p.path.clone(), p.module.imports.iter().map(|i| (import_name(i), i.path.trim_end_matches('/').to_string())).collect());
    }
    crate::check::set_packages(public, imports);
    let mut w = World::new(&l.sm, defs)?;
    let all = || std::iter::once(&l.main).chain(l.pkgs.iter().filter(|p| !ext.contains(&p.path)).map(|p| &p.module));
    let structs: Vec<_> = all().flat_map(|m| m.structs.iter()).cloned().collect();
    let consts: Vec<_> = l.pkgs.iter().filter(|p| !ext.contains(&p.path)).flat_map(|p| p.module.consts.iter()).chain(l.main.consts.iter()).cloned().collect();
    let enums: Vec<_> = all().flat_map(|m| m.enums.iter()).cloned().collect();
    let ifaces: Vec<_> = all().flat_map(|m| m.ifaces.iter()).cloned().collect();
    let refines: Vec<_> = all().flat_map(|m| m.refines.iter()).cloned().collect();
    if collect.is_some() {
        w.collect = true;
        for e in pre {
            w.record(e);
            w.decl_failed = true;
        }
        // Declarations, in `check_program`'s order, stopping at the first kind that failed
        // (later kinds would only cascade).
        let mut step = |w: &mut World, r: Result<(), Diag>| {
            if let Err(e) = r {
                w.record(e);
                w.decl_failed = true;
            }
            !w.decl_failed
        };
        let _ = (|| {
            let r = w.add_consts(&consts);
            if !step(&mut w, r) {
                return;
            }
            let r = w.add_iface_names(&ifaces);
            if !step(&mut w, r) {
                return;
            }
            w.add_builtin_errors();
            let r = w.add_structs(&structs, &enums);
            if !step(&mut w, r) {
                return;
            }
            let r = w.add_enum_consts();
            if !step(&mut w, r) {
                return;
            }
            let r = w.add_iface_sigs(&ifaces);
            if !step(&mut w, r) {
                return;
            }
            let r = w.add_refines(&refines);
            step(&mut w, r);
        })();
    } else {
        w.add_consts(&consts)?;
        w.add_iface_names(&ifaces)?;
        w.add_builtin_errors();
        w.add_structs(&structs, &enums)?;
        w.add_enum_consts()?;
        w.add_iface_sigs(&ifaces)?;
        w.add_refines(&refines)?;
    }
    let mut main = 0;
    if let Some((own, _)) = &collect {
        if !w.decl_failed {
            // The unit's main (a script's statements, a package's test runner), then every
            // def of the unit's own files that stands alone, then what only a build needs.
            match w.check_main(&l.main.main, l.main.overflow, Span { file: l.main.file, lo: 0, hi: 0 }) {
                Ok(id) => main = id,
                Err(e) => w.record(e),
            }
            for i in 0..w.defs.len() {
                if own(w.defs[i].def.span.file) && w.is_root(i) {
                    w.check_root(i);
                }
            }
            match w.var_inits() {
                Ok(_) => {}
                Err(e) => w.record(e),
            }
            let _ = w.message_instances();
            let _ = w.iface_stringers();
            let _ = w.iface_eq_instances();
        }
    } else {
        main = w.check_main(&l.main.main, l.main.overflow, Span { file: l.main.file, lo: 0, hi: 0 })?;
    }
    if collect.is_none() {
        // R11: package-level Atomics / Mutexes are set first thing in main.
        let inits = w.var_inits()?;
        let vars: Vec<usize> = inits.iter().map(|(g, _)| *g).collect();
        if let Some(Some(mf)) = w.funcs.get_mut(main) {
            let sp = mf.span;
            let pre: Vec<crate::tast::TStmt> = inits
                .iter()
                .map(|&(g, fid)| {
                    let t = w.funcs[fid].as_ref().unwrap().ret.clone();
                    let call = crate::tast::TExpr { kind: crate::tast::TK::Call(fid, vec![]), ty: t, span: sp };
                    crate::tast::TStmt::Expr(crate::tast::TExpr { kind: crate::tast::TK::M(crate::tast::M::SetGlobal(g), None, vec![call], None), ty: Ty::Unit, span: sp })
                })
                .collect();
            let mf = w.funcs[main].as_mut().unwrap();
            mf.body.splice(0..0, pre);
        }
        let messages = w.message_instances()?;
        w.iface_stringers()?;
        let iface_eqs = w.iface_eq_instances()?;
        let ifaces = std::mem::take(&mut w.impls);
        let shareable = std::mem::take(&mut w.shareable);
        let stringers = std::mem::take(&mut w.stringers);
        let errors = std::mem::take(&mut w.errors);
        let mut warnings = std::mem::take(&mut w.warnings);
        unused_imports(l, &ext, &mut warnings);
        warnings.sort_by_key(|d| (d.span.file, d.span.lo));
        let globals = w.globals.into_iter().map(|(_, v)| v).collect();
        let mut funcs: Vec<_> = w.funcs.into_iter().map(|f| f.expect("every instance checked")).collect();
        for f in funcs.iter_mut() {
            if !f.external {
                f.overflow = if f.is_main { l.main.overflow } else { ov(f.span.file) };
            }
        }
        for f in &funcs {
            prove::prove(f, &l.sm)?;
        }
        let mut p = TProgram { funcs, main, ifaces, shareable, stringers, errors, messages, warnings, globals, vars, iface_eqs };
        // R5: lambdas see the variables they capture, not copies.
        crate::capture::convert(&mut p);
        // R6: what goes to another task isn't used here afterwards.
        crate::sharing::check(&p, &l.sm)?;
        return Ok(Some(p));
    }
    // Collect mode: what is left is reported in `check_program`'s order.
    let (_, out) = collect.as_mut().unwrap();
    let errs = std::mem::take(&mut w.diags);
    let failed = !errs.is_empty();
    out.extend(errs.into_iter().map(|diag| Item { diag, phase: Phase::Check, error: true }));
    let mut warnings = std::mem::take(&mut w.warnings);
    unused_imports(l, &ext, &mut warnings);
    warnings.sort_by_key(|d| (d.span.file, d.span.lo));
    if !failed && !w.decl_failed {
        // Every instance succeeded: the build's later passes (the prover, then the sharing check).
        // The sharing check reads the implementors (which interfaces carry storage) and
        // the `#[shareable]` conversions.
        let ifaces = std::mem::take(&mut w.impls);
        let shareable = std::mem::take(&mut w.shareable);
        let funcs: Vec<_> = w.funcs.into_iter().map(|f| f.expect("every instance checked")).collect();
        let mut funcs = funcs;
        for f in funcs.iter_mut() {
            if !f.external {
                f.overflow = if f.is_main { l.main.overflow } else { ov(f.span.file) };
            }
        }
        let mut proved = true;
        for f in &funcs {
            if let Err(d) = prove::prove(f, &l.sm) {
                proved = false;
                if !out.iter().any(|i| i.diag.span == d.span && i.diag.msg == d.msg) {
                    out.push(Item { diag: d, phase: Phase::Prove, error: true });
                }
            }
        }
        if proved {
            let mut p = TProgram { funcs, main, ifaces, shareable, stringers: Default::default(), errors: Default::default(), messages: Default::default(), warnings: vec![], globals: vec![], vars: vec![], iface_eqs: Default::default() };
            crate::capture::convert(&mut p);
            if let Err(d) = crate::sharing::check(&p, &l.sm) {
                out.push(Item { diag: d, phase: Phase::Check, error: true });
            }
        }
    }
    out.extend(warnings.into_iter().map(|diag| Item { diag, phase: Phase::Warning, error: false }));
    Ok(None)
}

/// Imports nothing refers to (packages compiled separately count as used).
fn unused_imports(l: &Loaded, ext: &HashSet<String>, warnings: &mut Vec<Diag>) {
    let mods = std::iter::once((String::new(), &l.main)).chain(l.pkgs.iter().filter(|p| !ext.contains(&p.path)).map(|p| (p.path.clone(), &p.module)));
    for (pkg, m) in mods {
        if !pkg.is_empty() && is_std(&pkg) {
            continue; // std packages import what only their uninstantiated generics use
        }
        for i in &m.imports {
            let alias = import_name(i);
            // Only reached code records uses (generic and untested defs
            // aren't checked), so a reference in the file's text counts too.
            let in_text = l.sm.files.get(i.span.file as usize).is_some_and(|f| mentions(&f.text, &alias));
            // An import a derive added for its own code (`alxjson`, `alxjson2`, ...):
            // the derived methods the program doesn't call aren't checked. Or one
            // `alx test` added (no span: the runner's, an external test's testing).
            // `import _ "pkg"` (Go's blank import) is for its initializers alone.
            if i.alias.as_deref().is_some_and(|a| matches!(a, "_" | "alxjson" | "alxjson2" | "alxjsontext" | "alxdyn") || a.starts_with("__alx")) || i.span.hi == 0 {
                continue;
            }
            if !crate::check::import_used(&pkg, &alias) && !in_text && !ext.contains(i.path.trim_end_matches('/')) {
                warnings.push(Diag::new(i.span, format!("`{}` is imported but not used", i.path)));
            }
        }
    }
}

/// Kept for callers of the old library interface: packages are now
/// checked from source with the program.
pub fn lib_defs(_l: &Loaded) -> Vec<DefInfo> {
    vec![]
}

// ---------- separately compiled libraries ----------

use crate::ast::{Def, Param};
use crate::check::ExternSig;
use crate::tast::Ty;

/// Can package `p` be compiled on its own, behind a header? Its exports
/// must be plain (non-generic, non-fallible) defs, and it may not declare
/// types whose layout depends on the whole program (interfaces, errors) or
/// import other packages.
pub fn separable(p: &Package) -> bool {
    let m = &p.module;
    m.imports.is_empty()
        && m.ifaces.is_empty()
        && !m.enums.iter().any(|e| e.error || !e.tparams.is_empty())
        && !m.structs.iter().any(|s| !s.tparams.is_empty())
        && m.defs.iter().filter(|d| d.public).all(|d| d.tparams.is_empty() && !d.fallible && !d.name.contains("].") && d.params.iter().all(|p| p.ty.is_some()))
        // The header spells types with `parse_ty`: sized ints and structs can't cross it.
        && m.defs.iter().filter(|d| d.public).all(|d| d.params.iter().all(|p| p.ty.as_ref().is_some_and(header_type)) && d.ret.as_ref().is_none_or(header_type))
        && m.defs.iter().any(|d| d.public)
        // Array constants and package-level Atomics / Mutexes (R11) live
        // in globals the program's main sets up.
        && !m.consts.iter().any(|c| c.var || matches!(c.value.kind, crate::ast::ExprKind::Array(_) | crate::ast::ExprKind::ArrayRepeat(..)))
        // The builtin Complex is declared with the program.
        && !uses_complex(&p.source)
        // So is CallSite (S9).
        && !["track_caller", "caller_location", "CallSite"].iter().any(|w| p.source.contains(w))
        // Formats with flags reach the fmt engine, declared with the program (builtin.alx).
        && !["format(", "printf(", "sprintf(", "errorf("].iter().any(|f| p.source.contains(f))
        // So is `==` on maps (`__map_eq`).
        && !(p.source.contains("Map[") && (p.source.contains("==") || p.source.contains("!=")))
        // Embedded files aren't part of the source the cache is keyed by.
        && !m.consts.iter().any(|c| c.embed.is_some())
        // `==` on a type that contains itself calls `__eq` (builtin.alx, R12).
        && !recursive_types(m)
}

/// Does a struct or enum of the module mention itself through its fields
/// (directly or through the module's other types)?
fn recursive_types(m: &crate::ast::Module) -> bool {
    let short = |n: &str| n.rsplit('.').next().unwrap_or(n).to_string();
    let mut uses: HashMap<String, Vec<String>> = HashMap::new();
    let mut add = |name: &str, fields: &mut dyn Iterator<Item = &crate::ast::TypeExpr>| {
        let mut out = vec![];
        for te in fields {
            crate::check::type_names(te, &mut out);
        }
        uses.insert(short(name), out.iter().map(|n| short(n)).collect());
    };
    for s in &m.structs {
        add(&s.name, &mut s.fields.iter().map(|f| &f.1));
    }
    for e in &m.enums {
        add(&e.name, &mut e.variants.iter().flat_map(|v| v.1.iter().map(|f| &f.1)));
    }
    uses.keys().any(|start| {
        let mut seen: Vec<&String> = vec![];
        let mut stack: Vec<&String> = uses[start].iter().collect();
        while let Some(n) = stack.pop() {
            if n == start {
                return true;
            }
            if seen.contains(&n) {
                continue;
            }
            seen.push(n);
            if let Some(next) = uses.get(n) {
                stack.extend(next.iter());
            }
        }
        false
    })
}

fn uses_complex(src: &str) -> bool {
    src.contains("Complex") || lexer::lex(0, src).map_or(true, |ts| ts.iter().any(|t| matches!(t.tok, lexer::Tok::Imag(..))))
}

fn header_type(t: &crate::ast::TypeExpr) -> bool {
    use crate::ast::TypeExpr;
    match t {
        TypeExpr::Named(n, _) => matches!(n.as_str(), "Int" | "Float" | "Bool" | "Str"),
        TypeExpr::Array(t, _) => header_type(t),
        _ => false,
    }
}

/// Check a separable package on its own: its `pub` defs are the exports.
/// Exported instances get stable, prefixed symbol names.
pub fn check_library(l: &Loaded, idx: usize, prefix: &str) -> Result<(TProgram, Vec<(String, usize)>), Diag> {
    let pkg = &l.pkgs[idx];
    let m = &pkg.module;
    let ov = |file: u32| l.overflow.get(&file).copied().unwrap_or(Overflow::Abort);
    let defs = m.defs.iter().map(|d| def_info(d, ov(d.span.file), &pkg.path)).collect::<Result<Vec<_>, _>>()?;
    crate::check::set_packages(m.public.iter().cloned().chain(m.defs.iter().filter(|d| d.public).map(|d| d.name.clone())).collect(), HashMap::from([(pkg.path.clone(), HashMap::new())]));
    let mut w = World::new(&l.sm, defs)?;
    w.add_consts(&m.consts)?;
    w.add_iface_names(&m.ifaces)?;
    w.add_builtin_errors();
    w.add_structs(&m.structs, &m.enums)?;
    w.add_enum_consts()?;
    w.add_iface_sigs(&m.ifaces)?;
    let exports = w.check_exports()?;
    let messages = w.message_instances()?;
    w.iface_stringers()?;
    let iface_eqs = w.iface_eq_instances()?;
    let ifaces = std::mem::take(&mut w.impls);
    let stringers = std::mem::take(&mut w.stringers);
    let errors = std::mem::take(&mut w.errors);
    let mut funcs: Vec<_> = w.funcs.into_iter().map(|f| f.expect("every instance checked")).collect();
    for f in funcs.iter_mut() {
        f.overflow = ov(f.span.file);
    }
    for (name, fid) in &exports {
        funcs[*fid].cname = format!("{prefix}_{}", crate::check::cname(name));
    }
    for f in &funcs {
        prove::prove(f, &l.sm)?;
    }
    Ok((TProgram { funcs, main: usize::MAX, ifaces, shareable: HashMap::new(), stringers, errors, messages, warnings: vec![], globals: vec![], vars: vec![], iface_eqs }, exports))
}

/// The generated header: one line per export.
/// `def NAME(T, ...) -> T = SYMBOL [fallible] [pure] [promote]`
pub fn header(p: &TProgram, exports: &[(String, usize)], overflow: Overflow) -> String {
    let mut s = String::from("# generated by alx: the library's interface\n");
    for (name, fid) in exports {
        let f = &p.funcs[*fid];
        let params: Vec<String> = f.params.iter().map(|l| f.locals[*l].ty.show()).collect();
        s.push_str(&format!("def {name}({}) -> {} = {}", params.join(", "), f.ret.show(), f.cname));
        if f.fallible {
            s.push_str(" fallible");
        }
        if f.pure {
            s.push_str(" pure");
        }
        if overflow == Overflow::Promote {
            s.push_str(" promote");
        }
        s.push('\n');
    }
    s
}

/// Parse a header back into extern definitions.
pub fn parse_header(text: &str, overflow: Overflow, span: Span, pkg: &str) -> Result<Vec<DefInfo>, String> {
    let mut out = vec![];
    for line in text.lines().filter(|l| l.starts_with("def ")) {
        let rest = &line[4..];
        let (name, rest) = rest.split_once('(').ok_or("bad header line")?;
        let (params, rest) = split_params(rest)?;
        let rest = rest.trim_start().strip_prefix("->").ok_or("bad header: missing ->")?;
        let (ret, rest) = rest.split_once(" = ").ok_or("bad header: missing =")?;
        let mut words = rest.split_whitespace();
        let symbol = words.next().ok_or("bad header: missing symbol")?.to_string();
        let flags: Vec<&str> = words.collect();
        let lib_promote = flags.contains(&"promote");
        if lib_promote != (overflow == Overflow::Promote) {
            return Err(format!("library `{name}` was compiled with a different overflow mode than this file"));
        }
        let ptys = params.iter().map(|p| parse_ty(p.trim())).collect::<Result<Vec<_>, _>>()?;
        let def = Def {
            public: true,
            using: vec![],
            name: name.trim().to_string(),
            span,
            tparams: vec![],
            name_span: span,
            params: (0..ptys.len()).map(|i| Param { name: format!("p{i}"), ty: None, span, default: None }).collect(),
            ret: None,
            fallible: flags.contains(&"fallible"),
            errs: Some(vec!["Error".into()]),
            pure: flags.contains(&"pure"),
            track_caller: false,
            ffi: None,
            body: vec![],
        };
        let sig = ExternSig { params: ptys, ret: parse_ty(ret.trim())?, fallible: def.fallible, pure: def.pure, symbol, ffi: false };
        out.push(DefInfo { def, overflow, external: Some(sig), pkg: pkg.to_string() });
    }
    Ok(out)
}

fn split_params(s: &str) -> Result<(Vec<String>, &str), String> {
    let mut depth = 0;
    let mut cur = String::new();
    let mut out = vec![];
    for (i, c) in s.char_indices() {
        match c {
            '(' | '[' => depth += 1,
            ')' if depth == 0 => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                return Ok((out, &s[i + 1..]));
            }
            ')' | ']' => depth -= 1,
            ',' if depth == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    Err("bad header: unterminated parameter list".into())
}

fn parse_ty(s: &str) -> Result<Ty, String> {
    let inner = |pre: &str| s.strip_prefix(pre).and_then(|r| r.strip_suffix(']'));
    Ok(match s {
        "Int" => Ty::Int,
        "Float" => Ty::Float,
        "Bool" => Ty::Bool,
        "Str" => Ty::Str,
        "nil" => Ty::Unit,
        "Range[Int]" => Ty::Range,
        _ if s.starts_with('[') && s.ends_with(']') && !s.contains(';') => Ty::arr(parse_ty(&s[1..s.len() - 1])?),
        _ if s.starts_with("Enumerator[") => Ty::Gen(Box::new(parse_ty(inner("Enumerator[").unwrap())?)),
        _ if s.starts_with('(') && s.ends_with(')') => {
            let (parts, _) = split_params(&format!("{})", &s[1..s.len() - 1]))?;
            Ty::Tuple(parts.iter().map(|p| parse_ty(p)).collect::<Result<_, _>>()?)
        }
        _ => match crate::ast::IntKind::from_name(s) {
            Some(k) => Ty::of_kind(k),
            None => return Err(format!("bad header type `{s}`")),
        },
    })
}

// ---------- alx test ----------

/// An alx string literal for `s`.
fn alx_quote(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '"' => o.push_str("\\\""),
            '#' => o.push_str("\\#"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            '\0' => o.push_str("\\0"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

const RUNNER_HELPERS: &str = r#"
def __alx_rtrim(s: Str) -> Str {
  b = s.bytes
  n = b.size
  while n > 0 && (b[n - 1] == 10 || b[n - 1] == 32 || b[n - 1] == 13 || b[n - 1] == 9) {
    n -= 1
  }
  s[0...n]
}

def __alx_msg(m: Str) -> Str {
  if m.size >= 13 && m[0...13] == "alexandrite: " {
    return m[13..]
  }
  m
}

def __alx_secs(ns: I64) -> Str {
  format("%.2f", ns.to_f / 1000000000.0)
}

def __alx_secs3(ns: I64) -> Str {
  format("%.3f", ns.to_f / 1000000000.0)
}

def __alx_fail(name: Str, ns: I64, m: Str) -> I64 {
  puts "--- FAIL: #{name} (#{__alx_secs(ns)}s)"
  for line in m.split("\n") {
    puts "    #{line}"
  }
  1
}

def __alx_pass(name: Str, ns: I64) -> I64 {
  puts "--- PASS: #{name} (#{__alx_secs(ns)}s)"
  0
}
"#;

/// The runner: a program (in alx) that runs each item as its own task.
/// `tp` qualifies the package testing's names (`testing.`, or nothing in
/// testing's own tests); `main` is the test file's `test_main`, if any.
fn runner_source(items: &[TestItem], dir: &str, o: &TestOpts, tp: &str, main: Option<&str>) -> String {
    let mut s = String::from(RUNNER_HELPERS);
    s.push_str("\ndef __alx_run_all -> Int {\n__fails = 0\n__start = Time.now_ns\n");
    let order = [TestKind::Test, TestKind::Bench, TestKind::Example];
    for kind in order {
        for it in items.iter().filter(|i| i.kind == kind) {
            let name = alx_quote(&it.name);
            let f = &it.func;
            match kind {
                TestKind::Test if it.param => {
                    s.push_str(&format!("__fails += {tp}run_test({name}, ->(t: {tp}T) -> ~Unit {{ ~{f}(t); nil }})\n"));
                }
                TestKind::Bench if it.param => {
                    s.push_str(&format!("__fails += {tp}run_bench({name}, {}, ->(b: {tp}B) -> ~Unit {{ ~{f}(b); nil }})\n", o.bench_ns));
                }
                TestKind::Test | TestKind::Example => {
                    let ex = kind == TestKind::Example;
                    s.push_str(&format!("puts \"=== RUN   \" + {name}\n__t0 = Time.now_ns\n"));
                    if ex {
                        s.push_str("Test.begin_capture\n");
                    }
                    s.push_str(&format!("__r = (spawn {{ ~{f}(); 0 }}).wait\n"));
                    if ex {
                        s.push_str("__got = __alx_rtrim(Test.end_capture)\n");
                    }
                    s.push_str("__d = Time.now_ns - __t0\n");
                    s.push_str(&format!("if __e = __r.err {{\n  __fails += 1\n  __alx_fail({name}, __d, __alx_msg(\"#{{__e}}\"))\n}} else {{\n"));
                    if ex {
                        let want = alx_quote(it.outputs.as_deref().unwrap_or("").trim_end());
                        s.push_str(&format!("  if __got == {want} {{\n    __alx_pass({name}, __d)\n  }} else {{\n    __fails += 1\n    __alx_fail({name}, __d, \"got:\\n#{{__got}}\\nwant:\\n\" + {want})\n  }}\n"));
                    } else {
                        s.push_str(&format!("  __alx_pass({name}, __d)\n"));
                    }
                    s.push_str("}\n");
                }
                TestKind::Bench => {
                    let bname = alx_quote(&format!("Benchmark{}", it.name.replace(' ', "_")));
                    s.push_str(&format!(
                        "__n = 1\n__done = false\n__bname = {bname}\nwhile __done == false {{\n  __t0 = Time.now_ns\n  __r = (spawn {{ __i = 0; while __i < __n {{ ~{f}(); __i += 1 }}; 0 }}).wait\n  __d = Time.now_ns - __t0\n  if __e = __r.err {{\n    __fails += 1\n    __alx_fail(__bname, __d, __alx_msg(\"#{{__e}}\"))\n    __done = true\n  }} else {{\n    if __d >= {} || __n >= 1000000000 {{\n      puts \"#{{__bname}}   #{{__n}}   #{{format(\"%.1f\", __d.to_f / __n.to_f)}} ns/op\"\n      __done = true\n    }} else {{\n      __pred = __n * 2\n      if __d > 0 {{\n        __pred = ({}.to_f * __n.to_f / __d.to_f).to_i\n        __pred = __pred + __pred / 5\n      }}\n      if __pred > __n * 100 {{\n        __pred = __n * 100\n      }}\n      if __pred <= __n {{\n        __pred = __n + 1\n      }}\n      if __pred > 1000000000 {{\n        __pred = 1000000000\n      }}\n      __n = __pred\n    }}\n  }}\n}}\n",
                        o.bench_ns, o.bench_ns
                    ));
                }
            }
        }
    }
    let dir = alx_quote(dir);
    s.push_str(&format!(
        "__total = Time.now_ns - __start\nif __fails > 0 {{\n  puts \"FAIL\"\n  puts \"FAIL   \" + {dir} + \" #{{__alx_secs3(__total)}}s\"\n  return 1\n}}\nputs \"PASS\"\nputs \"ok     \" + {dir} + \" #{{__alx_secs3(__total)}}s\"\n0\n}}\n"
    ));
    match main {
        // Go's TestMain: it runs the tests through m.run; its code is the exit status.
        Some(m) => s.push_str(&format!("__m = {tp}new_m(->() -> Int {{ __alx_run_all() }})\n{m}(__m)\nTest.exit(__m.code)\n")),
        None => s.push_str("__code = __alx_run_all()\nTest.exit(__code) if __code != 0\n"),
    }
    s
}

/// Load the tests of directory `dir` (or the one test file `only` in it):
/// the package's other `.alx` files and the test files are merged into one
/// module (tests see the package's private names), then a generated runner.
/// `dir_shown` is how the directory was written on the command line.
pub fn load_tests(dir: &Path, dir_shown: &str, only: Option<&Path>, o: &TestOpts) -> Result<(Loaded, usize, usize), (SourceMap, Diag)> {
    let read = |p: &Path| std::fs::read_to_string(p);
    let list = |p: &Path| -> std::io::Result<Vec<PathBuf>> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(p)?.filter_map(|e| e.ok().map(|e| e.path())).collect();
        v.sort();
        Ok(v)
    };
    let sel = match only {
        Some(f) => Tests::Only(f),
        None => Tests::All,
    };
    load_tests_with(dir, dir_shown, sel, o, &read, &list)
}

/// Which test files `load_tests_with` loads.
#[derive(Clone, Copy)]
pub enum Tests<'a> {
    /// Every `*_test.alx` in the directory.
    All,
    /// Just this one (it need not be listed: an unsaved buffer).
    Only(&'a Path),
    /// None: the package alone with an empty runner (editor analysis of a package's own files).
    NoTests,
}

/// `load_tests`, reading through `read` and listing through `list` (unsaved
/// buffers). `Tests::NoTests` is the only selection that accepts a package
/// without tests.
pub fn load_tests_with(dir: &Path, dir_shown: &str, sel: Tests, o: &TestOpts, read: ReadFn, list: &dyn Fn(&Path) -> std::io::Result<Vec<PathBuf>>) -> Result<(Loaded, usize, usize), (SourceMap, Diag)> {
    let mut sm = SourceMap::default();
    let fail = |sm: SourceMap, msg: String| Err((sm, Diag::new(Span::default(), msg)));
    let mut files: Vec<PathBuf> = match list(dir) {
        Ok(rd) => rd.into_iter().filter(|p| p.extension().is_some_and(|e| e == "alx")).collect(),
        Err(e) => {
            sm.add(dir_shown.into(), String::new());
            return fail(sm, format!("cannot read `{dir_shown}`: {e}"));
        }
    };
    files.sort();
    let is_test = |p: &Path| p.to_string_lossy().ends_with("_test.alx");
    let tests: Vec<PathBuf> = match sel {
        Tests::Only(f) => vec![f.to_path_buf()],
        Tests::All => files.iter().filter(|p| is_test(p)).cloned().collect(),
        Tests::NoTests => vec![],
    };
    if tests.is_empty() && !matches!(sel, Tests::NoTests) {
        sm.add(dir_shown.into(), String::new());
        return fail(sm, format!("no `*_test.alx` files in `{dir_shown}`"));
    }
    let shown = |f: &Path| {
        let name = f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if dir_shown == "." || dir_shown.is_empty() { name } else { format!("{}/{name}", dir_shown.trim_end_matches('/')) }
    };
    let mut next_id = 0;
    let mut overflow = HashMap::new();
    // The package's files and its in-package tests (`acc`), and the
    // external tests (`xacc`, files declaring `#![test(external)]`, S11).
    let mut acc: Option<Module> = None;
    let mut xacc: Option<Module> = None;
    let mut decls: Vec<(TestDecl, bool)> = vec![];
    // `Only`: the other test files still contribute their helpers, not their tests;
    // one that doesn't parse is skipped (the focused file's diagnostics come first).
    let siblings: Vec<PathBuf> = match sel {
        Tests::Only(f) => files.iter().filter(|p| is_test(p) && p.as_path() != f).cloned().collect(),
        _ => vec![],
    };
    for f in files.iter().filter(|p| !is_test(p)).chain(siblings.iter()).chain(tests.iter()) {
        let sibling = siblings.contains(f);
        let text = match read(f) {
            Ok(t) => t,
            Err(e) => {
                sm.add(shown(f), String::new());
                return fail(sm, format!("cannot read `{}`: {e}", f.display()));
            }
        };
        let test_file = is_test(f);
        let mut m = match parse_file_at(&mut sm, shown(f), Some(f.clone()), text, &mut next_id) {
            Ok(m) => m,
            Err(_) if sibling => continue,
            Err(d) => return Err((sm, d)),
        };
        if sibling {
            m.tests.clear();
        }
        if let Err(d) = crate::embed::resolve_with(&mut m, &embed_dir(f), read) {
            return Err((sm, d));
        }
        if let Some(s) = m.main.iter().find(|s| !matches!(s.kind, crate::ast::StmtKind::Using(_))) {
            let why = if test_file { "a test file holds only declarations and test blocks; move statements into a `test`" } else { "a package holds only declarations; move statements into a def" };
            return Err((sm, Diag::new(s.span, why)));
        }
        if m.external_test && !test_file {
            return Err((sm, not_external(m.file)));
        }
        overflow.insert(m.file, m.overflow);
        let ext = m.external_test;
        decls.extend(std::mem::take(&mut m.tests).into_iter().map(|t| (t, ext)));
        let slot = if ext { &mut xacc } else { &mut acc };
        match slot {
            None => *slot = Some(m),
            Some(a) => merge_decls(a, m),
        }
    }
    let Some(mut acc) = acc else {
        sm.add(dir_shown.into(), String::new());
        return fail(sm, format!("`{dir_shown}` has only external tests (`#![test(external)]`): there is no package to test"));
    };
    let mods = match load_mods(dir, read) {
        Ok(m) => m,
        Err(d) => return Err((sm, d)),
    };
    // A package declaring a type with a builtin's name (math/big's `Int`)
    // can't be merged into the runner's main module, where the name would
    // clash with the builtin: it is checked as a package of its own (under
    // its import path), with its tests made public for the runner to call.
    // So is a package with external tests, which import it.
    let external = xacc.is_some();
    let shadow = external || acc.structs.iter().map(|s| &s.name).chain(acc.enums.iter().map(|e| &e.name)).any(|n| crate::check::SHADOWABLE.contains(&n.as_str()));
    let own_path = {
        let d = dir_shown.trim_end_matches('/');
        // `std/x` or `/abs/path/std/x` (a std package tested by its full path).
        match d.strip_prefix("std/").or_else(|| d.rfind("/std/").map(|i| &d[i + 5..])).filter(|p| is_std(p)) {
            Some(p) => p.to_string(),
            // External tests import the package by its import path.
            None if external => import_path_of(dir, &mods),
            None => format!("_test/{}", d.rsplit('/').next().unwrap_or(d)),
        }
    };
    let xpath = format!("{own_path}_test");
    // A body taking `|t|` / `|b|` gets the package testing's T / B, and the
    // runner imports testing (unless these are testing's own tests merged
    // into it).
    let own_testing = own_path == "testing";
    let tp = if own_testing && !shadow { "" } else { "testing." };
    let is_main = |d: &Def| d.name == "test_main" && d.params.len() == 1;
    let main_in = if acc.defs.iter().any(is_main) {
        Some("__alx_pkg.")
    } else if xacc.as_ref().is_some_and(|x| x.defs.iter().any(is_main)) {
        Some("__alx_xpkg.")
    } else {
        None
    };
    let mut needs_testing = main_in.is_some();
    // Which side imports testing for its T / B (or test_main).
    let mut x_testing = main_in == Some("__alx_xpkg.");
    let mut in_testing = main_in == Some("__alx_pkg.");
    // Every body becomes a def; pick what to run.
    let mut items = vec![];
    let total = decls.len();
    for (k, (mut t, ext)) in decls.into_iter().enumerate() {
        let mut func = format!("__alx_t{k}");
        t.def.name = func.clone();
        let param = !t.def.params.is_empty();
        if let Some(p) = t.def.params.first_mut() {
            let ty = if t.kind == TestKind::Bench { "B" } else { "T" };
            let q = if ext || !own_testing { "testing." } else { "" };
            p.ty = Some(crate::ast::TypeExpr::Named(format!("{q}{ty}"), p.span));
            needs_testing = true;
            x_testing |= ext;
            in_testing |= !ext;
        }
        if shadow {
            t.def.public = true;
            func = format!("{}.{func}", if ext { "__alx_xpkg" } else { "__alx_pkg" });
        }
        match (ext, xacc.as_mut()) {
            (true, Some(x)) => x.defs.push(t.def),
            _ => acc.defs.push(t.def),
        }
        let wanted = match t.kind {
            // `a|b` matches names containing a or b (the alternation of Go's
            // regexp filters; the parts are plain substrings).
            TestKind::Bench => o.bench.as_ref().is_some_and(|b| b == "." || b.split('|').any(|p| t.name.contains(p))),
            // `-run A/B`: A picks the tests, B their subtests (package testing).
            _ => o.run.as_ref().is_none_or(|r| r.split('/').next().unwrap_or("").split('|').any(|p| t.name.contains(p))),
        };
        if wanted {
            items.push(TestItem { kind: t.kind, name: t.name, func, outputs: t.outputs, param });
        }
    }
    let (n, total) = (items.len(), total);
    let main_name = main_in.map(|pre| if shadow { format!("{pre}test_main") } else { "test_main".to_string() });
    if shadow {
        for d in acc.defs.iter_mut().chain(xacc.iter_mut().flat_map(|x| x.defs.iter_mut())).filter(|d| is_main(d)) {
            d.public = true;
        }
    }
    let testing_import = Import { alias: None, path: "testing".into(), span: Span::default() };
    let has_testing = |m: &Module| m.imports.iter().any(|i| i.path == "testing");
    if in_testing && !own_testing && !has_testing(&acc) {
        acc.imports.push(testing_import.clone());
    }
    if let Some(x) = xacc.as_mut().filter(|x| x_testing && !has_testing(x)) {
        x.imports.push(testing_import.clone());
    }
    let mut runner = match parse_file(&mut sm, "<alx test>".into(), runner_source(&items, dir_shown, o, tp, main_name.as_deref()), &mut next_id) {
        Ok(m) => m,
        Err(d) => return Err((sm, d)),
    };
    // The runner is the main module; the merged declarations join it.
    let imports = acc.imports.clone();
    let own = if shadow {
        qualify(&mut acc, &own_path);
        runner.imports.push(Import { alias: Some("__alx_pkg".into()), path: own_path.clone(), span: Span::default() });
        if let Some(x) = xacc.as_mut() {
            qualify(x, &xpath);
            runner.imports.push(Import { alias: Some("__alx_xpkg".into()), path: xpath.clone(), span: Span::default() });
        }
        if needs_testing {
            runner.imports.push(testing_import);
        }
        Some(acc)
    } else {
        merge_decls(&mut runner, acc);
        None
    };
    let mut pkgs: Vec<Package> = vec![];
    let mut visiting: Vec<String> = vec![];
    let root = if dir_shown == "." { PathBuf::new() } else { PathBuf::from(dir_shown) };
    for imp in &imports {
        if let Err(d) = load_pkg(imp, &mods, &mut sm, &mut next_id, &mut pkgs, &mut visiting, &mut overflow, read, list, &root) {
            return Err((sm, d));
        }
    }
    if let Some(m) = own {
        pkgs.retain(|p| p.path != own_path);
        pkgs.push(Package { path: own_path.clone(), module: m, source: String::new() });
    }
    // The external tests' imports come after the package itself, so that
    // theirs (and those of packages they import, testing/fstest's io/fs)
    // are this one: the package is compiled once.
    if let Some(x) = xacc {
        for imp in &x.imports {
            if let Err(d) = load_pkg(imp, &mods, &mut sm, &mut next_id, &mut pkgs, &mut visiting, &mut overflow, read, list, &root) {
                return Err((sm, d));
            }
        }
        if !x.imports.iter().any(|i| i.path.trim_end_matches('/') == own_path) {
            let sp = Span { file: x.file, lo: 0, hi: 0 };
            return Err((sm, Diag::new(sp, format!("an external test imports the package it tests: `import \"{own_path}\"`"))));
        }
        pkgs.push(Package { path: xpath, module: x, source: String::new() });
    }
    overflow.insert(runner.file, Overflow::Abort);
    if let Err(d) = add_builtins(&mut sm, &mut next_id, &mut runner, &mut overflow) {
        return Err((sm, d));
    }
    Ok((Loaded { sm, main: runner, pkgs, overflow }, n, total))
}

/// A non-test file declaring `#![test(external)]`.
fn not_external(file: u32) -> Diag {
    Diag::new(Span { file, lo: 0, hi: 0 }, "`#![test(external)]` belongs in a `*_test.alx` file")
}

/// The import path of the package in `dir` (for its external tests): its
/// path inside the module (`alx.mod`), or, outside a module, the
/// directory's name.
fn import_path_of(dir: &Path, mods: &Mods) -> String {
    let canon = |p: &Path| std::fs::canonicalize(p).ok();
    let name = || canon(dir).and_then(|d| d.file_name().map(|n| n.to_string_lossy().to_string())).unwrap_or_else(|| "pkg".into());
    match (&mods.module, canon(dir), canon(&mods.root)) {
        (Some(m), Some(d), Some(r)) => match d.strip_prefix(&r) {
            Ok(rel) if rel.as_os_str().is_empty() => m.clone(),
            Ok(rel) => format!("{m}/{}", rel.to_string_lossy().replace('\\', "/")),
            Err(_) => name(),
        },
        _ => name(),
    }
}

/// Does `text` use `alias.` as a qualifier (not as part of a longer name)?
fn mentions(text: &str, alias: &str) -> bool {
    let pat = format!("{alias}.");
    let b = text.as_bytes();
    let mut from = 0;
    while let Some(k) = text[from..].find(&pat) {
        let at = from + k;
        let before = if at == 0 { b' ' } else { b[at - 1] };
        if !(before.is_ascii_alphanumeric() || before == b'_' || before == b'.') {
            return true;
        }
        from = at + 1;
    }
    false
}
