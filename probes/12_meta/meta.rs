//! Probe 12: scheduling for lazy, type-aware macros (DESIGN option B).
//!
//! Macros here are Rust closures standing in for pure Alexandrite functions
//! run by the compiler's IR interpreter. What's under test is the ORDER of
//! evaluation, interleaved with "type checking" (member lookups):
//!   - a type's member table is completed on first lookup (lazy, per type)
//!   - macros may look up other types' members (nested expansion)
//!   - dependency loops are reported, not hung on
//!   - compile-time method_missing generates methods on demand, cached
//!   - an incremental cache keyed on recorded dependencies: editing a
//!     declared input reruns only the macros that (transitively) read it
//!   - undeclared inputs are rejected
//!
//! `cargo run -p probe_meta --release` prints three builds and a scaling
//! table; `cargo test -p probe_meta` checks the scenario.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::time::Instant;

type TyId = usize;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct Field {
    name: String,
    ty: String,
    rename: Option<String>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct Method {
    name: String,
    sig: String,
    /// Which macro generated it (for error chains).
    from: String,
}

#[derive(Clone, Hash)]
enum Derive {
    Eq,
    Show,
    Json,
    Mirror(String),
}

impl Derive {
    fn name(&self) -> String {
        match self {
            Derive::Mirror(t) => format!("Mirror({t})"),
            d => format!("{d:?}"),
        }
    }
}

impl std::fmt::Debug for Derive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Derive::Eq => "Eq",
            Derive::Show => "Show",
            Derive::Json => "Json",
            Derive::Mirror(_) => "Mirror",
        })
    }
}

#[derive(Clone, Hash)]
struct TypeDecl {
    name: String,
    fields: Vec<Field>,
    derives: Vec<Derive>,
    /// `model X from "path"`: fields come from a declared input.
    schema: Option<String>,
    /// `with finders`: compile-time method_missing for find_by_<field>.
    finders: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum MetaError {
    Loop(Vec<String>),
    NoMethod { ty: String, name: String },
    UndeclaredInput(String),
    Missing(String),
}

impl std::fmt::Display for MetaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MetaError::Loop(chain) => write!(f, "dependency loop: {}", chain.join(" -> ")),
            MetaError::NoMethod { ty, name } => write!(f, "no method `{name}` on `{ty}` (method_missing returned nil)"),
            MetaError::UndeclaredInput(p) => write!(f, "`{p}` is not a declared input (add it to the build manifest)"),
            MetaError::Missing(m) => write!(f, "{m}"),
        }
    }
}

/// What an expansion read, with the fingerprint it saw.
#[derive(Clone, Debug, PartialEq)]
enum Dep {
    Decl(TyId, u64),
    Members(TyId, u64),
    Input(String, u64),
}

#[derive(Clone)]
struct Entry {
    deps: Vec<Dep>,
    out: Result<Out, MetaError>,
}

#[derive(Clone, Debug, PartialEq)]
enum Out {
    Methods(Vec<Method>),
    Fields(Vec<Field>),
    Finder(Option<Method>),
}

/// Survives across builds (the on-disk incremental cache).
#[derive(Default)]
struct Cache {
    entries: HashMap<String, Entry>,
}

#[derive(Clone)]
struct Members {
    fields: Vec<Field>,
    methods: Vec<Method>,
    hash: u64,
}

enum State {
    Fresh,
    /// In progress; own fields are already known (macros on T may read them).
    Expanding(Vec<Field>),
    Done(Result<Members, MetaError>),
}

fn fp<T: Hash>(x: &T) -> u64 {
    let mut h = DefaultHasher::new();
    x.hash(&mut h);
    h.finish()
}

struct Ctx<'a> {
    decls: &'a [TypeDecl],
    by_name: HashMap<&'a str, TyId>,
    /// Declared inputs only (the build manifest), with current contents.
    inputs: &'a HashMap<String, String>,
    cache: &'a mut Cache,
    state: Vec<State>,
    /// Expansion stack, for loop reporting.
    stack: Vec<String>,
    /// One dependency recorder per running macro.
    recorders: Vec<Vec<Dep>>,
    /// Macro runs this build (cache misses).
    ran: Vec<String>,
    reused: usize,
}

impl<'a> Ctx<'a> {
    fn new(decls: &'a [TypeDecl], inputs: &'a HashMap<String, String>, cache: &'a mut Cache) -> Self {
        Ctx {
            decls,
            by_name: decls.iter().enumerate().map(|(i, d)| (d.name.as_str(), i)).collect(),
            inputs,
            cache,
            state: decls.iter().map(|_| State::Fresh).collect(),
            stack: Vec::new(),
            recorders: Vec::new(),
            ran: Vec::new(),
            reused: 0,
        }
    }

    fn record(&mut self, d: Dep) {
        if let Some(r) = self.recorders.last_mut() {
            if !r.contains(&d) {
                r.push(d);
            }
        }
    }

    fn id(&self, name: &str) -> Result<TyId, MetaError> {
        self.by_name.get(name).copied().ok_or_else(|| MetaError::Missing(format!("unknown type `{name}`")))
    }

    // ---------- the API macros see ----------

    fn own_fields(&mut self, t: TyId) -> Vec<Field> {
        self.record(Dep::Decl(t, fp(&self.decls[t])));
        match &self.state[t] {
            State::Expanding(f) => f.clone(),
            State::Done(Ok(m)) => m.fields.clone(),
            _ => self.decls[t].fields.clone(),
        }
    }

    fn members(&mut self, t: TyId) -> Result<Members, MetaError> {
        let m = self.expand(t)?;
        self.record(Dep::Members(t, m.hash));
        Ok(m)
    }

    fn read_input(&mut self, path: &str) -> Result<String, MetaError> {
        let text = self.inputs.get(path).ok_or_else(|| MetaError::UndeclaredInput(path.into()))?.clone();
        self.record(Dep::Input(path.into(), fp(&text)));
        Ok(text)
    }

    // ---------- scheduling ----------

    /// Complete T's member table: schema fields, then every derive.
    fn expand(&mut self, t: TyId) -> Result<Members, MetaError> {
        match &self.state[t] {
            State::Done(r) => return r.clone(),
            State::Expanding(_) => {
                let mut chain = self.stack.clone();
                chain.push(self.decls[t].name.clone());
                return Err(MetaError::Loop(chain));
            }
            State::Fresh => {}
        }
        let decl = &self.decls[t];
        let r = (|| {
            let mut fields = decl.fields.clone();
            if let Some(path) = &decl.schema {
                let path = path.clone();
                match self.cached(&format!("schema:{}", decl.name), |cx| schema(cx, &path).map(Out::Fields))? {
                    Out::Fields(f) => fields = f,
                    _ => unreachable!(),
                }
            }
            self.state[t] = State::Expanding(fields.clone());
            let mut methods = Vec::new();
            for d in decl.derives.clone() {
                self.stack.push(format!("{} derive({})", decl.name, d.name()));
                let key = format!("derive:{}:{}", d.name(), decl.name);
                let out = self.cached(&key, |cx| run_derive(cx, t, &d).map(Out::Methods));
                self.stack.pop();
                match out? {
                    Out::Methods(m) => methods.extend(m),
                    _ => unreachable!(),
                }
            }
            let hash = fp(&(&fields, &methods));
            Ok(Members { fields, methods, hash })
        })();
        self.state[t] = State::Done(r.clone());
        r
    }

    /// Run `f` unless a cached result's recorded dependencies all still hold.
    fn cached(&mut self, key: &str, f: impl FnOnce(&mut Self) -> Result<Out, MetaError>) -> Result<Out, MetaError> {
        if let Some(e) = self.cache.entries.get(key).cloned() {
            if self.still_valid(&e.deps) {
                self.reused += 1;
                return e.out;
            }
        }
        self.recorders.push(Vec::new());
        let out = f(self);
        let deps = self.recorders.pop().unwrap();
        // A loop error depends on the expansion stack, not just on deps:
        // never cache it.
        // Deps stay DIRECT: a nested expansion's own deps are not copied up.
        // The parent recorded that type's member-table fingerprint instead,
        // which changes iff anything beneath it changed (early cutoff).
        if !matches!(out, Err(MetaError::Loop(_))) {
            self.cache.entries.insert(key.to_owned(), Entry { deps, out: out.clone() });
        }
        self.ran.push(key.to_owned());
        out
    }

    fn still_valid(&mut self, deps: &[Dep]) -> bool {
        deps.iter().all(|d| match d {
            Dep::Decl(t, h) => fp(&self.decls[*t]) == *h,
            Dep::Input(p, h) => self.inputs.get(p).map(fp) == Some(*h),
            // Re-checking another type's members may expand it (lazily,
            // itself mostly from cache).
            Dep::Members(t, h) => matches!(self.expand(*t), Ok(m) if m.hash == *h),
        })
    }

    /// What the type checker calls for `x.name` where x: T.
    fn lookup(&mut self, ty: &str, name: &str) -> Result<Method, MetaError> {
        let t = self.id(ty)?;
        let m = self.members(t)?;
        if let Some(found) = m.methods.iter().find(|x| x.name == name) {
            return Ok(found.clone());
        }
        if self.decls[t].finders {
            let key = format!("method_missing:{ty}:{name}");
            let name = name.to_owned();
            if let Out::Finder(Some(found)) = self.cached(&key, |cx| finder(cx, t, &name).map(Out::Finder))? {
                return Ok(found);
            }
        }
        Err(MetaError::NoMethod { ty: ty.into(), name: name.into() })
    }
}

// ---------- the macros (stand-ins for pure Alexandrite functions) ----------

fn is_user_type(cx: &Ctx, ty: &str) -> bool {
    cx.by_name.contains_key(ty)
}

fn run_derive(cx: &mut Ctx, t: TyId, d: &Derive) -> Result<Vec<Method>, MetaError> {
    let tn = cx.decls[t].name.clone();
    let fields = cx.own_fields(t);
    let m = |name: &str, sig: String| Method { name: name.into(), sig, from: format!("derive({}) on {tn}", d.name()) };
    match d {
        Derive::Eq => {
            for f in &fields {
                if is_user_type(cx, &f.ty) {
                    cx.lookup(&f.ty, "==")?; // field types must be comparable
                }
            }
            Ok(vec![m("==", format!("==(other: {tn}) -> Bool"))])
        }
        Derive::Show => Ok(vec![m("to_s", "to_s -> Str".into())]),
        Derive::Json => {
            let mut keys = Vec::new();
            for f in &fields {
                if is_user_type(cx, &f.ty) {
                    cx.lookup(&f.ty, "to_json")?;
                }
                keys.push(f.rename.clone().unwrap_or_else(|| f.name.clone()));
            }
            Ok(vec![
                m("to_json", format!("to_json -> Str  # keys: {}", keys.join(","))),
                m("from_json", format!("from_json(s: Str) -> {tn}!JsonError")),
            ])
        }
        Derive::Mirror(other) => {
            let o = cx.id(other)?;
            let om = cx.members(o)?;
            Ok(om.methods.iter().map(|x| m(&format!("mirror_{}", x.name), x.sig.clone())).collect())
        }
    }
}

/// `model X from "path"`: a tiny JSON object of field -> type.
fn schema(cx: &mut Ctx, path: &str) -> Result<Vec<Field>, MetaError> {
    let text = cx.read_input(path)?;
    Ok(text
        .trim()
        .trim_matches(|c| c == '{' || c == '}')
        .split(',')
        .filter(|kv| !kv.trim().is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once(':').expect("key:value");
            let clean = |s: &str| s.trim().trim_matches('"').to_owned();
            Field { name: clean(k), ty: clean(v), rename: None }
        })
        .collect())
}

/// Compile-time method_missing: `find_by_<field>` if the field exists.
fn finder(cx: &mut Ctx, t: TyId, name: &str) -> Result<Option<Method>, MetaError> {
    let tn = cx.decls[t].name.clone();
    let Some(field) = name.strip_prefix("find_by_") else { return Ok(None) };
    let m = cx.members(t)?;
    Ok(m.fields.iter().find(|f| f.name == field).map(|f| Method {
        name: name.into(),
        sig: format!("{name}({}: {}) -> {tn}?", f.name, f.ty),
        from: format!("method_missing on {tn}"),
    }))
}

// ---------- the scenario from src.alx ----------

fn f(name: &str, ty: &str) -> Field {
    Field { name: name.into(), ty: ty.into(), rename: None }
}

fn scenario() -> Vec<TypeDecl> {
    let t = |name: &str, fields: Vec<Field>, derives: Vec<Derive>| TypeDecl { name: name.into(), fields, derives, schema: None, finders: false };
    vec![
        t("User", vec![f("id", "Int"), f("name", "Str"), Field { rename: Some("email_address".into()), ..f("email", "Str") }], vec![Derive::Eq, Derive::Show, Derive::Json]),
        t("Order", vec![f("id", "Int"), f("user", "User"), f("total", "Int")], vec![Derive::Eq, Derive::Json]),
        t("Audit", vec![f("note", "Str")], vec![Derive::Show]),
        TypeDecl { name: "Account".into(), fields: vec![], derives: vec![], schema: Some("schema/account.json".into()), finders: true },
        t("A", vec![], vec![Derive::Mirror("B".into())]),
        t("B", vec![], vec![Derive::Mirror("A".into())]),
        TypeDecl { name: "Leaky".into(), fields: vec![], derives: vec![], schema: Some("secrets.json".into()), finders: false },
    ]
}

/// The member lookups the type checker makes while checking `main`.
const USES: [(&str, &str); 6] = [
    ("User", "=="),
    ("Order", "to_json"),
    ("Account", "find_by_email"),
    ("Account", "find_by_phone"),
    ("A", "mirror_x"),
    ("Leaky", "new"),
];

struct BuildResult {
    results: Vec<Result<Method, MetaError>>,
    ran: Vec<String>,
    reused: usize,
    never_expanded: Vec<String>,
}

fn build(decls: &[TypeDecl], inputs: &HashMap<String, String>, cache: &mut Cache, uses: &[(&str, &str)]) -> BuildResult {
    let mut cx = Ctx::new(decls, inputs, cache);
    let results = uses.iter().map(|(t, n)| cx.lookup(t, n)).collect();
    let never_expanded = cx
        .state
        .iter()
        .enumerate()
        .filter(|(_, s)| matches!(s, State::Fresh))
        .map(|(i, _)| decls[i].name.clone())
        .collect();
    BuildResult { results, ran: cx.ran, reused: cx.reused, never_expanded }
}

fn print_build(label: &str, b: &BuildResult) {
    println!("### {label}\n");
    for ((t, n), r) in USES.iter().zip(&b.results) {
        match r {
            Ok(m) => println!("  ok    {t}.{n}: {}   [{}]", m.sig, m.from),
            Err(e) => println!("  error {t}.{n}: {e}"),
        }
    }
    println!("\n  macros run ({}): {}", b.ran.len(), if b.ran.is_empty() { "none".into() } else { b.ran.join(", ") });
    println!("  reused from cache: {}", b.reused);
    println!("  never expanded: {}\n", b.never_expanded.join(", "));
}

fn scenario_inputs(extra_field: bool) -> HashMap<String, String> {
    let json = if extra_field { r#"{"id": "Int", "email": "Str", "phone": "Str"}"# } else { r#"{"id": "Int", "email": "Str"}"# };
    HashMap::from([("schema/account.json".to_string(), json.to_string())])
}

// ---------- scaling ----------

/// N types in a chain: T_i has a field of type T_{i-1}, and derives
/// Eq, Show and Json, so Json(T_i) forces T_{i-1}.
fn chain(n: usize) -> Vec<TypeDecl> {
    (0..n)
        .map(|i| {
            let mut fields = vec![f("id", "Int"), f("name", "Str")];
            if i > 0 {
                fields.push(f("prev", &format!("T{}", i - 1)));
            }
            TypeDecl { name: format!("T{i}"), fields, derives: vec![Derive::Eq, Derive::Show, Derive::Json], schema: None, finders: false }
        })
        .collect()
}

fn scale() {
    println!("### scaling (chain of N types, 3 derives each; macros are native closures)\n");
    println!("| N | lookup | types expanded | macro runs | cold ms | warm rebuild ms | warm runs |");
    println!("|---|---|---|---|---|---|---|");
    let inputs = HashMap::new();
    for n in [1_000usize, 10_000] {
        let decls = chain(n);
        for (label, target) in [("last.to_json", n - 1), ("T5.to_json", 5)] {
            let name = format!("T{target}");
            let mut cache = Cache::default();
            let t0 = Instant::now();
            let b = build(&decls, &inputs, &mut cache, &[(&name, "to_json")]);
            let cold = t0.elapsed().as_secs_f64() * 1e3;
            assert!(b.results[0].is_ok());
            let expanded = n - b.never_expanded.len();
            let t1 = Instant::now();
            let w = build(&decls, &inputs, &mut cache, &[(&name, "to_json")]);
            let warm = t1.elapsed().as_secs_f64() * 1e3;
            println!("| {n} | {label} | {expanded} | {} | {cold:.2} | {warm:.2} | {} |", b.ran.len(), w.ran.len());
        }
    }
    println!();
}

fn main() {
    let decls = scenario();
    let mut cache = Cache::default();

    let b1 = build(&decls, &scenario_inputs(false), &mut cache, &USES);
    print_build("build 1 (cold)", &b1);
    let b2 = build(&decls, &scenario_inputs(false), &mut cache, &USES);
    print_build("build 2 (nothing changed)", &b2);
    let b3 = build(&decls, &scenario_inputs(true), &mut cache, &USES);
    print_build("build 3 (schema/account.json gained `phone`)", &b3);

    // Deep chains recurse once per type; give the scaling run a big stack.
    std::thread::Builder::new().stack_size(512 << 20).spawn(scale).unwrap().join().unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn set(v: &[String]) -> BTreeSet<String> {
        v.iter().cloned().collect()
    }

    #[test]
    fn cold_build() {
        let mut c = Cache::default();
        let b = build(&scenario(), &scenario_inputs(false), &mut c, &USES);
        assert!(b.results[0].is_ok());
        assert!(b.results[1].as_ref().unwrap().sig.contains("keys: id,user,total"));
        assert!(b.results[2].as_ref().unwrap().sig.starts_with("find_by_email(email: Str)"));
        assert_eq!(b.results[3], Err(MetaError::NoMethod { ty: "Account".into(), name: "find_by_phone".into() }));
        assert!(matches!(&b.results[4], Err(MetaError::Loop(chain)) if chain.len() == 3));
        assert_eq!(b.results[5], Err(MetaError::UndeclaredInput("secrets.json".into())));
        assert_eq!(b.never_expanded, vec!["Audit".to_string()]);
    }

    #[test]
    fn rename_attribute_reaches_generated_code() {
        let mut c = Cache::default();
        let b = build(&scenario(), &scenario_inputs(false), &mut c, &[("User", "to_json")]);
        assert!(b.results[0].as_ref().unwrap().sig.contains("keys: id,name,email_address"));
    }

    #[test]
    fn deps_are_direct() {
        let decls = chain(50);
        let mut c = Cache::default();
        build(&decls, &HashMap::new(), &mut c, &[("T49", "to_json")]);
        let widest = c.entries.values().map(|e| e.deps.len()).max().unwrap();
        assert!(widest <= 3, "an entry recorded {widest} deps; transitive deps leaked upward");
    }

    #[test]
    fn early_cutoff() {
        // Editing T0 reruns T0's derives and the T1 derives that looked T0
        // up. T1's member table comes out identical, so T2..T49 stay cached.
        let mut decls = chain(50);
        let mut c = Cache::default();
        build(&decls, &HashMap::new(), &mut c, &[("T49", "to_json")]);
        decls[0].fields.push(f("extra", "Int"));
        let b = build(&decls, &HashMap::new(), &mut c, &[("T49", "to_json")]);
        assert_eq!(
            set(&b.ran),
            set(&["derive:Eq:T0".into(), "derive:Show:T0".into(), "derive:Json:T0".into(), "derive:Eq:T1".into(), "derive:Json:T1".into()])
        );
    }

    #[test]
    fn warm_build_runs_nothing() {
        let mut c = Cache::default();
        build(&scenario(), &scenario_inputs(false), &mut c, &USES);
        let b = build(&scenario(), &scenario_inputs(false), &mut c, &USES);
        // Loops are never cached, so only the loop's macros rerun.
        assert!(b.ran.iter().all(|k| k.contains("Mirror")), "{:?}", b.ran);
    }

    #[test]
    fn input_edit_reruns_only_dependents() {
        let mut c = Cache::default();
        build(&scenario(), &scenario_inputs(false), &mut c, &USES);
        let b = build(&scenario(), &scenario_inputs(true), &mut c, &USES);
        let ran: BTreeSet<_> = set(&b.ran).into_iter().filter(|k| !k.contains("Mirror")).collect();
        assert_eq!(
            ran,
            set(&[
                "schema:Account".into(),
                "method_missing:Account:find_by_email".into(),
                "method_missing:Account:find_by_phone".into(),
            ])
        );
        assert!(b.results[3].is_ok(), "find_by_phone resolves once the field exists");
    }
}
