//! Editor analysis: check a file's unit and collect diagnostics instead of
//! stopping at the first. The contract between the checker (this module's
//! `analyze`) and the CLI (`alx check --json`, driver.rs). docs/notes/lsp-plan.md.

use crate::diag::{Diag, SourceMap};
use std::path::{Path, PathBuf};

/// Which compilation phase produced a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Load,
    Parse,
    Check,
    Prove,
    Warning,
    Internal,
}

impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Phase::Load => "load",
            Phase::Parse => "parse",
            Phase::Check => "check",
            Phase::Prove => "prove",
            Phase::Warning => "warning",
            Phase::Internal => "internal",
        }
    }
}

/// One reported problem. `error` is false for warnings.
pub struct Item {
    pub diag: Diag,
    pub phase: Phase,
    pub error: bool,
}

/// What to check a file as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Force {
    /// A test file, or a file in a directory with no top-level statements, is a package; else a script.
    Auto,
    Script,
    Package,
}

pub struct Report {
    /// "script" or "package".
    pub unit: &'static str,
    /// The file (script) or directory (package) that was checked.
    pub root: PathBuf,
    /// In emission order: the first error is the one fail-fast mode reports.
    pub items: Vec<Item>,
    /// Resolves every `Diag`'s span (`Span::default()` has no file when the load itself failed).
    pub sm: SourceMap,
    /// The focused file's bindings and receivers with their types, and the members
    /// of the types they name (typemap.rs). Empty when nothing was checked.
    pub types: crate::typemap::TypeMap,
}

pub type ReadFn<'a> = &'a dyn Fn(&Path) -> std::io::Result<String>;
pub type ListFn<'a> = &'a dyn Fn(&Path) -> std::io::Result<Vec<PathBuf>>;

/// Does `text` (a non-test file of a directory) have top-level statements?
/// An unparsable file counts as having none.
fn has_statements(name: &str, text: String) -> bool {
    let mut sm = SourceMap::default();
    let mut next_id = 0;
    match crate::front::parse_file(&mut sm, name.to_string(), text, &mut next_id) {
        Ok(m) => m.main.iter().any(|s| !matches!(s.kind, crate::ast::StmtKind::Using(_))),
        Err(_) => false,
    }
}

/// Auto: a `*_test.alx` file, or a directory without a file that has top-level
/// statements, is a package; anything else is a script.
fn choose(file: &Path, force: Force, read: ReadFn, list: ListFn) -> bool {
    match force {
        Force::Script => return false,
        Force::Package => return true,
        Force::Auto => {}
    }
    if file.to_string_lossy().ends_with("_test.alx") {
        return true;
    }
    let dir = file.parent().unwrap_or(Path::new("."));
    let Ok(files) = list(dir) else { return false };
    for f in files.iter().filter(|f| f.extension().is_some_and(|e| e == "alx") && !f.to_string_lossy().ends_with("_test.alx")) {
        // The focused file's text is the buffer's, whatever the list says.
        let text = read(f);
        if let Ok(t) = text {
            if has_statements(&f.to_string_lossy(), t) {
                return false;
            }
        }
    }
    // A new file the list doesn't know yet.
    if !files.iter().any(|f| f == file) {
        if let Ok(t) = read(file) {
            return !has_statements(&file.to_string_lossy(), t);
        }
    }
    true
}

/// Syntax errors of `files`, one per broken top-level declaration (several files
/// are reported together). Empty when all parse.
fn parse_errors(files: &[PathBuf], read: ReadFn) -> (SourceMap, Vec<Item>) {
    let mut sm = SourceMap::default();
    let mut next_id = 0;
    let mut items = vec![];
    for f in files {
        let Ok(text) = read(f) else { continue };
        let (_, errs) = crate::front::parse_file_recovering(&mut sm, f.to_string_lossy().to_string(), Some(f.clone()), text, &mut next_id);
        items.extend(errs.into_iter().map(|diag| Item { diag, phase: Phase::Parse, error: true }));
    }
    (sm, items)
}

/// Check the unit `file` belongs to. `read`/`list` see unsaved buffers.
///
/// Every def of the unit's own files that stands alone is checked, called or not,
/// and one failure does not hide the next (collect mode, check.rs). The first
/// error is the one `alx run` would report.
pub fn analyze(file: &Path, force: Force, read: ReadFn, list: ListFn) -> Report {
    let package = choose(file, force, read, list);
    let dir = file.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")).to_path_buf();
    let unit = if package { "package" } else { "script" };
    let root = if package { dir.clone() } else { file.to_path_buf() };
    // The unit's own files, for syntax errors first.
    let own_files: Vec<PathBuf> = if package {
        let mut v: Vec<PathBuf> = list(&dir).unwrap_or_default().into_iter().filter(|f| f.extension().is_some_and(|e| e == "alx") && !f.to_string_lossy().ends_with("_test.alx")).collect();
        if file.to_string_lossy().ends_with("_test.alx") {
            v.push(file.to_path_buf());
        }
        v
    } else {
        vec![file.to_path_buf()]
    };
    let (psm, perrs) = parse_errors(&own_files, read);
    if !perrs.is_empty() {
        return Report { unit, root, items: perrs, sm: psm, types: Default::default() };
    }
    let mut items = vec![];
    let loaded = if package {
        let shown = dir.to_string_lossy().to_string();
        let sel = if file.to_string_lossy().ends_with("_test.alx") { crate::front::Tests::Only(file) } else { crate::front::Tests::NoTests };
        let o = crate::front::TestOpts { run: None, bench: None, bench_ns: 0, short: false };
        crate::front::load_tests_with(&dir, &shown, sel, &o, read, list).map(|(l, _, _)| l)
    } else {
        crate::front::load_with(file, &file.to_string_lossy(), read, list)
    };
    let l = match loaded {
        Ok(l) => l,
        Err((sm, d)) => return Report { unit, root, items: vec![Item { diag: d, phase: Phase::Load, error: true }], sm, types: Default::default() },
    };
    let mut types = crate::typemap::TypeMap::default();
    {
        let own = |f: u32| match l.sm.files.get(f as usize).and_then(|sf| sf.path.as_deref()) {
            Some(p) if package => p.parent() == Some(dir.as_path()),
            Some(p) => p == file,
            None => false,
        };
        let focused = l.sm.files.iter().position(|sf| sf.path.as_deref() == Some(file)).map(|i| i as u32);
        crate::front::check_collect_with(&l, &own, &mut items, &mut |w| {
            if let Some(f) = focused {
                types = crate::typemap::build(w, f);
            }
        });
    }
    Report { unit, root, items, sm: l.sm, types }
}
