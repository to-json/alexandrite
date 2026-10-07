//! `alx parse --decls`: the declaration list the language server's outline
//! scanner is tested against (acceptance group LSP).
use std::process::Command;

fn decls(name: &str, src: &str) -> String {
    let d = std::env::temp_dir().join(format!("alx-parse-decls-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    let f = d.join("t.alx");
    std::fs::write(&f, src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_alx")).args(["parse", "--decls"]).arg(&f).output().unwrap();
    let _ = std::fs::remove_dir_all(&d);
    assert!(out.status.success());
    let s = String::from_utf8(out.stdout).unwrap();
    let (head, rest) = s.split_once('\n').unwrap();
    assert!(head.starts_with("== "), "{s}");
    format!("{}{rest}", if head.ends_with(" error") { "error\n" } else { "" })
}

#[test]
fn kinds_names_owners_lines() {
    let src = "import fp \"path/filepath\"\n\
               pub MAX = 1\n\
               #[derive(Json)]\n\
               struct P {\n  x: Int\n  def self.origin -> P { P.new() }\n  def [](i: Int) -> Int { i }\n}\n\
               interface Shape { def area -> Float }\n\
               refine W for Str {\n  def words -> Int { 1 }\n}\n\
               extern def c_getpid() -> I32 = \"getpid\"\n\
               def f { }\n";
    let got = decls("kinds", src);
    let rows: Vec<&str> = got.lines().map(|l| l.rsplit_once('\t').unwrap().0).collect();
    assert_eq!(
        rows,
        [
            "import\tpath/filepath\tfp\t1",
            "const\tMAX\t\t2",
            "struct\tP\t\t4",
            "method\torigin\tP\t6",
            "method\t[]\tP\t7",
            "interface\tShape\t\t9",
            "method\tarea\tShape\t9",
            "refine\tW\t\t10",
            "method\twords\tW\t11",
            "extern\tc_getpid\t\t13",
            "def\tf\t\t14",
        ],
        "{got}"
    );
    // offsets are the names' (derives and the hidden import of a derive add none)
    let lo: usize = got.lines().nth(2).unwrap().rsplit_once('\t').unwrap().1.parse().unwrap();
    assert_eq!(&src[lo..lo + 1], "P");
}

#[test]
fn broken_files_are_marked_and_listed() {
    let got = decls("broken", "def a { 1 }\ndef b( { \n}\ndef c { }\n");
    assert!(got.starts_with("error\n"), "{got}");
    assert!(got.contains("def\tc\t\t4\t"), "{got}");
}
