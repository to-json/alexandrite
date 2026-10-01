//! The Alexandrite compiler as a library: front end, lowering to LIR, and
//! the C and Rust-oracle emitters. Platform-free, so it also builds for
//! wasm32 (the in-browser compiler in `web/`). The native driver and the
//! Cranelift JIT live in the `alx` binary.
pub mod ast;
pub mod cgen;
pub mod check;
pub mod diag;
pub mod front;
pub mod lexer;
pub mod lir;
pub mod lower;
pub mod parser;
pub mod prove;
pub mod rgen;
pub mod tast;
