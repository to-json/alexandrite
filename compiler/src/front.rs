//! Front end: load, parse (with imported packages), check, prove.

use crate::ast::{import_name, Import, Module, NodeId, Overflow, TestDecl, TestKind};
use std::collections::{HashMap, HashSet};
use crate::check::{DefInfo, World};
use crate::diag::{Diag, SourceMap, Span};
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
    let f = sm.add(display, text.clone());
    let toks = lexer::lex(f, &text)?;
    parser::parse(f, &toks, next_id)
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

/// Where the embedded standard library appears as a directory.
pub const STD_DIR: &str = "$std";

/// The files of an embedded std package directory.
fn std_list(dir: &Path) -> Option<Vec<PathBuf>> {
    let rel = dir.strip_prefix(STD_DIR).ok()?.to_string_lossy().replace('\\', "/");
    let fs: Vec<PathBuf> = stdlib::STD
        .iter()
        .filter(|(p, _)| p.rsplit_once('/').map(|(d, _)| d) == Some(rel.as_str()))
        .map(|(p, _)| Path::new(STD_DIR).join(p))
        .collect();
    Some(fs)
}

fn std_read(f: &Path) -> Option<String> {
    let rel = f.strip_prefix(STD_DIR).ok()?.to_string_lossy().replace('\\', "/");
    stdlib::STD.iter().find(|(p, _)| *p == rel).map(|(_, t)| t.to_string())
}

/// Is `path` a package of the standard library?
pub fn is_std(path: &str) -> bool {
    let pre = format!("{path}/");
    stdlib::STD.iter().any(|(p, _)| p.strip_prefix(&pre).is_some_and(|f| !f.contains('/')))
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
            sm.add(display.into(), String::new());
            return Err((sm, d));
        }
    };
    let main = match parse_file(&mut sm, display.to_string(), text, &mut next_id) {
        Ok(m) => m,
        Err(d) => return Err((sm, d)),
    };
    if let Some(t) = main.tests.first() {
        return Err((sm, not_a_test_file(t)));
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    let mods = match load_mods(dir, read) {
        Ok(m) => m,
        Err(d) => return Err((sm, d)),
    };
    let mut overflow = HashMap::from([(main.file, main.overflow)]);
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
    let files: Vec<PathBuf> = match std_list(&dir).map(Ok).unwrap_or_else(|| list(&dir)) {
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
        let text = match std_read(f) {
            Some(t) => t,
            None => read(f).map_err(|e| Diag::new(imp.span, format!("cannot read `{}`: {e}", f.display())))?,
        };
        source.push_str(&text);
        let shown = match f.strip_prefix(STD_DIR) {
            Ok(rel) => format!("std/{}", rel.display()),
            Err(_) => shown_root.join(f.strip_prefix(&mods.root).unwrap_or(f)).display().to_string(),
        };
        let m = parse_file(sm, shown, text, next_id)?;
        if let Some(s) = m.main.first() {
            return Err(Diag::new(s.span, format!("a package (`{path}`) holds only declarations; move statements into a def")));
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
}

/// What `alx test` was asked for.
pub struct TestOpts {
    /// Only tests and examples whose name contains this.
    pub run: Option<String>,
    /// Benchmarks run only when set: `.` for all, else a substring of the name.
    pub bench: Option<String>,
    pub bench_ns: i64,
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

/// Check the whole program, every imported package in the same world.
/// `externs`: the interface of packages compiled separately (whose
/// declarations are then left out).
pub fn check_program(l: &Loaded, externs: Vec<DefInfo>) -> Result<TProgram, Diag> {
    let ov = |file: u32| l.overflow.get(&file).copied().unwrap_or(Overflow::Abort);
    let ext: HashSet<String> = externs.iter().map(|d| d.pkg.clone()).collect();
    let ext_names: Vec<String> = externs.iter().map(|d| d.def.name.clone()).collect();
    let mut defs: Vec<DefInfo> = l.main.defs.iter().map(|d| DefInfo { def: d.clone(), overflow: ov(d.span.file), external: None, pkg: String::new() }).collect();
    for p in l.pkgs.iter().filter(|p| !ext.contains(&p.path)) {
        defs.extend(p.module.defs.iter().map(|d| DefInfo { def: d.clone(), overflow: ov(d.span.file), external: None, pkg: p.path.clone() }));
    }
    defs.extend(externs);
    // T3: a package's public defs spell their parameter types.
    for p in &l.pkgs {
        for d in p.module.defs.iter().filter(|d| d.public) {
            if let Some(q) = d.params.iter().find(|q| q.ty.is_none() && q.name != "self") {
                return Err(Diag::new(q.span, format!("`pub def` parameters need types: `{}: Type`", q.name)));
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
    w.add_consts(&consts)?;
    let enums: Vec<_> = all().flat_map(|m| m.enums.iter()).cloned().collect();
    let ifaces: Vec<_> = all().flat_map(|m| m.ifaces.iter()).cloned().collect();
    w.add_iface_names(&ifaces)?;
    w.add_builtin_errors();
    w.add_structs(&structs, &enums)?;
    w.add_iface_sigs(&ifaces)?;
    let refines: Vec<_> = all().flat_map(|m| m.refines.iter()).cloned().collect();
    w.add_refines(&refines)?;
    let main = w.check_main(&l.main.main, l.main.overflow, Span { file: l.main.file, lo: 0, hi: 0 })?;
    let messages = w.message_instances()?;
    let ifaces = std::mem::take(&mut w.impls);
    let stringers = std::mem::take(&mut w.stringers);
    let errors = std::mem::take(&mut w.errors);
    let mut warnings = std::mem::take(&mut w.warnings);
    // Imports nothing refers to (packages compiled separately count as used).
    let mods = std::iter::once((String::new(), &l.main)).chain(l.pkgs.iter().filter(|p| !ext.contains(&p.path)).map(|p| (p.path.clone(), &p.module)));
    for (pkg, m) in mods {
        for i in &m.imports {
            let alias = import_name(i);
            if !crate::check::import_used(&pkg, &alias) && !ext.contains(i.path.trim_end_matches('/')) {
                warnings.push(Diag::new(i.span, format!("`{}` is imported but not used", i.path)));
            }
        }
    }
    warnings.sort_by_key(|d| (d.span.file, d.span.lo));
    let mut funcs: Vec<_> = w.funcs.into_iter().map(|f| f.expect("every instance checked")).collect();
    for f in funcs.iter_mut() {
        if !f.external {
            f.overflow = if f.is_main { l.main.overflow } else { ov(f.span.file) };
        }
    }
    for f in &funcs {
        prove::prove(f, &l.sm)?;
    }
    let mut p = TProgram { funcs, main, ifaces, stringers, errors, messages, warnings };
    // R5: lambdas see the variables they capture, not copies.
    crate::capture::convert(&mut p);
    // R6: what goes to another task isn't used here afterwards.
    crate::sharing::check(&p, &l.sm)?;
    Ok(p)
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
        && m.defs.iter().any(|d| d.public)
}

/// Check a separable package on its own: its `pub` defs are the exports.
/// Exported instances get stable, prefixed symbol names.
pub fn check_library(l: &Loaded, idx: usize, prefix: &str) -> Result<(TProgram, Vec<(String, usize)>), Diag> {
    let pkg = &l.pkgs[idx];
    let m = &pkg.module;
    let ov = |file: u32| l.overflow.get(&file).copied().unwrap_or(Overflow::Abort);
    let defs = m.defs.iter().map(|d| DefInfo { def: d.clone(), overflow: ov(d.span.file), external: None, pkg: pkg.path.clone() }).collect();
    crate::check::set_packages(m.public.iter().cloned().chain(m.defs.iter().filter(|d| d.public).map(|d| d.name.clone())).collect(), HashMap::from([(pkg.path.clone(), HashMap::new())]));
    let mut w = World::new(&l.sm, defs)?;
    w.add_consts(&m.consts)?;
    w.add_iface_names(&m.ifaces)?;
    w.add_builtin_errors();
    w.add_structs(&m.structs, &m.enums)?;
    w.add_iface_sigs(&m.ifaces)?;
    let exports = w.check_exports()?;
    let messages = w.message_instances()?;
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
    Ok((TProgram { funcs, main: usize::MAX, ifaces, stringers, errors, messages, warnings: vec![] }, exports))
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
            params: (0..ptys.len()).map(|i| Param { name: format!("p{i}"), ty: None, span }).collect(),
            ret: None,
            fallible: flags.contains(&"fallible"),
            errs: Some(vec!["Error".into()]),
            pure: flags.contains(&"pure"),
            body: vec![],
        };
        let sig = ExternSig { params: ptys, ret: parse_ty(ret.trim())?, fallible: def.fallible, pure: def.pure, symbol };
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
        _ => return Err(format!("bad header type `{s}`")),
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

def __alx_secs(ns: Int) -> Str {
  format("%.2f", ns.to_f / 1000000000.0)
}

def __alx_secs3(ns: Int) -> Str {
  format("%.3f", ns.to_f / 1000000000.0)
}

def __alx_fail(name: Str, ns: Int, m: Str) -> Int {
  puts "--- FAIL: #{name} (#{__alx_secs(ns)}s)"
  for line in m.split("\n") {
    puts "    #{line}"
  }
  1
}

def __alx_pass(name: Str, ns: Int) -> Int {
  puts "--- PASS: #{name} (#{__alx_secs(ns)}s)"
  0
}
"#;

/// The runner: a program (in alx) that runs each item as its own task.
fn runner_source(items: &[TestItem], dir: &str, o: &TestOpts) -> String {
    let mut s = String::from(RUNNER_HELPERS);
    s.push_str("\n__fails = 0\n__start = Time.now_ns\n");
    let order = [TestKind::Test, TestKind::Bench, TestKind::Example];
    for kind in order {
        for it in items.iter().filter(|i| i.kind == kind) {
            let name = alx_quote(&it.name);
            let f = &it.func;
            match kind {
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
        "__total = Time.now_ns - __start\nif __fails > 0 {{\n  puts \"FAIL\"\n  puts \"FAIL   \" + {dir} + \" #{{__alx_secs3(__total)}}s\"\n  Test.exit(1)\n}}\nputs \"PASS\"\nputs \"ok     \" + {dir} + \" #{{__alx_secs3(__total)}}s\"\n"
    ));
    s
}

/// Load the tests of directory `dir` (or the one test file `only` in it):
/// the package's other `.alx` files and the test files are merged into one
/// module (tests see the package's private names), then a generated runner.
/// `dir_shown` is how the directory was written on the command line.
pub fn load_tests(dir: &Path, dir_shown: &str, only: Option<&Path>, o: &TestOpts) -> Result<(Loaded, usize, usize), (SourceMap, Diag)> {
    let mut sm = SourceMap::default();
    let fail = |sm: SourceMap, msg: String| Err((sm, Diag::new(Span::default(), msg)));
    let mut files: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|e| e == "alx")).collect(),
        Err(e) => {
            sm.add(dir_shown.into(), String::new());
            return fail(sm, format!("cannot read `{dir_shown}`: {e}"));
        }
    };
    files.sort();
    let is_test = |p: &Path| p.to_string_lossy().ends_with("_test.alx");
    let tests: Vec<PathBuf> = match only {
        Some(f) => vec![f.to_path_buf()],
        None => files.iter().filter(|p| is_test(p)).cloned().collect(),
    };
    if tests.is_empty() {
        sm.add(dir_shown.into(), String::new());
        return fail(sm, format!("no `*_test.alx` files in `{dir_shown}`"));
    }
    let shown = |f: &Path| {
        let name = f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if dir_shown == "." || dir_shown.is_empty() { name } else { format!("{}/{name}", dir_shown.trim_end_matches('/')) }
    };
    let mut next_id = 0;
    let mut overflow = HashMap::new();
    let mut acc: Option<Module> = None;
    let mut decls: Vec<TestDecl> = vec![];
    for f in files.iter().filter(|p| !is_test(p)).chain(tests.iter()) {
        let text = match std::fs::read_to_string(f) {
            Ok(t) => t,
            Err(e) => {
                sm.add(shown(f), String::new());
                return fail(sm, format!("cannot read `{}`: {e}", f.display()));
            }
        };
        let test_file = is_test(f);
        let mut m = match parse_file(&mut sm, shown(f), text, &mut next_id) {
            Ok(m) => m,
            Err(d) => return Err((sm, d)),
        };
        if let Some(s) = m.main.first() {
            let why = if test_file { "a test file holds only declarations and test blocks; move statements into a `test`" } else { "a package holds only declarations; move statements into a def" };
            return Err((sm, Diag::new(s.span, why)));
        }
        overflow.insert(m.file, m.overflow);
        decls.append(&mut m.tests);
        match &mut acc {
            None => acc = Some(m),
            Some(a) => merge_decls(a, m),
        }
    }
    let mut acc = acc.unwrap();
    // Every body becomes a def; pick what to run.
    let mut items = vec![];
    let total = decls.len();
    for (k, mut t) in decls.into_iter().enumerate() {
        let func = format!("__alx_t{k}");
        t.def.name = func.clone();
        acc.defs.push(t.def);
        let wanted = match t.kind {
            TestKind::Bench => o.bench.as_ref().is_some_and(|b| b == "." || t.name.contains(b.as_str())),
            _ => o.run.as_ref().is_none_or(|r| t.name.contains(r.as_str())),
        };
        if wanted {
            items.push(TestItem { kind: t.kind, name: t.name, func, outputs: t.outputs });
        }
    }
    let (n, total) = (items.len(), total);
    let mut runner = match parse_file(&mut sm, "<alx test>".into(), runner_source(&items, dir_shown, o), &mut next_id) {
        Ok(m) => m,
        Err(d) => return Err((sm, d)),
    };
    let read = |p: &Path| std::fs::read_to_string(p);
    let mods = match load_mods(dir, &read) {
        Ok(m) => m,
        Err(d) => return Err((sm, d)),
    };
    // The runner is the main module; the merged declarations join it.
    let imports = acc.imports.clone();
    merge_decls(&mut runner, acc);
    let list = |p: &Path| -> std::io::Result<Vec<PathBuf>> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(p)?.filter_map(|e| e.ok().map(|e| e.path())).collect();
        v.sort();
        Ok(v)
    };
    let mut pkgs: Vec<Package> = vec![];
    let mut visiting: Vec<String> = vec![];
    let root = if dir_shown == "." { PathBuf::new() } else { PathBuf::from(dir_shown) };
    for imp in &imports {
        if let Err(d) = load_pkg(imp, &mods, &mut sm, &mut next_id, &mut pkgs, &mut visiting, &mut overflow, &read, &list, &root) {
            return Err((sm, d));
        }
    }
    overflow.insert(runner.file, Overflow::Abort);
    Ok((Loaded { sm, main: runner, pkgs, overflow }, n, total))
}
