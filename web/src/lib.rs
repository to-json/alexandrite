//! Alexandrite in the browser: the compiler (front end and lowering, from
//! the `alx` crate) plus a WebAssembly backend and the runtime, in one wasm
//! module. `compile` returns a program module; the page instantiates it
//! with this module's memory and `alxr_*` exports as imports, then calls
//! its `main`.

mod rt;
mod wasmgen;

use std::path::Path;
use wasm_bindgen::prelude::*;

/// Make a file visible to `require` and `File.read` (paths relative to the
/// program, e.g. `lib/primes.alx`, `fixtures/names.txt`).
#[wasm_bindgen]
pub fn set_file(path: &str, data: &[u8]) {
    rt::st().files.insert(path.to_string(), data.to_vec());
}

/// Compile `source` (shown as `name` in messages) to a WebAssembly module.
/// Errs with the rendered diagnostic.
#[wasm_bindgen]
pub fn compile(name: &str, source: &str) -> Result<Vec<u8>, String> {
    let read = |p: &Path| -> std::io::Result<String> {
        let key = p.to_string_lossy().trim_start_matches("./").to_string();
        if key == name {
            return Ok(source.to_string());
        }
        match rt::st().files.get(&key) {
            Some(b) => Ok(String::from_utf8_lossy(b).into_owned()),
            None => Err(std::io::Error::new(std::io::ErrorKind::NotFound, "not found")),
        }
    };
    // Packages: the files under a directory in the virtual file system.
    let list = |p: &Path| -> std::io::Result<Vec<std::path::PathBuf>> {
        let dir = p.to_string_lossy().trim_start_matches("./").trim_end_matches('/').to_string();
        let prefix = if dir.is_empty() || dir == "." { String::new() } else { format!("{dir}/") };
        let mut v: Vec<std::path::PathBuf> = rt::st().files.keys().filter(|k| k.starts_with(&prefix) && !k[prefix.len()..].contains('/')).map(std::path::PathBuf::from).collect();
        v.sort();
        Ok(v)
    };
    let l = alx::front::load_with(Path::new(name), name, &read, &list).map_err(|(sm, d)| sm.render(&d))?;
    let p = alx::front::check_program(&l, alx::front::lib_defs(&l)).map_err(|d| l.sm.render(&d))?;
    let lp = alx::lower::lower(&p, &l.sm, &alx::lower::Opts { release: false });
    wasmgen::emit(&lp).map_err(|e| if e.contains("aren't available in the browser") { format!("error: {e}") } else { format!("internal compiler error: {e}") })
}

/// Start a run: fresh memory region and output buffers.
#[wasm_bindgen]
pub fn begin_run() {
    rt::reset();
}

#[wasm_bindgen]
pub fn take_stdout() -> String {
    std::mem::take(&mut rt::st().out)
}

#[wasm_bindgen]
pub fn take_stderr() -> String {
    std::mem::take(&mut rt::st().err)
}
