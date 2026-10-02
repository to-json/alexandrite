//! Front end: load, parse (with `require`d libraries), check, prove.

use crate::ast::{Module, NodeId, Overflow};
use crate::check::{DefInfo, World};
use crate::diag::{Diag, SourceMap, Span};
use crate::tast::TProgram;
use crate::{lexer, parser, prove};
use std::path::{Path, PathBuf};

pub struct Loaded {
    pub sm: SourceMap,
    pub main: Module,
    /// Required libraries: (path as written, resolved file, module).
    pub libs: Vec<(String, PathBuf, Module)>,
}

pub fn parse_file(sm: &mut SourceMap, display: String, text: String, next_id: &mut NodeId) -> Result<Module, Diag> {
    let f = sm.add(display, text.clone());
    let toks = lexer::lex(f, &text)?;
    parser::parse(f, &toks, next_id)
}

pub fn load(path: &Path, display: &str) -> Result<Loaded, (SourceMap, Diag)> {
    load_with(path, display, &|p| std::fs::read_to_string(p))
}

/// `load`, reading files through `read` (the browser has no file system).
pub fn load_with(path: &Path, display: &str, read: &dyn Fn(&Path) -> std::io::Result<String>) -> Result<Loaded, (SourceMap, Diag)> {
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
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut libs = vec![];
    for (req, sp) in &main.requires {
        let file = dir.join(format!("{req}.alx"));
        let text = match read(&file) {
            Ok(t) => t,
            Err(_) => return Err((sm, Diag::new(*sp, format!("cannot find `{req}` (looked for `{}`)", file.display())))),
        };
        let shown = format!("{req}.alx");
        match parse_file(&mut sm, shown, text, &mut next_id) {
            Ok(m) => {
                if !m.main.is_empty() {
                    let sp = m.main[0].span;
                    return Err((sm, Diag::new(sp, "a required library may only contain definitions")));
                }
                libs.push((req.clone(), file, m));
            }
            Err(d) => return Err((sm, d)),
        }
    }
    Ok(Loaded { sm, main, libs })
}

/// Check the whole program with every library's source in the same world.
pub fn check_program(l: &Loaded, externs: Vec<DefInfo>) -> Result<TProgram, Diag> {
    let mut defs: Vec<DefInfo> = l.main.defs.iter().map(|d| DefInfo { def: d.clone(), overflow: l.main.overflow, external: None }).collect();
    defs.extend(externs);
    let mut w = World::new(&l.sm, defs)?;
    let structs: Vec<_> = l.main.structs.iter().chain(l.libs.iter().flat_map(|(_, _, m)| m.structs.iter())).cloned().collect();
    let consts: Vec<_> = l.libs.iter().flat_map(|(_, _, m)| m.consts.iter()).chain(l.main.consts.iter()).cloned().collect();
    w.add_consts(&consts)?;
    let enums: Vec<_> = l.main.enums.iter().chain(l.libs.iter().flat_map(|(_, _, m)| m.enums.iter())).cloned().collect();
    let ifaces: Vec<_> = l.main.ifaces.iter().chain(l.libs.iter().flat_map(|(_, _, m)| m.ifaces.iter())).cloned().collect();
    w.add_iface_names(&ifaces)?;
    w.add_builtin_errors();
    w.add_structs(&structs, &enums)?;
    w.add_iface_sigs(&ifaces)?;
    let main = w.check_main(&l.main.main, l.main.overflow, Span { file: l.main.file, lo: 0, hi: 0 })?;
    let messages = w.message_instances()?;
    let ifaces = std::mem::take(&mut w.impls);
    let stringers = std::mem::take(&mut w.stringers);
    let errors = std::mem::take(&mut w.errors);
    let mut funcs: Vec<_> = w.funcs.into_iter().map(|f| f.expect("every instance checked")).collect();
    for f in funcs.iter_mut() {
        if !f.external {
            f.overflow = if f.is_main { l.main.overflow } else { f.overflow_from(&l.main, &l.libs) };
        }
    }
    for f in &funcs {
        prove::prove(f, &l.sm)?;
    }
    Ok(TProgram { funcs, main, ifaces, stringers, errors, messages })
}

/// Lib defs as world entries (checked from source).
pub fn lib_defs(l: &Loaded) -> Vec<DefInfo> {
    l.libs.iter().flat_map(|(_, _, m)| m.defs.iter().map(|d| DefInfo { def: d.clone(), overflow: m.overflow, external: None })).collect()
}

trait OverflowOf {
    fn overflow_from(&self, main: &Module, libs: &[(String, PathBuf, Module)]) -> Overflow;
}

impl OverflowOf for crate::tast::TFunc {
    fn overflow_from(&self, main: &Module, libs: &[(String, PathBuf, Module)]) -> Overflow {
        if self.span.file == main.file {
            return main.overflow;
        }
        libs.iter().find(|(_, _, m)| m.file == self.span.file).map_or(Overflow::Abort, |(_, _, m)| m.overflow)
    }
}

// ---------- separately compiled libraries ----------

use crate::ast::{Def, Param};
use crate::check::ExternSig;
use crate::tast::Ty;

/// Check a required library on its own: every def is an export.
/// Exported instances get stable, prefixed symbol names.
pub fn check_library(l: &Loaded, idx: usize, prefix: &str) -> Result<(TProgram, Vec<(String, usize)>), Diag> {
    let (_, _, m) = &l.libs[idx];
    let defs = m.defs.iter().map(|d| DefInfo { def: d.clone(), overflow: m.overflow, external: None }).collect();
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
        f.overflow = m.overflow;
    }
    for (name, fid) in &exports {
        funcs[*fid].cname = format!("{prefix}_{}", crate::check::cname(name));
    }
    for f in &funcs {
        prove::prove(f, &l.sm)?;
    }
    Ok((TProgram { funcs, main: usize::MAX, ifaces, stringers, errors, messages }, exports))
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
pub fn parse_header(text: &str, overflow: Overflow, span: Span) -> Result<Vec<DefInfo>, String> {
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
        out.push(DefInfo { def, overflow, external: Some(sig) });
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
        _ if s.starts_with("Array[") => Ty::arr(parse_ty(inner("Array[").unwrap())?),
        _ if s.starts_with("Enumerator[") => Ty::Gen(Box::new(parse_ty(inner("Enumerator[").unwrap())?)),
        _ if s.starts_with('(') && s.ends_with(')') => {
            let (parts, _) = split_params(&format!("{})", &s[1..s.len() - 1]))?;
            Ty::Tuple(parts.iter().map(|p| parse_ty(p)).collect::<Result<_, _>>()?)
        }
        _ => return Err(format!("bad header type `{s}`")),
    })
}
