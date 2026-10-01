//! Probe 04: does custom MIR (the "inline MIR" escape hatch) get borrow-checked?
//! Each function below is a borrowck violation written directly in MIR.
#![feature(custom_mir, core_intrinsics)]
#![allow(internal_features)]

use core::intrinsics::mir::*;

/// A sound custom-MIR body: one `&mut`, used, then the owner read.
/// Checks that the hatch works at all and still passes borrowck.
#[custom_mir(dialect = "built")]
fn bump(x: i32) -> i32 {
    mir! {
        let y: i32;
        let a: &mut i32;
        {
            y = x;
            a = &mut y;
            (*a) = (*a) + 1;
            RET = y;
            Return()
        }
    }
}

// --- `built` dialect: should be rejected by borrowck ---

/// Two live `&mut` to the same local, both used. Surface Rust: E0499.
#[cfg(feature = "violations")]
#[custom_mir(dialect = "built")]
fn aliasing_mut(x: i32) -> i32 {
    mir! {
        let y: i32;
        let a: &mut i32;
        let b: &mut i32;
        {
            y = x;
            a = &mut y;
            b = &mut y;
            (*a) = 1;
            (*b) = 2;
            RET = y;
            Return()
        }
    }
}

/// Use after move. Surface Rust: E0382.
#[cfg(feature = "violations")]
#[custom_mir(dialect = "built")]
fn use_after_move(s: String) -> usize {
    mir! {
        let a: String;
        let b: String;
        let r: &String;
        {
            a = Move(s);
            b = Move(s);
            r = &b;
            Call(RET = String::len(r), ReturnTo(done), UnwindContinue())
        }
        done = {
            Return()
        }
    }
}

// --- `runtime` dialect: borrowck skipped ---

/// Two live `&mut` to the same local, both used. Surface Rust: E0499.
#[cfg(all(feature = "unchecked", not(feature = "violations")))]
#[custom_mir(dialect = "runtime")]
fn aliasing_mut(x: i32) -> i32 {
    mir! {
        let y: i32;
        let a: &mut i32;
        let b: &mut i32;
        {
            y = x;
            a = &mut y;
            b = &mut y;
            (*a) = 1;
            (*b) = 2;
            RET = y;
            Return()
        }
    }
}

/// Use after move. Surface Rust: E0382.
#[cfg(all(feature = "unchecked", not(feature = "violations")))]
#[custom_mir(dialect = "runtime")]
fn use_after_move(s: String) -> usize {
    mir! {
        let a: String;
        let b: String;
        let r: &String;
        {
            a = Move(s);
            b = Move(s);
            r = &b;
            Call(RET = String::len(r), ReturnTo(done), UnwindContinue())
        }
        done = {
            Return()
        }
    }
}

fn main() {
    println!("bump(41)       -> {}", bump(41));
    #[cfg(any(feature = "violations", feature = "unchecked"))]
    {
        println!("aliasing_mut   -> {}", aliasing_mut(0));
        println!("use_after_move -> {}", use_after_move(String::from("hello")));
    }
}
