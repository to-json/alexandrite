//! Parser error recovery: one error per broken top-level declaration.
use alx::diag::SourceMap;
use alx::front::{parse_file, parse_file_recovering};

fn errs(src: &str) -> (Vec<String>, Vec<u32>, bool) {
    let mut sm = SourceMap::default();
    let (m, es) = parse_file_recovering(&mut sm, "t.alx".into(), None, src.to_string(), &mut 0);
    let lines = es.iter().map(|d| sm.files[0].line_col(d.span.lo).0).collect();
    (es.into_iter().map(|d| d.msg).collect(), lines, m.is_some())
}

fn first(src: &str) -> Option<String> {
    let mut sm = SourceMap::default();
    parse_file(&mut sm, "t.alx".into(), src.to_string(), &mut 0).err().map(|d| d.msg)
}

#[test]
fn one_error_per_broken_declaration() {
    let src = "def a { 1 }\n\
               def b( { \n}\n\
               struct S {\n  x: Int\n}\n\
               def c -> { 1 }\n\
               pub def d { 2 }\n\
               struct T { x: }\n\
               def ok -> Int { 3 }\n";
    let (e, lines, _) = errs(src);
    assert_eq!(e.len(), 3, "{e:?}");
    assert_eq!(lines, vec![2, 7, 9], "{e:?}");
    assert_eq!(Some(e[0].clone()), first(src));
}

#[test]
fn braces_in_a_broken_body_do_not_end_recovery_early() {
    let src = "def a {\n  if x {\n    y = \n  }\n  def inner { }\n}\ndef b { ) }\nstruct S { x: Int }\n";
    let (e, lines, _) = errs(src);
    assert_eq!(e.len(), 2, "{e:?} {lines:?}");
    assert_eq!(Some(e[0].clone()), first(src));
}

#[test]
fn clean_file_and_lexer_failure() {
    let (e, _, some) = errs("def a { 1 }\nstruct S { x: Int }\n");
    assert!(e.is_empty() && some);
    let (e, _, some) = errs("def a { \"unterminated }\n");
    assert_eq!(e.len(), 1);
    assert!(!some);
}

#[test]
fn capped_at_fifty() {
    let src = "def a( {\n}\n".repeat(80);
    let (e, _, _) = errs(&src);
    assert_eq!(e.len(), 50);
}
