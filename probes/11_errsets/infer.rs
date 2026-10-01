//! Probe 11: error-set inference, prototyped in Rust (the compiler's
//! language). Models src.alx as a call graph and checks:
//!   - `T!` sets are inferred as the union of `fail` sites and `try`'d callees
//!   - recursion is solved by fixpoint (sets only grow; tags are finite)
//!   - callers see the DECLARED set where there is one, the inferred set
//!     otherwise
//!   - explicit sets must cover the body; exported fns must be explicit
//!
//! `cargo run -p probe_errsets` prints the table; `cargo test -p probe_errsets`
//! checks it.

use std::collections::{BTreeMap, BTreeSet};

type Set = BTreeSet<&'static str>;

struct Fn {
    name: &'static str,
    /// `T!E`: the declared set. `None` for `T!` (inferred).
    declared: Option<Set>,
    exported: bool,
    /// Tags from `fail X` in the body, including those a `rescue` handler fails with.
    fails: Set,
    /// Callees under `try`: their sets flow into this one.
    tries: Vec<&'static str>,
    /// Callees under `rescue`: handled here, nothing flows.
    rescues: Vec<&'static str>,
}

fn set(tags: &[&'static str]) -> Set {
    tags.iter().copied().collect()
}

fn named(sets: &BTreeMap<&str, Set>, n: &str) -> Set {
    sets[n].clone()
}

#[derive(Debug, PartialEq)]
enum Verdict {
    Ok,
    MissingTags(Set),
    ExportedInferred,
}

struct Report {
    body: BTreeMap<&'static str, Set>,
    visible: BTreeMap<&'static str, Set>,
    verdicts: BTreeMap<&'static str, Verdict>,
    rounds: usize,
}

fn infer(fns: &[Fn]) -> Report {
    // What callers see: declared set if any, else the (growing) inferred set.
    let mut visible: BTreeMap<&str, Set> = fns
        .iter()
        .map(|f| (f.name, f.declared.clone().unwrap_or_default()))
        .collect();
    let mut body: BTreeMap<&str, Set> = BTreeMap::new();
    let mut rounds = 0;
    loop {
        rounds += 1;
        let mut changed = false;
        for f in fns {
            let mut b = f.fails.clone();
            for c in &f.tries {
                b.extend(visible[c].iter().copied());
            }
            if f.declared.is_none() && b != visible[f.name] {
                visible.insert(f.name, b.clone());
                changed = true;
            }
            body.insert(f.name, b);
        }
        if !changed {
            break;
        }
    }
    let verdicts = fns
        .iter()
        .map(|f| {
            let v = match (&f.declared, f.exported) {
                (None, true) => Verdict::ExportedInferred,
                (Some(d), _) => {
                    let missing: Set = body[f.name].difference(d).copied().collect();
                    if missing.is_empty() { Verdict::Ok } else { Verdict::MissingTags(missing) }
                }
                (None, false) => Verdict::Ok,
            };
            (f.name, v)
        })
        .collect();
    Report { body, visible, verdicts, rounds }
}

fn program() -> Vec<Fn> {
    let errors: BTreeMap<&str, Set> = BTreeMap::from([
        ("ParseError", set(&["BadDigit", "Empty"])),
        ("IoError", set(&["NotFound", "Denied"])),
        ("ConfigError", set(&["Missing", "Invalid"])),
    ]);
    let f = |name, declared: Option<&str>, exported, fails: &[&'static str], tries: &[&'static str], rescues: &[&'static str]| Fn {
        name,
        declared: declared.map(|d| named(&errors, d)),
        exported,
        fails: set(fails),
        tries: tries.to_vec(),
        rescues: rescues.to_vec(),
    };
    vec![
        f("digit", None, false, &["BadDigit"], &[], &[]),
        // `Overflow` is the builtin tag for unproven arithmetic (probe 06).
        f("parse", None, false, &["Empty", "Overflow"], &["digit"], &[]),
        f("read", Some("IoError"), false, &[], &[], &[]),
        f("load", None, false, &[], &["parse", "read"], &[]),
        // `eval` is listed before what it calls, and calls itself: needs rounds.
        f("eval", None, false, &["Overflow"], &["parse", "eval", "load"], &[]),
        f("config", Some("ConfigError"), true, &["Missing", "Invalid"], &[], &["load"]),
        f("leaky", None, true, &[], &["load"], &[]),
        f("narrow", Some("ParseError"), false, &[], &["parse"], &[]),
    ]
}

fn show(s: &Set) -> String {
    format!("{{{}}}", s.iter().copied().collect::<Vec<_>>().join(", "))
}

fn main() {
    let fns = program();
    let r = infer(&fns);
    println!("fixpoint rounds: {}\n", r.rounds);
    println!("| fn | declared | body produces | callers see | verdict |");
    println!("|---|---|---|---|---|");
    for f in &fns {
        let decl = f.declared.as_ref().map_or("inferred".into(), show);
        let v = match &r.verdicts[f.name] {
            Verdict::Ok => "ok".to_string(),
            Verdict::MissingTags(m) => format!("**error**: declared set lacks {}", show(m)),
            Verdict::ExportedInferred => "**error**: exported fn needs an explicit set".into(),
        };
        let resc = if f.rescues.is_empty() { String::new() } else { format!(" (rescues {})", f.rescues.join(", ")) };
        println!("| {}{} | {} | {} | {} | {} |", f.name, resc, decl, show(&r.body[f.name]), show(&r.visible[f.name]), v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inferred_sets() {
        let r = infer(&program());
        assert_eq!(r.visible["digit"], set(&["BadDigit"]));
        assert_eq!(r.visible["parse"], set(&["BadDigit", "Empty", "Overflow"]));
        assert_eq!(r.visible["load"], set(&["BadDigit", "Denied", "Empty", "NotFound", "Overflow"]));
        assert_eq!(r.visible["eval"], r.visible["load"]);
    }

    #[test]
    fn declared_set_is_the_boundary() {
        let r = infer(&program());
        assert_eq!(r.visible["config"], set(&["Invalid", "Missing"]));
        assert_eq!(r.verdicts["config"], Verdict::Ok);
    }

    #[test]
    fn rejections() {
        let r = infer(&program());
        assert_eq!(r.verdicts["leaky"], Verdict::ExportedInferred);
        assert_eq!(r.verdicts["narrow"], Verdict::MissingTags(set(&["Overflow"])));
    }

    #[test]
    fn fixpoint_terminates_on_mutual_recursion() {
        let fns = vec![
            Fn { name: "a", declared: None, exported: false, fails: set(&["A"]), tries: vec!["b"], rescues: vec![] },
            Fn { name: "b", declared: None, exported: false, fails: set(&["B"]), tries: vec!["a"], rescues: vec![] },
        ];
        let r = infer(&fns);
        assert_eq!(r.visible["a"], set(&["A", "B"]));
        assert_eq!(r.visible["b"], set(&["A", "B"]));
        assert!(r.rounds <= 3);
    }
}
