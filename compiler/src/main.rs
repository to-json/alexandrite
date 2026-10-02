mod driver;
mod jit;

pub use alx::{cgen, check, front, lir, lower, rgen, tast};

use std::process::ExitCode;

const USAGE: &str = "usage: alx run [--release] [--sanitize] [--expect VALUE] [--emit-c FILE] [--emit-rust FILE] [-v] file.alx
       alx build [--release] [--sanitize] [-o OUT] file.alx
       alx check file.alx
       alx fmt [--check] [files|dirs...]
       alx test [--release] [-run NAME] [-bench NAME] [-benchtime DUR] [dir | file_test.alx]";

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
    let mut t = alx::front::TestOpts { run: None, bench: None, bench_ns: 500_000_000 };
    if let Some(v) = std::env::var("ALX_BENCHTIME").ok().and_then(|v| driver::parse_duration_ns(&v)) {
        t.bench_ns = v;
    }
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
            "--strict" => o.strict = true,
            "--expect" => o.expect = val(),
            "-run" => t.run = val(),
            "-bench" => t.bench = val(),
            "-benchtime" => match val().as_deref().and_then(driver::parse_duration_ns) {
                Some(ns) => t.bench_ns = ns,
                None => {
                    eprintln!("alx: `-benchtime` takes a duration like `100ms` or `2s`");
                    return ExitCode::from(2);
                }
            },
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
    if cmd == "test" {
        return driver::test(file.as_deref().unwrap_or("."), &t, &o);
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
