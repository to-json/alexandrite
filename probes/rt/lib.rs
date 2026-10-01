//! What the Alexandrite compiler would link against. Small on purpose: if the
//! emitted Rust needs more than this, that's a finding.

use std::alloc::{GlobalAlloc, Layout, System};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

// ---------- cost counters: every hidden cost gets a number ----------

/// Elements copied by copy-and-hand-off.
pub static COPIES: AtomicUsize = AtomicUsize::new(0);
/// Times a pool outgrew its 2x headroom and had to reallocate.
pub static SPILLS: AtomicUsize = AtomicUsize::new(0);

pub fn report(label: &str) {
    println!(
        "[{label}] copies={} spills={}",
        COPIES.load(Relaxed),
        SPILLS.load(Relaxed)
    );
}

pub fn reset_counters() {
    COPIES.store(0, Relaxed);
    SPILLS.store(0, Relaxed);
}

// ---------- typed pool ----------

/// A handle into a `Pool<T>`. Copyable, 4 bytes, no lifetime.
pub struct Id<T>(u32, PhantomData<fn() -> T>);

impl<T> Clone for Id<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Id<T> {}
impl<T> PartialEq for Id<T> {
    fn eq(&self, o: &Self) -> bool {
        self.0 == o.0
    }
}
impl<T> Eq for Id<T> {}
impl<T> std::fmt::Debug for Id<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}
impl<T> Id<T> {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// Arena allocated at a declaration with 2x the compile-time-known size.
pub struct Pool<T> {
    slots: Vec<T>,
}

impl<T> Pool<T> {
    /// `n` is the size the compiler knows statically; we reserve 2n.
    pub fn with_headroom(n: usize) -> Self {
        Pool { slots: Vec::with_capacity(2 * n) }
    }

    /// Exactly `n` slots: for destinations whose size a header proves.
    pub fn exact(n: usize) -> Self {
        Pool { slots: Vec::with_capacity(n) }
    }

    pub fn put(&mut self, v: T) -> Id<T> {
        if self.slots.len() == self.slots.capacity() {
            SPILLS.fetch_add(1, Relaxed);
        }
        let id = Id(self.slots.len() as u32, PhantomData);
        self.slots.push(v);
        id
    }

    pub fn get(&self, id: Id<T>) -> &T {
        &self.slots[id.index()]
    }

    pub fn get_mut(&mut self, id: Id<T>) -> &mut T {
        &mut self.slots[id.index()]
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.slots.iter()
    }

    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, T> {
        self.slots.iter_mut()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.slots.capacity()
    }
}

/// Types the compiler would derive this for: rebuild a value with its
/// child handles remapped. Used for compacting copy-out.
pub trait Relocate: Sized {
    fn relocate(&self, f: &mut dyn FnMut(Id<Self>) -> Id<Self>) -> Self;
}

/// A value together with the pool it lives in. Returning one of these is
/// "the value escapes by taking its region with it".
pub struct Rooted<T> {
    pub pool: Pool<T>,
    pub root: Id<T>,
}

impl<T: Relocate> Rooted<T> {
    /// Copy-out at return: copy everything reachable from `root` into a fresh
    /// exact-size pool (Cheney-style: the new pool is the to-space). Garbage
    /// stays behind and dies with the old pool.
    pub fn compact(&self) -> Rooted<T> {
        let mut dst = Pool::with_headroom(self.pool.len().div_ceil(2));
        let root = copy_rec(&self.pool, self.root, &mut dst);
        Rooted { pool: dst, root }
    }
}

fn copy_rec<T: Relocate>(src: &Pool<T>, id: Id<T>, dst: &mut Pool<T>) -> Id<T> {
    let v = src.get(id).relocate(&mut |child| copy_rec(src, child, dst));
    COPIES.fetch_add(1, Relaxed);
    dst.put(v)
}

// ---------- byte arena for strings ----------

/// A string handle: a span in a `Bytes` arena.
#[derive(Clone, Copy, Debug)]
pub struct Span {
    start: u32,
    len: u32,
}

pub struct Bytes {
    buf: Vec<u8>,
}

impl Bytes {
    pub fn with_headroom(n: usize) -> Self {
        Bytes { buf: Vec::with_capacity(2 * n) }
    }

    fn reserve_for(&mut self, extra: usize) {
        if self.buf.len() + extra > self.buf.capacity() {
            SPILLS.fetch_add(1, Relaxed);
        }
    }

    pub fn push_str(&mut self, s: &str) -> Span {
        self.reserve_for(s.len());
        let start = self.buf.len() as u32;
        self.buf.extend_from_slice(s.as_bytes());
        Span { start, len: s.len() as u32 }
    }

    pub fn get(&self, s: Span) -> &str {
        let r = s.start as usize..(s.start + s.len) as usize;
        std::str::from_utf8(&self.buf[r]).expect("arena holds utf-8")
    }

    /// Build a new span from an existing one in the same arena, byte by byte.
    /// ASCII-only transforms (keeps utf-8 valid for ASCII input).
    pub fn map(&mut self, s: Span, f: impl Fn(u8) -> u8) -> Span {
        self.reserve_for(s.len as usize);
        let start = self.buf.len() as u32;
        for i in s.start..s.start + s.len {
            let b = self.buf[i as usize];
            self.buf.push(f(b));
        }
        Span { start, len: s.len }
    }

    /// Narrow a span without copying.
    pub fn trim(&self, s: Span) -> Span {
        let t = self.get(s);
        let lead = t.len() - t.trim_start().len();
        let len = t.trim().len();
        Span { start: s.start + lead as u32, len: len as u32 }
    }

    /// Copy-and-hand-off: copy a span out of another arena into this one.
    pub fn copy_from(&mut self, src: &Bytes, s: Span) -> Span {
        COPIES.fetch_add(s.len as usize, Relaxed);
        self.push_str(src.get(s))
    }

    /// Reset for the next iteration. Keeps the allocation.
    pub fn clear(&mut self) {
        self.buf.clear();
    }

    pub fn used(&self) -> usize {
        self.buf.len()
    }
}

// ---------- counting allocator ----------

pub struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        let now = LIVE.fetch_add(l.size(), Relaxed) + l.size();
        PEAK.fetch_max(now, Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        if new > l.size() {
            let now = LIVE.fetch_add(new - l.size(), Relaxed) + new - l.size();
            PEAK.fetch_max(now, Relaxed);
        } else {
            LIVE.fetch_sub(l.size() - new, Relaxed);
        }
        unsafe { System.realloc(p, l, new) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Relaxed);
        unsafe { System.dealloc(p, l) }
    }
}

pub fn live_bytes() -> usize {
    LIVE.load(Relaxed)
}

/// Total allocations ever made (realloc counts as one).
pub fn alloc_count() -> usize {
    ALLOCS.load(Relaxed)
}

pub fn peak_bytes() -> usize {
    PEAK.load(Relaxed)
}

// ---------- branded pools (probe 03b) ----------

/// Invariant brand: `'id` can be neither shortened nor lengthened, so two
/// pools never share one and a handle can't outlive its pool.
#[derive(Clone, Copy)]
pub struct Brand<'id>(PhantomData<fn(&'id ()) -> &'id ()>);

/// `@T` in Alexandrite.
pub struct Handle<'id, T> {
    i: u32,
    _b: Brand<'id>,
    _t: PhantomData<fn() -> T>,
}
impl<T> Clone for Handle<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Handle<'_, T> {}

pub struct BPool<'id, T> {
    slots: Vec<T>,
    b: Brand<'id>,
}

impl<'id, T> BPool<'id, T> {
    pub fn put(&mut self, v: T) -> Handle<'id, T> {
        self.slots.push(v);
        Handle { i: self.slots.len() as u32 - 1, _b: self.b, _t: PhantomData }
    }
    pub fn get(&self, h: Handle<'id, T>) -> &T {
        &self.slots[h.i as usize]
    }
}

/// Open a pool with 2x headroom; each call mints a fresh brand.
pub fn with_pool<T, R>(n: usize, f: impl for<'id> FnOnce(BPool<'id, T>) -> R) -> R {
    f(BPool { slots: Vec::with_capacity(2 * n), b: Brand(PhantomData) })
}
