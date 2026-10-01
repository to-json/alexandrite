//! Go's exact constants: integer constants are arbitrary-precision integers,
//! float constants exact rationals. They fold at compile time and only round
//! (or must fit) when a use gives them a type.

use crate::ast::{BinOp, IntKind};
use crate::tast::ConstVal;
use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::{One, Signed, ToPrimitive, Zero};

/// The exact value of a decimal float literal (`4.84143e+00`, `0.1`).
pub fn parse_float(text: &str) -> Option<BigRational> {
    let t = text.replace('_', "");
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r.to_string()),
        None => (false, t),
    };
    let (mant, exp) = match t.find(['e', 'E']) {
        Some(i) => (&t[..i], t[i + 1..].parse::<i64>().ok()?),
        None => (&t[..], 0),
    };
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    let digits: BigInt = format!("{int}{frac}").parse().ok()?;
    let scale = exp - frac.len() as i64;
    if scale.unsigned_abs() > 100_000 {
        return None;
    }
    let ten = BigInt::from(10);
    let mut v = if scale >= 0 { BigRational::from_integer(digits * num_traits::pow(ten, scale as usize)) } else { BigRational::new(digits, num_traits::pow(ten, (-scale) as usize)) };
    if neg {
        v = -v;
    }
    Some(v)
}

/// The nearest f64 (ties to even), as Go converts float constants.
pub fn to_f64(q: &BigRational) -> f64 {
    q.to_f64().unwrap_or(if q.is_negative() { f64::NEG_INFINITY } else { f64::INFINITY })
}

fn as_rat(v: &ConstVal) -> BigRational {
    match v {
        ConstVal::Int(i) => BigRational::from_integer(i.clone()),
        ConstVal::Float(q) => q.clone(),
    }
}

/// Fold `a op b`. `Ok(None)`: not a constant operation (comparisons, `**`, `&&`).
pub fn fold(op: BinOp, a: &ConstVal, b: &ConstVal) -> Result<Option<ConstVal>, String> {
    use BinOp::*;
    if let (ConstVal::Int(x), ConstVal::Int(y)) = (a, b) {
        let r = match op {
            Add | AddW => x + y,
            Sub | SubW => x - y,
            Mul | MulW => x * y,
            Div | Rem => {
                if y.is_zero() {
                    return Err("division by zero in a constant".into());
                }
                // Go truncates.
                let (q, r) = (x / y, x % y);
                if op == Div { q } else { r }
            }
            BitAnd => x & y,
            BitOr => x | y,
            BitXor => x ^ y,
            AndNot => x & !y,
            Shl | Shr => {
                let n = y.to_u64().filter(|n| *n <= 10_000).ok_or("constant shift count must be between 0 and 10000")?;
                if op == Shl { x << n as usize } else { x >> n as usize }
            }
            _ => return Ok(None),
        };
        return Ok(Some(ConstVal::Int(r)));
    }
    // A Float on either side: exact rational arithmetic.
    let (x, y) = (as_rat(a), as_rat(b));
    let r = match op {
        Add => x + y,
        Sub => x - y,
        Mul => x * y,
        Div => {
            if y.is_zero() {
                return Err("division by zero in a constant".into());
            }
            x / y
        }
        Rem | BitAnd | BitOr | BitXor | AndNot | Shl | Shr | AddW | SubW | MulW => return Err(format!("`{}` needs integer constants", op.text())),
        _ => return Ok(None),
    };
    Ok(Some(ConstVal::Float(r)))
}

pub fn neg(v: &ConstVal) -> ConstVal {
    match v {
        ConstVal::Int(i) => ConstVal::Int(-i),
        ConstVal::Float(q) => ConstVal::Float(-q),
    }
}

/// The integer value, if the constant is an integer (a Float constant counts
/// when it is integral, as in Go: `var n int = 2.0`).
pub fn as_int(v: &ConstVal) -> Option<BigInt> {
    match v {
        ConstVal::Int(i) => Some(i.clone()),
        ConstVal::Float(q) if q.is_integer() => Some(q.to_integer()),
        _ => None,
    }
}

/// The bit pattern of an integer constant of kind `k`, if it fits.
pub fn fit(v: &BigInt, k: IntKind) -> Option<i64> {
    let x = v.to_i128()?;
    if x < k.min() || x > k.max() {
        return None;
    }
    Some(x as i64)
}

pub fn show(v: &ConstVal) -> String {
    match v {
        ConstVal::Int(i) => i.to_string(),
        ConstVal::Float(q) => {
            let f = to_f64(q);
            format!("{f}")
        }
    }
}
