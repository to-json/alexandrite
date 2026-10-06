//! The Alexandrite compiler as a library: front end, lowering to LIR, and
//! the C and Rust-oracle emitters. Platform-free, so it also builds for
//! wasm32 (the in-browser compiler in `web/`). The native driver and the
//! Cranelift JIT live in the `alx` binary.
pub mod analyze;
pub mod ast;
pub mod capture;
pub mod cgen;
pub mod check;
pub mod check_json;
pub mod consts;
pub mod derive;
pub mod derive_asn1;
pub mod derive_data;
pub mod derive_json2;
pub mod derive_gob;
pub mod derive_xml;
pub mod diag;
pub mod embed;
pub mod fmt;
pub mod front;
pub mod lexer;
pub mod lir;
pub mod lower;
pub mod mapgen;
pub mod parser;
pub mod prove;
pub mod regions;
pub mod rgen;
pub mod sharing;
pub mod strgen;
pub mod tast;
