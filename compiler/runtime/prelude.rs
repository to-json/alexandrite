// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
// Prelude for the Rust oracle backend: the same runtime surface as alx.h,
// in safe Rust, so rustc checks every emitted program.
#![feature(gen_blocks)]
#![allow(unused, non_snake_case, unreachable_code, unused_mut, unused_variables, unused_assignments, unused_labels, while_true, dead_code, unused_parens)]

mod rt {
    use std::cell::RefCell;
    use std::cmp::Ordering;
    use std::rc::Rc;
    use std::sync::Arc;

    #[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Debug)]
    pub struct Str(pub Arc<[u8]>);

    impl Str {
        pub fn lit(b: &[u8]) -> Str {
            Str(Arc::from(b))
        }
        pub fn len(&self) -> i64 {
            self.0.len() as i64
        }
    }

    #[derive(Clone, Copy, Default, Debug)]
    pub struct AlxRange {
        pub lo: i64,
        pub hi: i64,
        pub excl: bool,
    }

    #[derive(Clone, Debug)]
    pub enum AlxErr {
        Overflow(&'static str),
        NotFound(Str, &'static str),
        Io(Str, &'static str),
    }

    /// Enumerator: a shared, resumable iterator.
    pub struct Gen<T>(pub Rc<RefCell<Box<dyn Iterator<Item = T>>>>);
    impl<T> Clone for Gen<T> {
        fn clone(&self) -> Self {
            Gen(self.0.clone())
        }
    }
    impl<T: 'static> Default for Gen<T> {
        fn default() -> Self {
            Gen(Rc::new(RefCell::new(Box::new(std::iter::empty()))))
        }
    }
    impl<T: 'static> Gen<T> {
        pub fn new(it: impl Iterator<Item = T> + 'static) -> Self {
            Gen(Rc::new(RefCell::new(Box::new(it))))
        }
        pub fn next(&self) -> Option<T> {
            self.0.borrow_mut().next()
        }
    }

    pub fn panic(what: &str, loc: &str) -> ! {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        eprintln!("alexandrite: {what} at {loc}");
        std::process::abort()
    }
    pub fn overflow(loc: &str) -> ! {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        eprintln!("alexandrite: overflow at {loc}\nhint: add `#![overflow(promote)]` to this file to promote to bignums");
        std::process::abort()
    }
    pub fn die(e: AlxErr) -> ! {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        match e {
            AlxErr::NotFound(p, loc) => eprintln!("File.read: no such file `{}` ({loc})", String::from_utf8_lossy(&p.0)),
            AlxErr::Io(p, loc) => eprintln!("File.read: cannot read `{}` ({loc})", String::from_utf8_lossy(&p.0)),
            AlxErr::Overflow(loc) => eprintln!("error: overflow ({loc})"),
        }
        std::process::exit(1)
    }
    pub fn err_overflow(loc: &'static str) -> AlxErr {
        AlxErr::Overflow(loc)
    }

    pub fn idx(i: i64, n: usize, loc: &str) -> usize {
        if i < 0 || i as u64 >= n as u64 {
            panic("index out of bounds", loc)
        }
        i as usize
    }

    pub fn add(a: i64, b: i64, loc: &str) -> i64 {
        a.checked_add(b).unwrap_or_else(|| overflow(loc))
    }
    pub fn sub(a: i64, b: i64, loc: &str) -> i64 {
        a.checked_sub(b).unwrap_or_else(|| overflow(loc))
    }
    pub fn mul(a: i64, b: i64, loc: &str) -> i64 {
        a.checked_mul(b).unwrap_or_else(|| overflow(loc))
    }
    pub fn div(a: i64, b: i64, loc: &str) -> i64 {
        if b == 0 {
            panic("division by zero", loc)
        }
        try_div(a, b).unwrap_or_else(|| overflow(loc))
    }
    pub fn rem(a: i64, b: i64, loc: &str) -> i64 {
        if b == 0 {
            panic("division by zero", loc)
        }
        try_rem(a, b).unwrap()
    }
    pub fn pow(a: i64, b: i64, loc: &str) -> i64 {
        if b < 0 {
            panic("negative exponent", loc)
        }
        try_pow(a, b).unwrap_or_else(|| overflow(loc))
    }
    pub fn neg(a: i64, loc: &str) -> i64 {
        a.checked_neg().unwrap_or_else(|| overflow(loc))
    }
    pub fn try_add(a: i64, b: i64) -> Option<i64> {
        a.checked_add(b)
    }
    pub fn try_sub(a: i64, b: i64) -> Option<i64> {
        a.checked_sub(b)
    }
    pub fn try_mul(a: i64, b: i64) -> Option<i64> {
        a.checked_mul(b)
    }
    pub fn try_div(a: i64, b: i64) -> Option<i64> {
        if b == 0 {
            return None;
        }
        let q = a.checked_div(b)?;
        Some(if a % b != 0 && ((a < 0) != (b < 0)) { q - 1 } else { q })
    }
    pub fn try_rem(a: i64, b: i64) -> Option<i64> {
        if b == 0 {
            return None;
        }
        if b == -1 {
            return Some(0);
        }
        let r = a % b;
        Some(if r != 0 && ((r < 0) != (b < 0)) { r + b } else { r })
    }
    pub fn try_pow(a: i64, b: i64) -> Option<i64> {
        if b < 0 || b > u32::MAX as i64 {
            return None;
        }
        a.checked_pow(b as u32)
    }
    pub fn sat_add(a: i64, b: i64) -> i64 {
        a.saturating_add(b)
    }
    pub fn even(a: i64) -> bool {
        a & 1 == 0
    }
    pub fn isqrt(n: i64, loc: &str) -> i64 {
        if n < 0 {
            panic("Int.sqrt of a negative number", loc)
        }
        n.isqrt()
    }
    pub fn digits(mut v: i64, loc: &str) -> Vec<i64> {
        if v < 0 {
            panic("`digits` of a negative number", loc)
        }
        let mut d = vec![];
        loop {
            d.push(v % 10);
            v /= 10;
            if v == 0 {
                return d;
            }
        }
    }

    pub fn int_to_s(v: i64) -> Str {
        Str::lit(v.to_string().as_bytes())
    }
    pub fn str_rev(s: Str) -> Str {
        let t = String::from_utf8_lossy(&s.0).chars().rev().collect::<String>();
        Str::lit(t.as_bytes())
    }
    pub fn str_delete(s: Str, chars: Str) -> Str {
        Str(s.0.iter().copied().filter(|b| !chars.0.contains(b)).collect::<Vec<u8>>().into())
    }
    pub fn str_split(s: Str, sep: Str) -> Vec<Str> {
        let mut out = vec![];
        let (b, p) = (&s.0[..], &sep.0[..]);
        let mut start = 0;
        let mut i = 0;
        while i + p.len() <= b.len() {
            if &b[i..i + p.len()] == p {
                out.push(Str::lit(&b[start..i]));
                i += p.len();
                start = i;
            } else {
                i += 1;
            }
        }
        out.push(Str::lit(&b[start..]));
        while out.last().is_some_and(|x| x.0.is_empty()) {
            out.pop();
        }
        out
    }
    pub fn str_to_i(s: Str) -> i64 {
        let t = String::from_utf8_lossy(&s.0);
        let t = t.trim_start();
        let (neg, rest) = match t.as_bytes().first() {
            Some(b'-') => (true, &t[1..]),
            Some(b'+') => (false, &t[1..]),
            _ => (false, t),
        };
        let mut v: i64 = 0;
        for c in rest.bytes().take_while(u8::is_ascii_digit) {
            v = v.checked_mul(10).and_then(|v| v.checked_add((c - b'0') as i64)).unwrap_or_else(|| overflow("String#to_i"));
        }
        if neg { -v } else { v }
    }
    pub fn str_charlen(s: &Str, i: i64) -> i64 {
        let c = s.0[i as usize];
        let n = if c < 0x80 { 1 } else if c < 0xE0 { 2 } else if c < 0xF0 { 3 } else { 4 };
        n.min(s.0.len() as i64 - i)
    }
    pub fn str_sub(s: &Str, i: i64, n: i64) -> Str {
        Str::lit(&s.0[i as usize..(i + n) as usize])
    }
    pub fn str_byte(s: &Str, i: i64) -> i64 {
        s.0[i as usize] as i64
    }
    pub fn file_read(p: Str, loc: &'static str) -> Result<Str, AlxErr> {
        let path = String::from_utf8_lossy(&p.0).to_string();
        match std::fs::read(&path) {
            Ok(b) => Ok(Str(b.into())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(AlxErr::NotFound(p, loc)),
            Err(_) => Err(AlxErr::Io(p, loc)),
        }
    }

    pub fn arr_new<T: Clone>(n: i64, fill: T, loc: &str) -> Vec<T> {
        if n < 0 {
            panic("negative array size", loc)
        }
        vec![fill; n as usize]
    }
    pub fn cap(n: i64) -> usize {
        n.max(0) as usize
    }

    pub fn puts_i64(v: i64) {
        println!("{v}");
    }
    pub fn puts_str(s: Str) {
        use std::io::Write;
        let mut o = std::io::stdout();
        let _ = o.write_all(&s.0);
        if s.0.last() != Some(&b'\n') {
            let _ = o.write_all(b"\n");
        }
    }
    pub fn puts_bool(b: bool) {
        println!("{b}");
    }
    pub fn puts_pint(v: PInt) {
        println!("{}", v.to_string());
    }

    pub fn pmap<T: Clone + Sync, R: Clone + Send + Default>(xs: &[T], f: fn(T) -> Result<R, AlxErr>) -> Result<Vec<R>, AlxErr> {
        let mut out = vec![R::default(); xs.len()];
        let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
        let chunk = xs.len().div_ceil(workers).max(1);
        let mut errs: Vec<Option<AlxErr>> = vec![None; xs.len().div_ceil(chunk)];
        std::thread::scope(|s| {
            for ((src, dst), err) in xs.chunks(chunk).zip(out.chunks_mut(chunk)).zip(errs.iter_mut()) {
                s.spawn(move || {
                    for (x, o) in src.iter().zip(dst) {
                        match f(x.clone()) {
                            Ok(v) => *o = v,
                            Err(e) => {
                                *err = Some(e);
                                return;
                            }
                        }
                    }
                });
            }
        });
        match errs.into_iter().flatten().next() {
            Some(e) => Err(e),
            None => Ok(out),
        }
    }

    // ---------- bignums: sign + magnitude in base 1e9 ----------

    const BASE: u64 = 1_000_000_000;

    #[derive(Clone, Default, Debug, PartialEq, Eq)]
    pub struct PInt {
        neg: bool,
        /// Little-endian base-1e9 digits; empty = zero.
        mag: Vec<u32>,
    }

    fn trim(m: &mut Vec<u32>) {
        while m.last() == Some(&0) {
            m.pop();
        }
    }
    fn cmp_mag(a: &[u32], b: &[u32]) -> Ordering {
        if a.len() != b.len() {
            return a.len().cmp(&b.len());
        }
        for i in (0..a.len()).rev() {
            if a[i] != b[i] {
                return a[i].cmp(&b[i]);
            }
        }
        Ordering::Equal
    }
    fn add_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
        let mut out = Vec::with_capacity(a.len().max(b.len()) + 1);
        let mut carry = 0u64;
        for i in 0..a.len().max(b.len()) {
            let s = carry + *a.get(i).unwrap_or(&0) as u64 + *b.get(i).unwrap_or(&0) as u64;
            out.push((s % BASE) as u32);
            carry = s / BASE;
        }
        if carry > 0 {
            out.push(carry as u32);
        }
        out
    }
    /// a - b where |a| >= |b|.
    fn sub_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
        let mut out = Vec::with_capacity(a.len());
        let mut borrow = 0i64;
        for i in 0..a.len() {
            let mut d = a[i] as i64 - borrow - *b.get(i).unwrap_or(&0) as i64;
            borrow = 0;
            if d < 0 {
                d += BASE as i64;
                borrow = 1;
            }
            out.push(d as u32);
        }
        trim(&mut out);
        out
    }
    fn mul_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
        if a.is_empty() || b.is_empty() {
            return vec![];
        }
        let mut out = vec![0u64; a.len() + b.len()];
        for (i, &x) in a.iter().enumerate() {
            let mut carry = 0u64;
            for (j, &y) in b.iter().enumerate() {
                let cur = out[i + j] + x as u64 * y as u64 + carry;
                out[i + j] = cur % BASE;
                carry = cur / BASE;
            }
            let mut k = i + b.len();
            while carry > 0 {
                let cur = out[k] + carry;
                out[k] = cur % BASE;
                carry = cur / BASE;
                k += 1;
            }
        }
        let mut m: Vec<u32> = out.into_iter().map(|d| d as u32).collect();
        trim(&mut m);
        m
    }

    impl PInt {
        pub fn from_i64(v: i64) -> PInt {
            let mut mag = vec![];
            let mut u = v.unsigned_abs();
            while u > 0 {
                mag.push((u % BASE) as u32);
                u /= BASE;
            }
            PInt { neg: v < 0, mag }
        }
        pub fn to_i64(&self, loc: &str) -> i64 {
            let mut u: i128 = 0;
            for &d in self.mag.iter().rev() {
                u = u * BASE as i128 + d as i128;
                if u > i64::MAX as i128 + 1 {
                    overflow(loc)
                }
            }
            let v = if self.neg { -u } else { u };
            i64::try_from(v).unwrap_or_else(|_| overflow(loc))
        }
        pub fn add(&self, o: &PInt) -> PInt {
            if self.neg == o.neg {
                return PInt { neg: self.neg, mag: add_mag(&self.mag, &o.mag) };
            }
            match cmp_mag(&self.mag, &o.mag) {
                Ordering::Less => PInt { neg: o.neg, mag: sub_mag(&o.mag, &self.mag) },
                _ => {
                    let mag = sub_mag(&self.mag, &o.mag);
                    PInt { neg: self.neg && !mag.is_empty(), mag }
                }
            }
        }
        pub fn sub(&self, o: &PInt) -> PInt {
            self.add(&PInt { neg: !o.neg && !o.mag.is_empty(), mag: o.mag.clone() })
        }
        pub fn mul(&self, o: &PInt) -> PInt {
            let mag = mul_mag(&self.mag, &o.mag);
            PInt { neg: (self.neg != o.neg) && !mag.is_empty(), mag }
        }
        pub fn pow(&self, e: &PInt, loc: &str) -> PInt {
            let mut e = e.to_i64(loc);
            if e < 0 {
                panic("negative exponent", loc)
            }
            let mut base = self.clone();
            let mut acc = PInt::from_i64(1);
            while e > 0 {
                if e & 1 == 1 {
                    acc = acc.mul(&base);
                }
                e >>= 1;
                if e > 0 {
                    base = base.mul(&base);
                }
            }
            acc
        }
        pub fn cmp(&self, o: &PInt) -> Ordering {
            match (self.neg, o.neg) {
                (false, true) => Ordering::Greater,
                (true, false) => Ordering::Less,
                (false, false) => cmp_mag(&self.mag, &o.mag),
                (true, true) => cmp_mag(&o.mag, &self.mag),
            }
        }
        pub fn even(&self) -> bool {
            self.mag.first().is_none_or(|d| d % 2 == 0)
        }
        pub fn to_string(&self) -> String {
            if self.mag.is_empty() {
                return "0".into();
            }
            let mut s = String::new();
            if self.neg {
                s.push('-');
            }
            s.push_str(&self.mag.last().unwrap().to_string());
            for d in self.mag.iter().rev().skip(1) {
                s.push_str(&format!("{d:09}"));
            }
            s
        }
        pub fn to_s(&self) -> Str {
            Str::lit(self.to_string().as_bytes())
        }
        pub fn digits(&self, loc: &str) -> Vec<PInt> {
            if self.neg {
                panic("`digits` of a negative number", loc)
            }
            self.to_string().bytes().rev().map(|b| PInt::from_i64((b - b'0') as i64)).collect()
        }
        pub fn div(&self, o: &PInt, loc: &str) -> PInt {
            PInt::from_i64(div(self.to_i64(loc), o.to_i64(loc), loc))
        }
        pub fn rem(&self, o: &PInt, loc: &str) -> PInt {
            PInt::from_i64(rem(self.to_i64(loc), o.to_i64(loc), loc))
        }
    }
}

use rt::*;
