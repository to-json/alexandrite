//! Ruby's string conveniences as LIR functions (generated once per program,
//! from operations every backend has): `strip`, `lstrip`, `rstrip`,
//! `start_with?`, `end_with?`, `include?`, `lines`, `s * n`.

use crate::lir::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StrFn {
    Strip,
    Lstrip,
    Rstrip,
    StartWith,
    EndWith,
    Include,
    Lines,
    Repeat,
}

impl StrFn {
    pub fn name(self) -> &'static str {
        match self {
            StrFn::Strip => "__str_strip",
            StrFn::Lstrip => "__str_lstrip",
            StrFn::Rstrip => "__str_rstrip",
            StrFn::StartWith => "__str_start_with",
            StrFn::EndWith => "__str_end_with",
            StrFn::Include => "__str_include",
            StrFn::Lines => "__str_lines",
            StrFn::Repeat => "__str_repeat",
        }
    }
}

struct Fb {
    vars: Vec<LVar>,
    labels: usize,
}

impl Fb {
    fn var(&mut self, name: &str, ty: LTy) -> V {
        self.vars.push(LVar { name: name.into(), ty });
        self.vars.len() - 1
    }
    fn label(&mut self) -> Label {
        self.labels += 1;
        self.labels - 1
    }
}

fn b<T>(x: T) -> Box<T> {
    Box::new(x)
}
fn var(v: V) -> LE {
    LE::Var(v)
}
fn len(s: LE) -> LE {
    LE::Rt(Rt::StrLen, vec![s])
}
fn byte(s: LE, i: LE) -> LE {
    LE::Rt(Rt::StrByte, vec![s, i, LE::I(0)])
}
fn add(x: LE, y: LE) -> LE {
    LE::Arith(Op::Add, b(x), b(y), Ovf::Unchecked)
}
fn sub(x: LE, y: LE) -> LE {
    LE::Arith(Op::Sub, b(x), b(y), Ovf::Unchecked)
}
fn cmp(op: Op, x: LE, y: LE) -> LE {
    LE::Cmp(op, b(x), b(y), LTy::I64)
}
/// Ruby's whitespace: space, \t, \n, \v, \f, \r.
fn blank(c: LE) -> LE {
    // (Conditionals, not bitwise ops: Bools are i32 in wasm.)
    let sp = cmp(Op::Eq, c.clone(), LE::I(32));
    let ctl = LE::Cond(b(cmp(Op::Ge, c.clone(), LE::I(9))), b(cmp(Op::Le, c, LE::I(13))), b(LE::B(false)));
    LE::Cond(b(sp), b(LE::B(true)), b(ctl))
}
/// s[from, n] (a substring; `n` must not be the literal 0 for the backends'
/// byte/substring split, so it goes through a variable).
fn substr(fb: &mut Fb, out: &mut Vec<LS>, s: LE, from: LE, n: LE) -> LE {
    let nv = fb.var("n", LTy::I64);
    out.push(LS::Set(nv, n));
    LE::Rt(Rt::StrByte, vec![s, from, var(nv)])
}

/// Make sure `f` exists in `prog`; its name.
pub fn instantiate(prog: &mut LProgram, f: StrFn) -> &'static str {
    let name = f.name();
    if !prog.funcs.iter().any(|g| g.name == name) {
        let func = generate(f);
        prog.funcs.push(func);
    }
    name
}

fn generate(f: StrFn) -> LFunc {
    let mut fb = Fb { vars: vec![], labels: 0 };
    let mut body = vec![];
    let (params, ret) = match f {
        StrFn::Strip | StrFn::Lstrip | StrFn::Rstrip => {
            let s = fb.var("s", LTy::Str);
            let (lo, hi) = (fb.var("lo", LTy::I64), fb.var("hi", LTy::I64));
            body.push(LS::Set(lo, LE::I(0)));
            body.push(LS::Set(hi, len(var(s))));
            if f != StrFn::Rstrip {
                let l = fb.label();
                body.push(LS::Loop(
                    l,
                    vec![
                        LS::If(cmp(Op::Ge, var(lo), var(hi)), vec![LS::Break(l)], vec![]),
                        LS::If(LE::Not(b(blank(byte(var(s), var(lo))))), vec![LS::Break(l)], vec![]),
                        LS::Set(lo, add(var(lo), LE::I(1))),
                    ],
                ));
            }
            if f != StrFn::Lstrip {
                let l = fb.label();
                body.push(LS::Loop(
                    l,
                    vec![
                        LS::If(cmp(Op::Le, var(hi), var(lo)), vec![LS::Break(l)], vec![]),
                        LS::If(LE::Not(b(blank(byte(var(s), sub(var(hi), LE::I(1)))))), vec![LS::Break(l)], vec![]),
                        LS::Set(hi, sub(var(hi), LE::I(1))),
                    ],
                ));
            }
            let r = substr(&mut fb, &mut body, var(s), var(lo), sub(var(hi), var(lo)));
            body.push(LS::Return(Some(r)));
            (vec![s], LTy::Str)
        }
        StrFn::StartWith | StrFn::EndWith => {
            let s = fb.var("s", LTy::Str);
            let p = fb.var("p", LTy::Str);
            body.push(LS::If(cmp(Op::Gt, len(var(p)), len(var(s))), vec![LS::Return(Some(LE::B(false)))], vec![]));
            let from = if f == StrFn::StartWith { LE::I(0) } else { sub(len(var(s)), len(var(p))) };
            let part = substr(&mut fb, &mut body, var(s), from, len(var(p)));
            body.push(LS::Return(Some(LE::Cmp(Op::Eq, b(part), b(var(p)), LTy::Str))));
            (vec![s, p], LTy::Bool)
        }
        StrFn::Include => {
            let s = fb.var("s", LTy::Str);
            let p = fb.var("p", LTy::Str);
            let i = fb.var("i", LTy::I64);
            let last = fb.var("last", LTy::I64);
            body.push(LS::Set(i, LE::I(0)));
            body.push(LS::Set(last, sub(len(var(s)), len(var(p)))));
            let l = fb.label();
            let mut inner = vec![LS::If(cmp(Op::Gt, var(i), var(last)), vec![LS::Break(l)], vec![])];
            let part = substr(&mut fb, &mut inner, var(s), var(i), len(var(p)));
            inner.push(LS::If(LE::Cmp(Op::Eq, b(part), b(var(p)), LTy::Str), vec![LS::Return(Some(LE::B(true)))], vec![]));
            inner.push(LS::Set(i, add(var(i), LE::I(1))));
            body.push(LS::Loop(l, inner));
            body.push(LS::Return(Some(LE::B(false))));
            (vec![s, p], LTy::Bool)
        }
        StrFn::Lines => {
            // split("\n"), without the empty piece after a final newline.
            let s = fb.var("s", LTy::Str);
            let at = LTy::Arr(b(LTy::Str));
            let a = fb.var("a", at.clone());
            let n = fb.var("n", LTy::I64);
            body.push(LS::If(cmp(Op::Eq, len(var(s)), LE::I(0)), vec![LS::Return(Some(LE::ArrWithCap(LTy::Str, b(LE::I(0)))))], vec![]));
            body.push(LS::Set(a, LE::Rt(Rt::StrSplit, vec![var(s), LE::S("\n".into())])));
            body.push(LS::Set(n, LE::Len(b(var(a)))));
            let last = LE::Index { arr: b(var(a)), idx: b(sub(var(n), LE::I(1))), check: None };
            let empty = cmp(Op::Eq, len(last), LE::I(0));
            body.push(LS::If(empty, vec![LS::Set(a, LE::Slice(at, b(var(a)), b(LE::I(0)), b(sub(var(n), LE::I(1)))))], vec![]));
            body.push(LS::Return(Some(var(a))));
            (vec![s], LTy::Arr(b(LTy::Str)))
        }
        StrFn::Repeat => {
            let s = fb.var("s", LTy::Str);
            let n = fb.var("n", LTy::I64);
            let out = fb.var("out", LTy::Str);
            let i = fb.var("i", LTy::I64);
            body.push(LS::If(cmp(Op::Lt, var(n), LE::I(0)), vec![LS::Panic("negative argument to String#*".into(), "runtime".into())], vec![]));
            body.push(LS::Set(out, LE::S(String::new())));
            body.push(LS::Set(i, LE::I(0)));
            let l = fb.label();
            body.push(LS::Loop(
                l,
                vec![
                    LS::If(cmp(Op::Ge, var(i), var(n)), vec![LS::Break(l)], vec![]),
                    LS::Set(out, LE::Rt(Rt::StrCat, vec![var(out), var(s)])),
                    LS::Set(i, add(var(i), LE::I(1))),
                ],
            ));
            body.push(LS::Return(Some(var(out))));
            (vec![s, n], LTy::Str)
        }
    };
    LFunc { name: f.name().into(), params, vars: fb.vars, ret, body, external: false, is_main: false, labels: fb.labels }
}
