//! Probe 03b: branded handles. `cargo run -p probe_graph --bin brand` for the
//! sound case; `--features cross` / `--features escape` must fail to compile.
use std::marker::PhantomData;
// Invariant brand: 'id can't be shortened or lengthened.
#[derive(Clone, Copy)]
struct Brand<'id>(PhantomData<fn(&'id ()) -> &'id ()>);
#[derive(Clone, Copy)]
struct Id<'id, T> { i: u32, _b: Brand<'id>, _t: PhantomData<fn() -> T> }
struct Pool<'id, T> { slots: Vec<T>, b: Brand<'id> }
impl<'id, T> Pool<'id, T> {
    fn put(&mut self, v: T) -> Id<'id, T> { self.slots.push(v); Id { i: self.slots.len() as u32 - 1, _b: self.b, _t: PhantomData } }
    fn get(&self, id: Id<'id, T>) -> &T { &self.slots[id.i as usize] }
}
// Each call mints a fresh, unnameable 'id.
fn with_pool<T, R>(n: usize, f: impl for<'id> FnOnce(Pool<'id, T>) -> R) -> R {
    f(Pool { slots: Vec::with_capacity(2 * n), b: Brand(PhantomData) })
}
fn main() {
    let v = with_pool(4, |mut p: Pool<'_, &str>| { let a = p.put("ashby"); p.get(a).len() });
    println!("same pool ok: {v}");
    #[cfg(feature = "cross")]
    with_pool(4, |mut p1: Pool<'_, i32>| with_pool(4, |p2: Pool<'_, i32>| {
        let a = p1.put(1);
        *p2.get(a) // handle from p1 used on p2
    }));
    #[cfg(feature = "escape")]
    let _leaked = with_pool(4, |mut p: Pool<'_, i32>| p.put(1)); // handle outlives pool
}
