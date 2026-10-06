use alx::fmt::{format, norm_tokens};
use std::path::{Path, PathBuf};

fn alx_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut es: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    es.sort();
    for p in es {
        if p.is_dir() {
            alx_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "alx") {
            out.push(p);
        }
    }
}

fn cases() -> Vec<PathBuf> {
    let mut v = Vec::new();
    alx_files(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../acceptance/cases"), &mut v);
    assert!(v.len() > 20);
    v
}

/// Every acceptance case: formatting preserves tokens, is idempotent, and is
/// a fixed point (the cases are the style guide).
#[test]
fn acceptance_cases_are_fixed_points() {
    let mut bad = Vec::new();
    for p in cases() {
        let src = std::fs::read_to_string(&p).unwrap();
        let out = format(&src).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
        assert_eq!(norm_tokens(&src).unwrap(), norm_tokens(&out).unwrap(), "tokens changed: {}", p.display());
        assert_eq!(format(&out).unwrap(), out, "not idempotent: {}", p.display());
        if out != src {
            let diff: Vec<String> = src.lines().zip(out.lines()).filter(|(a, b)| a != b).map(|(a, b)| format!("  - {a}\n  + {b}")).collect();
            bad.push(format!("{}\n{}", p.display(), diff.join("\n")));
        }
    }
    assert!(bad.is_empty(), "not fixed points:\n{}", bad.join("\n"));
}

fn check(input: &str, expected: &str) {
    let out = format(input).unwrap();
    assert_eq!(out, expected, "input:\n{input}");
    assert_eq!(format(&out).unwrap(), out, "not idempotent");
    assert_eq!(norm_tokens(input).unwrap(), norm_tokens(&out).unwrap());
}

#[test]
fn messy_spacing() {
    check("x=1+2*3\nputs(x)   \n", "x = 1 + 2 * 3\nputs(x)\n");
    check("def f(a:Int,b : Int)->Int{a+b}\n", "def f(a: Int, b : Int) -> Int { a + b }\n");
    check("puts [1,2 ,3].map{|x|x*2}.sum\n", "puts [1, 2, 3].map { |x| x * 2 }.sum\n");
    check("a = -1\nb = a - -1\nc = x ? 1:2\n", "a = -1\nb = a - -1\nc = x ? 1 : 2\n");
    check("for i in 0 ... 10 { puts i }\n", "for i in 0...10 { puts i }\n");
    check("x = foo( 1, 2 )\ny = [ 1, 2 ]\nz = { a: 1 }\nw = {a:1}\n", "x = foo(1, 2)\ny = [1, 2]\nz = {a: 1}\nw = {a: 1}\n");
}

#[test]
fn indentation_and_blank_lines() {
    check(
        "\n\n  def f {\n\n      x = 1\n\n\n\n   if x > 0 {\n puts x   \n}\n\n  }\n\n\n",
        "def f {\n  x = 1\n\n  if x > 0 {\n    puts x\n  }\nend_marker\n"
            .replace("end_marker\n", "}\n")
            .as_str(),
    );
    check("a.each { |x|\nputs x\n}\n", "a.each { |x|\n  puts x\n}\n");
    check("f(1,\n2,\n3)\n", "f(1,\n  2,\n  3)\n");
    check("x = a\n.b\n.c\n", "x = a\n  .b\n  .c\n");
}

#[test]
fn comments() {
    check("x = 1      #note\n#another\n  # kept   \n", "x = 1 # note\n# another\n# kept\n");
    check("def f {\n# inside\n}\n", "def f {\n  # inside\n}\n");
    check("puts \"a  #{ 1+1 }  b\"   # c\n", "puts \"a  #{ 1+1 }  b\" # c\n");
}

#[test]
fn heredoc_and_unicode_survive() {
    let src = "d = <<~D.chars\n    keep   this  \n  # not a comment\nD\nƒ area(r: Float) -> Float { r*r }\n";
    check(src, "d = <<~D.chars\n    keep   this  \n  # not a comment\nD\nƒ area(r: Float) -> Float { r * r }\n");
}

#[test]
fn generics_and_unary() {
    check("def p(s: Str) -> ~Int<A|B> { 1 }\n", "def p(s: Str) -> ~Int<A | B> { 1 }\n");
    check("def q(s: Str) -> ~[Byte]<HexError> { 1 }\n", "def q(s: Str) -> ~[Byte]<HexError> { 1 }\n");
    check("def *(k: Int) -> V2 { 1 }\ndef <=>(o: V2) -> Int { 0 }\n", "def *(k: Int) -> V2 { 1 }\ndef <=>(o: V2) -> Int { 0 }\n");
    check("xs.map(&:to_i)\nputs -x\nputs !a\nf(*args)\n", "xs.map(&:to_i)\nputs -x\nputs !a\nf(*args)\n");
    check("case n {\n1|2=>\"a\"\n_=>\"b\"\n}\n", "case n {\n  1 | 2 => \"a\"\n  _ => \"b\"\n}\n");
}

#[test]
fn trailing_comments_align_in_runs() {
    check("a = 1 # x\nbbb = 2 # y\n\nc = 3    # z\n", "a = 1   # x\nbbb = 2 # y\n\nc = 3 # z\n");
    check("a = 1 # x\n# full\nbbb = 2 # y\n", "a = 1 # x\n# full\nbbb = 2 # y\n");
    check("def f {  # c\n  x = 1 # d\n}\n", "def f { # c\n  x = 1 # d\n}\n");
}

#[test]
fn heredoc_interpolation_is_kept() {
    let src = "x = 1\ns = <<~EOS\n    a #{x+1} \\#{x}\n      #{ [1].map { it } }\n  EOS\nputs s\n";
    check(src, src);
}

#[test]
fn leading_dot_chains_indent() {
    check(
        "ys = xs\n.map { it * 2 }\n# why\n.select { it > 2 }\nz = o\n?.to_s\n",
        "ys = xs\n  .map { it * 2 }\n  # why\n  .select { it > 2 }\nz = o\n  ?.to_s\n",
    );
    check("w =\n1 + 2\nw +=\n4\n", "w =\n  1 + 2\nw +=\n  4\n");
}

#[test]
fn trivial_files() {
    check("", "");
    check("\n\n", "");
    check("x = 1", "x = 1\n");
}
