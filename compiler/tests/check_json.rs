//! `alx check --json`: overlays, std overlays, output shape.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("alx-check-json-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn quote(s: &str) -> String {
    alx::check_json::quote(s)
}

/// Run `alx check --json file` with overlays on stdin; (stdout, exit code).
fn run(file: &Path, overlays: &[(&Path, &str)], env: &[(&str, &Path)]) -> (String, i32) {
    let mut c = Command::new(env!("CARGO_BIN_EXE_alx"));
    c.args(["check", "--json", "--overlays", "-"]).arg(file).stdin(Stdio::piped()).stdout(Stdio::piped());
    for (k, v) in env {
        c.env(k, v);
    }
    let mut ch = c.spawn().unwrap();
    let body: Vec<String> = overlays.iter().map(|(p, t)| format!("{}:{}", quote(&p.to_string_lossy()), quote(t))).collect();
    ch.stdin.take().unwrap().write_all(format!("{{{}}}", body.join(",")).as_bytes()).unwrap();
    let out = ch.wait_with_output().unwrap();
    (String::from_utf8(out.stdout).unwrap(), out.status.code().unwrap())
}

fn ndiag(j: &str) -> usize {
    j.matches("\"phase\":").count()
}

#[test]
fn valid_and_error() {
    let d = tmp("basic");
    let ok = d.join("ok.alx");
    std::fs::write(&ok, "puts 1\n").unwrap();
    let (j, code) = run(&ok, &[], &[]);
    assert_eq!(code, 0);
    assert!(j.starts_with("{\"unit\":\"script\"") && j.ends_with("\"diagnostics\":[]}\n"), "{j}");
    let bad = d.join("bad.alx");
    std::fs::write(&bad, "x = 1 + \"a\"\n").unwrap();
    let (j, code) = run(&bad, &[], &[]);
    assert_eq!(code, 0);
    assert_eq!(ndiag(&j), 1, "{j}");
    assert!(j.contains("\"severity\":\"error\"") && j.contains("\"line\":0,\"col\":4") && j.contains(&quote(&bad.to_string_lossy())), "{j}");
}

#[test]
fn overlay_shadows_disk_but_never_adds() {
    let d = tmp("overlay");
    let f = d.join("a.alx");
    std::fs::write(&f, "puts 1\n").unwrap();
    let (j, _) = run(&f, &[(&f, "x = 1 + \"a\"\n")], &[]);
    assert_eq!(ndiag(&j), 1, "{j}");
    // The overlay fixes a broken file.
    std::fs::write(&f, "x = 1 + \"a\"\n").unwrap();
    let (j, _) = run(&f, &[(&f, "puts 1\n")], &[]);
    assert_eq!(ndiag(&j), 0, "{j}");
    // A file that is neither on disk nor the focused file is not created by an overlay.
    let h = d.join("ghost");
    std::fs::create_dir_all(&h).unwrap();
    let main = h.join("main.alx");
    std::fs::write(&main, "import \"ghostpkg\"\nputs 1\n").unwrap();
    let ghost = h.join("ghostpkg").join("g.alx");
    let (j, _) = run(&main, &[(&ghost, "pub def f -> Int { 1 }\n")], &[]);
    assert_eq!(ndiag(&j), 1, "{j}");
    // Unknown focused file: the editor's unsaved buffer, supplied by the overlay itself.
    let g = d.join("nope.alx");
    let (j, code) = run(&g, &[(&g, "puts 1\n")], &[]);
    assert_eq!(code, 0);
    assert_eq!(ndiag(&j), 0, "{j}");
    // No overlay and no file: cannot read.
    let (j, code) = run(&g, &[], &[]);
    assert_eq!(code, 0);
    assert!(j.contains("\"phase\":\"load\"") && j.contains("cannot read"), "{j}");
}

#[test]
fn overlay_on_imported_package() {
    let d = tmp("import");
    std::fs::create_dir_all(d.join("geom")).unwrap();
    let lib = d.join("geom/geom.alx");
    std::fs::write(&lib, "pub def one -> Int { 1 }\n").unwrap();
    let main = d.join("main.alx");
    std::fs::write(&main, "import \"geom\"\nputs geom.one\n").unwrap();
    let (j, _) = run(&main, &[], &[]);
    assert_eq!(ndiag(&j), 0, "{j}");
    let (j, _) = run(&main, &[(&lib, "pub def one -> Str { 1 }\n")], &[]);
    assert_eq!(ndiag(&j), 1, "{j}");
    assert!(j.contains(&quote(&lib.to_string_lossy())), "diagnostic is in the imported file: {j}");
}

#[test]
fn overlay_on_std_file_with_std_dir() {
    let d = tmp("std");
    let std_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../std").canonicalize().unwrap();
    let main = d.join("m.alx");
    std::fs::write(&main, "import \"strings\"\nputs strings.to_upper(\"a\")\n").unwrap();
    let (j, _) = run(&main, &[], &[("ALX_STD_DIR", &std_dir)]);
    assert_eq!(ndiag(&j), 0, "{j}");
    let target = std_dir.join("strings/strings.alx");
    assert!(target.exists());
    let mut text = std::fs::read_to_string(&target).unwrap();
    text.push_str("\npub def overlay_probe( {\n");
    let (j, _) = run(&main, &[(&target, &text)], &[("ALX_STD_DIR", &std_dir)]);
    assert_eq!(ndiag(&j), 1, "{j}");
    assert!(j.contains(&quote(&target.to_string_lossy())), "real std path: {j}");
}

#[test]
fn overlay_supplies_a_new_unsaved_focused_file() {
    let d = tmp("newbuf");
    let f = d.join("new.alx"); // never written to disk
    let (j, code) = run(&f, &[(&f, "x: Int = \"a\"\nputs x\n")], &[]);
    assert_eq!(code, 0, "{j}");
    assert_eq!(ndiag(&j), 1, "{j}");
    assert!(!j.contains("cannot read"), "{j}");
    let (j, _) = run(&f, &[(&f, "puts 1\n")], &[]);
    assert_eq!(ndiag(&j), 0, "{j}");
}
