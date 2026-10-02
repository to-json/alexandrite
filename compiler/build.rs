//! Links the C runtime into `alx` itself, for the JIT behind `alx run`,
//! and embeds the standard library's packages (`std/`).
fn main() {
    embed_std();
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32") {
        return;
    }
    let rt = "runtime";
    for f in ["alx.h", "alx.c", "alx_big.c", "jit_shims.c", "libtommath/tommath_amalgam.c"] {
        println!("cargo:rerun-if-changed={rt}/{f}");
    }
    cc::Build::new()
        .files([format!("{rt}/alx.c"), format!("{rt}/alx_big.c"), format!("{rt}/jit_shims.c")])
        .include(rt)
        .include(format!("{rt}/libtommath"))
        .flag("-std=gnu11")
        .flag("-fwrapv")
        .flag("-w")
        .opt_level(2)
        .compile("alxrt");
}

/// `STD`: every `.alx` file under `std/`, as (path inside std, source).
fn embed_std() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../std");
    println!("cargo:rerun-if-changed={}", root.display());
    let mut files = vec![];
    let mut dirs = vec![root.clone()];
    while let Some(d) = dirs.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                dirs.push(p);
            } else if p.extension().is_some_and(|x| x == "alx") {
                files.push(p);
            }
        }
    }
    files.sort();
    let mut out = String::from("pub static STD: &[(&str, &str)] = &[\n");
    for f in &files {
        let rel = f.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
        out.push_str(&format!("    ({rel:?}, include_str!({:?})),\n", f.canonicalize().unwrap()));
    }
    out.push_str("];\n");
    let dst = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("std_files.rs");
    std::fs::write(dst, out).unwrap();
}
