//! Links the C runtime into `alx` itself, for the JIT behind `alx run`.
fn main() {
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
