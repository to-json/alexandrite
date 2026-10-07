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
    c.args(["check", "--json", "--types", "--overlays", "-"]).arg(file).stdin(Stdio::piped()).stdout(Stdio::piped());
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
    assert!(j.starts_with("{\"unit\":\"script\"") && j.contains("\"diagnostics\":[],") && j.ends_with("}\n"), "{j}");
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

// ---------- M2.5: `types` and `members` ----------

/// The `types` entries named `name` of kind `kind`: (lo, hi, type).
fn bindings(j: &str, name: &str, kind: &str) -> Vec<(usize, usize, String)> {
    let start = j.find("\"types\":[").expect("types key") + 9;
    let end = start + j[start..].find("],\"members\"").expect("members key");
    let mut out = vec![];
    for e in j[start..end].split("},{") {
        let field = |k: &str| {
            let i = e.find(&format!("\"{k}\":"))? + k.len() + 3;
            let r = &e[i..];
            Some(if let Some(r) = r.strip_prefix('"') { r[..r.find('"')?].to_string() } else { r[..r.find(|c: char| !c.is_ascii_digit()).unwrap_or(r.len())].to_string() })
        };
        if field("name").as_deref() == Some(name) && field("kind").as_deref() == Some(kind) {
            out.push((field("lo").unwrap().parse().unwrap(), field("hi").unwrap().parse().unwrap(), field("type").unwrap()));
        }
    }
    out
}

/// The member list of `ty` (its JSON text).
fn members<'a>(j: &'a str, ty: &str) -> &'a str {
    let key = format!("{}:[", quote(ty));
    let i = j.find(&key).unwrap_or_else(|| panic!("no members for {ty}: {j}")) + key.len();
    &j[i..i + j[i..].find("}]").unwrap() + 1]
}

const TYPES_SRC: &str = r#"import "bufio"
import "strings"

struct Point {
  x: Int
  y: Int
  def sum -> Int { x + y }
  def move!(dx: Int) { self.x += dx }
}

def twice[T](v: T) -> [T] {
  out = [v, v]
  out
}

def go(p: Point, names: [Str]) -> ~Int {
  total = p.sum
  for n in names {
    total += n.size
  }
  names.each { |s| puts s.strip }
  m: Map[Str, Int] = {"a" => 1}
  if k = m["a"] { puts k }
  r = bufio.new_reader(strings.new_reader("hi\n"))
  line = r.~read_string!(10.as_u8) || ""
  q = twice(3)
  puts line.size + q.size + make(1).sum
  p.move!(2)
  total
}

def make(n: Int) -> Point { Point.new(x: n) }

def bad -> Int {
  broken = 1
  "nope"
}

res = go(Point.new(x: 1, y: 2), ["a"])
puts res.ok?
"#;

#[test]
fn types_locals_params_self_receivers() {
    let d = tmp("types");
    let f = d.join("t.alx");
    std::fs::write(&f, TYPES_SRC).unwrap();
    let (j, code) = run(&f, &[], &[]);
    assert_eq!(code, 0);
    // The failing def is reported, and the others' types are still there.
    assert_eq!(ndiag(&j), 1, "{j}");
    assert!(bindings(&j, "broken", "local").is_empty(), "a def that failed contributes nothing: {j}");
    let at = |name: &str, kind: &str| -> (usize, usize, String) {
        let v = bindings(&j, name, kind);
        assert_eq!(v.len(), 1, "{name} {kind}: {j}");
        v[0].clone()
    };
    let check = |name: &str, kind: &str, ty: &str, needle: &str| {
        let (lo, hi, t) = at(name, kind);
        assert_eq!(t, ty, "{name}");
        assert_eq!(&TYPES_SRC[lo..hi], name);
        // The span is the binding's own occurrence (`needle` starts there).
        assert!(TYPES_SRC[lo..].starts_with(needle), "{name} at {lo}: {:?}", &TYPES_SRC[lo..lo + 20]);
    };
    check("total", "local", "Int", "total = p.sum");
    check("p", "param", "Point", "p: Point");
    check("names", "param", "[Str]", "names: [Str]");
    check("n", "local", "Str", "n in names");
    check("s", "local", "Str", "s| puts");
    check("m", "local", "Map[Str, Int]", "m: Map");
    check("k", "local", "Int", "k = m");
    check("r", "local", "bufio.Reader", "r = bufio");
    check("line", "local", "Str", "line = r");
    check("dx", "param", "Int", "dx: Int");
    check("res", "local", "~Int", "res = go");
    // `self`: its span is the method.
    let selfs = bindings(&j, "self", "self");
    assert_eq!(selfs.len(), 2, "{j}");
    assert!(selfs.iter().all(|(_, _, t)| t == "Point"), "{j}");
    assert!(selfs.iter().any(|(lo, _, _)| TYPES_SRC[*lo..].starts_with("def move!")), "{j}");
    // Receivers: a local, a call's result, a `!` call's place, the `~` call's receiver.
    let recv = |text: &str| -> Vec<String> { bindings(&j, text, "recv").into_iter().map(|b| b.2).collect() };
    assert_eq!(recv("p"), vec!["Point", "Point"], "p.sum and p.move!: {j}");
    assert_eq!(recv("make(1)"), vec!["Point"], "{j}");
    assert_eq!(recv("r"), vec!["bufio.Reader"], "{j}");
    assert_eq!(recv("n"), vec!["Str"], "{j}");
    // A generic def used at one type: that instance's types.
    check("v", "param", "Int", "v: T");
    check("out", "local", "[Int]", "out = [v");
    // Members: a user struct (fields and methods), a std type (`pub` methods), builtins.
    let pt = members(&j, "Point");
    assert!(
        pt.contains("{\"name\":\"x\",\"kind\":\"field\",\"type\":\"Int\"}")
            && pt.contains("{\"name\":\"move!\",\"kind\":\"method\",\"sig\":\"(dx: Int)\"}")
            && pt.contains("{\"name\":\"sum\",\"kind\":\"method\",\"sig\":\"-> Int\"}"),
        "{pt}"
    );
    let br = members(&j, "bufio.Reader");
    assert!(br.contains("{\"name\":\"read_line!\",\"kind\":\"method\",\"sig\":\"-> ~Str?\"}") && br.contains("\"name\":\"buffered\""), "{br}");
    let st = members(&j, "Str");
    assert!(st.contains("\"name\":\"split\"") && st.contains("\"name\":\"start_with?\""), "{st}");
    assert!(members(&j, "[Str]").contains("\"name\":\"each\""), "{j}");
    assert!(members(&j, "Map[Str, Int]").contains("\"name\":\"keys\""), "{j}");
    assert!(members(&j, "~Int").contains("\"name\":\"ok?\""), "{j}");
}

#[test]
fn types_generic_at_two_types_is_dropped() {
    let d = tmp("types-gen");
    let f = d.join("g.alx");
    let src = "def id[T](v: T) -> T {\n  w = v\n  w\n}\n\nputs id(1)\nputs id(\"a\")\n";
    std::fs::write(&f, src).unwrap();
    let (j, _) = run(&f, &[], &[]);
    assert_eq!(ndiag(&j), 0, "{j}");
    assert!(bindings(&j, "v", "param").is_empty() && bindings(&j, "w", "local").is_empty(), "instances disagree: {j}");
}

#[test]
fn types_empty_on_parse_error_and_keys_present() {
    let d = tmp("types-parse");
    let f = d.join("p.alx");
    std::fs::write(&f, "def f( {\n").unwrap();
    let (j, _) = run(&f, &[], &[]);
    assert!(j.contains("\"types\":[],\"members\":{}"), "{j}");
}

#[test]
fn types_only_with_the_flag() {
    let d = tmp("types-flag");
    let f = d.join("a.alx");
    std::fs::write(&f, "x = 1\nputs x\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_alx")).args(["check", "--json"]).arg(&f).output().unwrap();
    let j = String::from_utf8(out.stdout).unwrap();
    assert!(j.contains("\"diagnostics\":[]") && !j.contains("\"types\""), "{j}");
}
