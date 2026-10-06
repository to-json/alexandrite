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
}

pub type ReadFn<'a> = &'a dyn Fn(&Path) -> std::io::Result<String>;
pub type ListFn<'a> = &'a dyn Fn(&Path) -> std::io::Result<Vec<PathBuf>>;

/// Check the unit `file` belongs to. `read`/`list` see unsaved buffers.
///
/// STUB (fail-fast): checks `file` as a script and reports at most one error.
/// The collecting implementation (unit selection, roots, recovery) replaces this body.
pub fn analyze(file: &Path, _force: Force, read: ReadFn, list: ListFn) -> Report {
    let display = file.to_string_lossy().to_string();
    let mk = |sm: SourceMap, items: Vec<Item>| Report { unit: "script", root: file.to_path_buf(), items, sm };
    let l = match crate::front::load_with(file, &display, read, list) {
        Ok(l) => l,
        Err((sm, d)) => return mk(sm, vec![Item { diag: d, phase: Phase::Load, error: true }]),
    };
    let externs = crate::front::lib_defs(&l);
    match crate::front::check_program(&l, externs) {
        Ok(p) => {
            let items = p.warnings.iter().cloned().map(|d| Item { diag: d, phase: Phase::Warning, error: false }).collect();
            mk(l.sm, items)
        }
        Err(d) => mk(l.sm, vec![Item { diag: d, phase: Phase::Check, error: true }]),
    }
}
