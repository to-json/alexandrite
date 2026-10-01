//! Probe 13a: inferring trait bounds for unannotated (generic) parameters.
//!
//! A parameter without a type is generic. Each method used on it becomes a
//! requirement. A requirement resolves to a trait when exactly one trait
//! provides that method; then the bound is nominal and can be written into
//! a header. Inherent-only or ambiguous requirements stay structural: fine
//! inside a module (checked per call), an error on export.
//!
//! `cargo run -p probe_generics` prints headers and diagnostics;
//! `cargo test -p probe_generics` checks them.

use std::collections::{BTreeMap, BTreeSet};

struct World {
    /// trait -> methods it provides
    traits: BTreeMap<&'static str, Vec<&'static str>>,
    /// type -> traits it implements (nominal)
    impls: BTreeMap<&'static str, Vec<&'static str>>,
    /// type -> inherent methods
    inherent: BTreeMap<&'static str, Vec<&'static str>>,
}

struct Func {
    name: &'static str,
    exported: bool,
    /// methods called on the generic param, with the source line
    uses: Vec<(&'static str, u32)>,
}

#[derive(Debug, PartialEq, Clone)]
enum Req {
    /// Exactly one trait provides the method.
    Trait(&'static str),
    /// Only some type's inherent method: structural.
    Inherent(&'static str),
    /// Several traits provide it: structural.
    Ambiguous(&'static str, Vec<&'static str>),
}

struct Inferred {
    name: &'static str,
    /// Nominal bound: the union of resolved traits.
    bound: BTreeSet<&'static str>,
    /// Where each trait in the bound came from (for call-site errors).
    because: BTreeMap<&'static str, (&'static str, u32)>,
    structural: Vec<(Req, u32)>,
}

fn resolve(w: &World, method: &'static str) -> Req {
    let providers: Vec<_> = w.traits.iter().filter(|(_, ms)| ms.contains(&method)).map(|(t, _)| *t).collect();
    match providers.len() {
        1 => Req::Trait(providers[0]),
        0 => Req::Inherent(method),
        _ => Req::Ambiguous(method, providers),
    }
}

fn infer(w: &World, f: &Func) -> Inferred {
    let mut out = Inferred { name: f.name, bound: BTreeSet::new(), because: BTreeMap::new(), structural: vec![] };
    for &(m, line) in &f.uses {
        match resolve(w, m) {
            Req::Trait(t) => {
                out.bound.insert(t);
                out.because.entry(t).or_insert((m, line));
            }
            r => out.structural.push((r, line)),
        }
    }
    out
}

/// The generated header line, or the export error.
fn header(f: &Func, inf: &Inferred) -> Result<String, String> {
    if let Some((r, line)) = inf.structural.first() {
        let why = match r {
            Req::Inherent(m) => format!("`x.{m}` (line {line}) is an inherent method, not provided by any trait"),
            Req::Ambiguous(m, ts) => format!("`x.{m}` (line {line}) is provided by several traits: {}", ts.join(", ")),
            Req::Trait(_) => unreachable!(),
        };
        return if f.exported {
            Err(format!("cannot export `{}`: {why}; name a trait bound or a concrete type", f.name))
        } else {
            Ok(format!("def {}(x)  # internal: structural requirement, checked per call", f.name))
        };
    }
    let b: Vec<_> = inf.bound.iter().copied().collect();
    let kw = if f.exported { "pub def" } else { "def" };
    Ok(format!("{kw} {}[T: {}](x: T)", f.name, b.join(" + ")))
}

/// Check a call `f(value of type ty)` at `line`.
fn check_call(w: &World, inf: &Inferred, ty: &str, line: u32) -> Result<(), String> {
    let impls = w.impls.get(ty).cloned().unwrap_or_default();
    for t in &inf.bound {
        if !impls.contains(t) {
            let (m, l) = inf.because[t];
            return Err(format!(
                "line {line}: `{ty}` doesn't implement `{t}`, which `{}` requires (inferred from `x.{m}` on line {l})",
                inf.name
            ));
        }
    }
    for (r, l) in &inf.structural {
        let m = match r {
            Req::Inherent(m) | Req::Ambiguous(m, _) => *m,
            Req::Trait(_) => unreachable!(),
        };
        let has_inherent = w.inherent.get(ty).is_some_and(|ms| ms.contains(&m));
        let via_trait = impls.iter().any(|t| w.traits[t].contains(&m));
        if !has_inherent && !via_trait {
            return Err(format!("line {line}: `{ty}` has no method `{m}`, which `{}` calls on line {l}", inf.name));
        }
    }
    Ok(())
}

fn world() -> World {
    World {
        traits: BTreeMap::from([
            ("Sum", vec!["sum"]),
            ("Shape", vec!["area", "name"]),
            ("Show", vec!["to_s"]),
            ("Sized2", vec!["size"]),
            ("Collection", vec!["size"]),
        ]),
        impls: BTreeMap::from([("Circle", vec!["Shape", "Show"]), ("Square", vec!["Show"]), ("Ints", vec!["Sum"])]),
        inherent: BTreeMap::from([("Circle", vec!["radius"])]),
    }
}

fn funcs() -> Vec<Func> {
    vec![
        Func { name: "total", exported: true, uses: vec![("sum", 15)] },
        Func { name: "describe", exported: true, uses: vec![("name", 16), ("area", 16)] },
        Func { name: "show_all", exported: true, uses: vec![("to_s", 17), ("area", 17)] },
        Func { name: "radius_of", exported: false, uses: vec![("radius", 18)] },
        Func { name: "radius_pub", exported: true, uses: vec![("radius", 19)] },
        Func { name: "amb", exported: true, uses: vec![("size", 20)] },
    ]
}

const CALLS: [(&str, &str, u32); 5] = [
    ("describe", "Circle", 23),
    ("describe", "Square", 24),
    ("radius_of", "Circle", 25),
    ("radius_of", "Square", 26),
    ("show_all", "Circle", 27),
];

fn main() {
    let w = world();
    let fs = funcs();
    let infs: BTreeMap<_, _> = fs.iter().map(|f| (f.name, infer(&w, f))).collect();
    println!("### generated headers\n");
    for f in &fs {
        match header(f, &infs[f.name]) {
            Ok(h) => println!("  {h}"),
            Err(e) => println!("  error: {e}"),
        }
    }
    println!("\n### call sites\n");
    for (f, ty, line) in CALLS {
        match check_call(&w, &infs[f], ty, line) {
            Ok(()) => println!("  ok    {f}({ty}) at line {line}"),
            Err(e) => println!("  error {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(name: &str) -> Result<String, String> {
        let w = world();
        let f = funcs().into_iter().find(|f| f.name == name).unwrap();
        header(&f, &infer(&w, &f))
    }

    #[test]
    fn nominal_bounds_are_inferred() {
        assert_eq!(hdr("total").unwrap(), "pub def total[T: Sum](x: T)");
        assert_eq!(hdr("describe").unwrap(), "pub def describe[T: Shape](x: T)");
        assert_eq!(hdr("show_all").unwrap(), "pub def show_all[T: Shape + Show](x: T)");
    }

    #[test]
    fn structural_requirements_block_export_only() {
        assert!(hdr("radius_of").unwrap().contains("internal"));
        assert!(hdr("radius_pub").unwrap_err().contains("inherent"));
        assert!(hdr("amb").unwrap_err().contains("Collection, Sized2"));
    }

    #[test]
    fn call_errors_name_the_inferred_bound() {
        let w = world();
        let f = &funcs()[1];
        let e = check_call(&w, &infer(&w, f), "Square", 24).unwrap_err();
        assert!(e.contains("doesn't implement `Shape`") && e.contains("`x.name` on line 16"), "{e}");
        let r = &funcs()[3];
        assert!(check_call(&w, &infer(&w, r), "Square", 26).unwrap_err().contains("no method `radius`"));
    }
}
