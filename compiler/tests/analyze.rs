//! `analyze`: collect-mode checking. First-error equality with fail-fast mode on
//! every negative acceptance case; recovery, no cascades, roots.
use alx::analyze::{analyze, Force, Report};
use std::path::{Path, PathBuf};

fn read(p: &Path) -> std::io::Result<String> {
    std::fs::read_to_string(p)
}
fn list(p: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(p)?.filter_map(|e| e.ok().map(|e| e.path())).collect();
    v.sort();
    Ok(v)
}

fn errors(r: &Report) -> Vec<String> {
    r.items.iter().filter(|i| i.error).map(|i| format!("{}: {}", r.sm.loc(i.diag.span), i.diag.msg)).collect()
}

fn script(name: &str, src: &str) -> Report {
    let d = std::env::temp_dir().join(format!("alx-analyze-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let f = d.join("a.alx");
    std::fs::write(&f, src).unwrap();
    analyze(&f, Force::Script, &read, &list)
}

#[test]
fn first_error_equals_fail_fast() {
    let cases = Path::new(env!("CARGO_MANIFEST_DIR")).join("../acceptance/cases");
    let mut checked = 0;
    let mut bad = vec![];
    let mut files: Vec<PathBuf> = std::fs::read_dir(&cases).unwrap().filter_map(|e| e.ok().map(|e| e.path())).collect();
    files.sort();
    for exp in files.iter().filter(|p| p.extension().is_some_and(|e| e == "expected_error")) {
        let src = exp.with_extension("alx");
        if !src.exists() {
            continue;
        }
        let src = src.canonicalize().unwrap();
        let shown = src.to_string_lossy().to_string();
        let want = match alx::front::load(&src, &shown) {
            Err((sm, d)) => Some(format!("{}: {}", sm.loc(d.span), d.msg)),
            Ok(l) => match alx::front::check_program(&l, vec![]) {
                Err(d) => Some(format!("{}: {}", l.sm.loc(d.span), d.msg)),
                Ok(_) => None,
            },
        };
        let Some(want) = want else { continue }; // a runtime negative
        let r = analyze(&src, Force::Script, &read, &list);
        let got = errors(&r).into_iter().next();
        checked += 1;
        if got.as_deref() != Some(want.as_str()) {
            bad.push(format!("{}\n  fail-fast: {want}\n  analyze:   {got:?}", src.display()));
        }
    }
    assert!(checked > 40, "only {checked} cases compared");
    assert!(bad.is_empty(), "{} of {checked} differ:\n{}", bad.len(), bad.join("\n"));
}

#[test]
fn two_bad_defs_two_diagnostics() {
    let r = script("two", "def a(x: Int) -> Str { x }\ndef b(x: Int) -> Int { \"s\" }\ndef ok(x: Int) -> Int { x }\n");
    let e = errors(&r);
    assert_eq!(e.len(), 2, "{e:?}");
    assert!(e[0].contains("`a` returns Str"), "{e:?}");
    assert!(e[1].contains("`b` returns Int"), "{e:?}");
}

#[test]
fn caller_of_a_failed_def_does_not_cascade() {
    // `f` has no declared return type: the old behaviour reported "`f` is recursive".
    let r = script("cascade", "def f(x: Int) { x.nope }\ndef g(x: Int) -> Int { f(x); 1 }\ndef h -> Int { g(2) }\nputs h\n");
    let e = errors(&r);
    assert_eq!(e.len(), 1, "{e:?}");
    assert!(!e[0].contains("recursive"), "{e:?}");
}

#[test]
fn generic_owner_methods_and_generic_defs_are_not_roots() {
    let r = script("generic", "struct Box[T] {\n  v: T\n  def get -> T { v + 1 }\n}\ndef id[T](x: T) -> T { x + \"a\" }\ndef untyped(x) { x.nope }\ndef fine(x: Int) -> Int { x }\n");
    assert!(errors(&r).is_empty(), "{:?}", errors(&r));
}

#[test]
fn declaration_error_stops_bodies() {
    let r = script("decl", "struct S { a: Nope }\ndef f(x: Int) -> Str { x }\n");
    let e = errors(&r);
    assert_eq!(e.len(), 1, "{e:?}");
}

#[test]
fn warnings_are_not_errors() {
    let r = script("warn", "import \"strings\"\ndef f(x: Int) -> Int { x }\n");
    assert!(errors(&r).is_empty());
    assert!(r.items.iter().any(|i| !i.error && i.phase == alx::analyze::Phase::Warning));
}

/// Plan 1.4's exit criterion: check-all over every std package reports nothing.
/// (Directories that hold only external tests have no package of their own.)
#[test]
fn std_packages_report_no_diagnostics() {
    fn walk(d: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        let mut subs: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
        subs.sort();
        if subs.iter().any(|p| p.extension().is_some_and(|e| e == "alx") && !p.to_string_lossy().ends_with("_test.alx")) {
            out.push(d.to_path_buf());
        }
        for p in subs.into_iter().filter(|p| p.is_dir() && p.file_name().is_some_and(|n| n != "testdata")) {
            walk(&p, out);
        }
    }
    let std_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("std");
    let mut dirs = vec![];
    walk(&std_dir, &mut dirs);
    assert!(dirs.len() > 100, "found only {} std packages", dirs.len());
    let mut bad = vec![];
    for d in &dirs {
        // A package's own files: any one non-test file selects the package unit.
        let Some(file) = std::fs::read_dir(d).unwrap().filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|e| e == "alx") && !p.to_string_lossy().ends_with("_test.alx")).min() else { continue };
        let r = analyze(&file, Force::Package, &read, &list);
        for i in &r.items {
            bad.push(format!("{}: {}: {}", d.display(), r.sm.loc(i.diag.span), i.diag.msg));
        }
    }
    assert!(bad.is_empty(), "{} diagnostics:\n{}", bad.len(), bad.join("\n"));
}
