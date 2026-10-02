mod driver;
mod jit;

pub use alx::{cgen, check, front, lir, lower, rgen, tast};

use std::process::ExitCode;

const USAGE: &str = "usage: alx run [--release] [--sanitize] [--expect VALUE] [--emit-c FILE] [--emit-rust FILE] [-v] file.alx
       alx build [--release] [--sanitize] [-o OUT] file.alx
       alx check file.alx
       alx fmt [--check] [files|dirs...]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    if cmd == "fmt" {
        return ExitCode::from(alx::fmt::cli(&args[1..]) as u8);
    }
    let mut o = driver::Options::default();
    let mut file = None;
    let mut i = 1;
    while i < args.len() {
        let a = args[i].as_str();
        let mut val = || {
            i += 1;
            args.get(i).cloned()
        };
        match a {
            "--release" => o.release = true,
            "--sanitize" => o.sanitize = true,
            "-v" | "--verbose" => o.verbose = true,
            "--expect" => o.expect = val(),
            "--emit-c" => o.emit_c = val(),
            "--emit-rust" => o.emit_rust = val(),
            "-o" => o.out = val(),
            f if !f.starts_with('-') => file = Some(f.to_string()),
            f => {
                eprintln!("unknown flag `{f}`\n{USAGE}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }
    let Some(file) = file else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    match cmd.as_str() {
        "run" => driver::run(&file, &o),
        "build" => driver::build_only(&file, &o),
        "check" => driver::check_only(&file),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
