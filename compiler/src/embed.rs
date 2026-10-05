//! `#[embed("pattern", ...)]` (Go's `//go:embed`, D59): find the files a
//! constant embeds, by Go's rules (cmd/go's resolveEmbed), and read them.
//! The checker turns them into the constant's value (`CVal::Embed`).

use crate::ast::Module;
use crate::diag::Diag;
use std::collections::BTreeSet;
use std::path::Path;

/// Resolve the patterns of every embedded constant of `m`, declared in a
/// file in directory `dir`.
pub fn resolve(m: &mut Module, dir: &Path) -> Result<(), Diag> {
    for c in &mut m.consts {
        let Some(e) = &mut c.embed else { continue };
        let names = resolve_patterns(dir, &e.patterns).map_err(|msg| Diag::new(e.span, msg))?;
        let mut files = vec![];
        for n in &names {
            let data = std::fs::read(dir.join(n)).map_err(|err| Diag::new(e.span, format!("cannot embed `{n}`: {err}")))?;
            files.push((n.clone(), data));
        }
        // Go's embed.FS lists every directory leading to a file, named with a trailing `/`.
        let mut dirs = BTreeSet::new();
        for n in &names {
            let mut d = n.as_str();
            while let Some(k) = d.rfind('/') {
                d = &d[..k];
                dirs.insert(format!("{d}/"));
            }
        }
        files.extend(dirs.into_iter().map(|d| (d, vec![])));
        files.sort_by(|a, b| split(&a.0).cmp(&split(&b.0)));
        e.files = std::rc::Rc::new(files);
    }
    Ok(())
}

/// Go's embed `split`: (directory, element) of a name, a trailing `/` dropped.
fn split(name: &str) -> (&str, &str) {
    let name = name.strip_suffix('/').unwrap_or(name);
    match name.rfind('/') {
        Some(i) => (&name[..i], &name[i + 1..]),
        None => (".", name),
    }
}

/// The files (relative, slash-separated, sorted) the patterns name.
fn resolve_patterns(dir: &Path, patterns: &[String]) -> Result<Vec<String>, String> {
    let mut all_files = BTreeSet::new();
    for pattern in patterns {
        let err = |m: String| format!("pattern {pattern}: {m}");
        let (glob, all) = match pattern.strip_prefix("all:") {
            Some(g) => (g, true),
            None => (pattern.as_str(), false),
        };
        if glob == "." || !valid_path(glob) || match_pattern(glob, "").is_err() {
            return Err(err("invalid pattern syntax".into()));
        }
        let mut list = BTreeSet::new();
        for rel in glob_dir(dir, glob) {
            let path = dir.join(&rel);
            let meta = std::fs::symlink_metadata(&path).map_err(|e| err(e.to_string()))?;
            let what = if meta.is_dir() { "directory" } else { "file" };
            // Every element on the way must be a good name, and no directory a module of its own.
            let elems: Vec<&str> = rel.split('/').collect();
            for k in (0..elems.len()).rev() {
                let sub = elems[..=k].join("/");
                if k < elems.len() - 1 && dir.join(&sub).join("alx.mod").exists() {
                    return Err(err(format!("cannot embed {what} {rel}: in different module")));
                }
                if bad_name(elems[k]) {
                    return Err(err(if k == elems.len() - 1 { format!("cannot embed {what} {rel}: invalid name {}", elems[k]) } else { format!("cannot embed {what} {rel}: in invalid directory {}", elems[k]) }));
                }
            }
            if meta.is_file() {
                list.insert(rel);
            } else if meta.is_dir() {
                let mut found = BTreeSet::new();
                walk(dir, &rel, all, &mut found).map_err(err)?;
                if found.is_empty() {
                    return Err(err(format!("cannot embed directory {rel}: contains no embeddable files")));
                }
                list.extend(found);
            } else {
                return Err(err(format!("cannot embed irregular file {rel}")));
            }
        }
        if list.is_empty() {
            return Err(err("no matching files found".into()));
        }
        all_files.extend(list);
    }
    Ok(all_files.into_iter().collect())
}

/// The regular files under directory `rel`, skipping names starting with
/// `.` or `_` unless `all`, and subdirectories that are modules.
fn walk(dir: &Path, rel: &str, all: bool, out: &mut BTreeSet<String>) -> Result<(), String> {
    let mut names: Vec<String> = std::fs::read_dir(dir.join(rel)).map_err(|e| e.to_string())?.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect();
    names.sort();
    for name in names {
        let sub = format!("{rel}/{name}");
        let hidden = name.starts_with('.') || name.starts_with('_');
        let meta = std::fs::symlink_metadata(dir.join(&sub)).map_err(|e| e.to_string())?;
        if bad_name(&name) || (hidden && !all) {
            if meta.is_dir() || hidden {
                continue;
            }
            return Err(format!("cannot embed file {sub}: invalid name {name}"));
        }
        if meta.is_dir() {
            if dir.join(&sub).join("alx.mod").exists() {
                continue;
            }
            walk(dir, &sub, all, out)?;
        } else if meta.is_file() {
            out.insert(sub);
        }
    }
    Ok(())
}

/// A name that can't be embedded (version-control directories, and names
/// a module can't hold).
fn bad_name(name: &str) -> bool {
    name.is_empty() || matches!(name, ".bzr" | ".hg" | ".git" | ".svn") || name.chars().any(|c| c.is_control() || "\"*<>?`'|:\\".contains(c))
}

/// Go's fs.ValidPath: unrooted, slash-separated, no empty, `.` or `..` elements.
fn valid_path(p: &str) -> bool {
    p == "." || (!p.is_empty() && p.split('/').all(|e| !e.is_empty() && e != "." && e != ".."))
}

/// The paths (relative to `dir`) that glob pattern `pat` matches, element by element.
fn glob_dir(dir: &Path, pat: &str) -> Vec<String> {
    let mut cur = vec![String::new()];
    for elem in pat.split('/') {
        let mut next = vec![];
        for base in &cur {
            let at = if base.is_empty() { dir.to_path_buf() } else { dir.join(base) };
            let join = |n: &str| if base.is_empty() { n.to_string() } else { format!("{base}/{n}") };
            if !elem.contains(['*', '?', '[', '\\']) {
                if std::fs::symlink_metadata(at.join(elem)).is_ok() {
                    next.push(join(elem));
                }
                continue;
            }
            let Ok(rd) = std::fs::read_dir(&at) else { continue };
            let mut names: Vec<String> = rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect();
            names.sort();
            for n in names {
                if match_pattern(elem, &n) == Ok(true) {
                    next.push(join(&n));
                }
            }
        }
        cur = next;
    }
    cur
}

/// Go's path.Match: does `name` match shell pattern `pat`? Err on a
/// malformed pattern (checked through the whole pattern, as Go does).
pub fn match_pattern(pat: &str, name: &str) -> Result<bool, ()> {
    let p: Vec<char> = pat.chars().collect();
    let n: Vec<char> = name.chars().collect();
    validate(&p)?;
    Ok(matches(&p, &n))
}

fn validate(p: &[char]) -> Result<(), ()> {
    let mut i = 0;
    while i < p.len() {
        match p[i] {
            '\\' => {
                i += 1;
                if i >= p.len() {
                    return Err(());
                }
                i += 1;
            }
            '[' => {
                i += 1;
                if i < p.len() && p[i] == '^' {
                    i += 1;
                }
                let mut first = true;
                loop {
                    if i >= p.len() {
                        return Err(());
                    }
                    if p[i] == ']' && !first {
                        i += 1;
                        break;
                    }
                    if p[i] == ']' {
                        return Err(());
                    }
                    first = false;
                    let lo = class_char(p, &mut i)?;
                    if i < p.len() && p[i] == '-' {
                        i += 1;
                        let hi = class_char(p, &mut i)?;
                        if hi < lo {
                            return Err(());
                        }
                    }
                }
            }
            _ => i += 1,
        }
    }
    Ok(())
}

fn class_char(p: &[char], i: &mut usize) -> Result<char, ()> {
    if *i >= p.len() || p[*i] == '-' || p[*i] == ']' {
        return Err(());
    }
    if p[*i] == '\\' {
        *i += 1;
        if *i >= p.len() {
            return Err(());
        }
    }
    let c = p[*i];
    *i += 1;
    Ok(c)
}

fn matches(p: &[char], n: &[char]) -> bool {
    if p.is_empty() {
        return n.is_empty();
    }
    match p[0] {
        '*' => (0..=n.len()).take_while(|&k| k == 0 || n[k - 1] != '/').any(|k| matches(&p[1..], &n[k..])),
        '?' => !n.is_empty() && n[0] != '/' && matches(&p[1..], &n[1..]),
        '\\' => !n.is_empty() && p.len() > 1 && n[0] == p[1] && matches(&p[2..], &n[1..]),
        '[' => {
            if n.is_empty() || n[0] == '/' {
                return false;
            }
            let c = n[0];
            let mut i = 1;
            let neg = i < p.len() && p[i] == '^';
            if neg {
                i += 1;
            }
            let mut hit = false;
            let mut first = true;
            while i < p.len() && (p[i] != ']' || first) {
                first = false;
                let lo = class_char(p, &mut i).unwrap_or('\0');
                let hi = if i < p.len() && p[i] == '-' {
                    i += 1;
                    class_char(p, &mut i).unwrap_or('\0')
                } else {
                    lo
                };
                if lo <= c && c <= hi {
                    hit = true;
                }
            }
            hit != neg && matches(&p[(i + 1).min(p.len())..], &n[1..])
        }
        c => !n.is_empty() && n[0] == c && matches(&p[1..], &n[1..]),
    }
}
