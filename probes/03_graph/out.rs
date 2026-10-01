//! Probe 03, hand-emitted. Cycles via handles. Removal uses tombstones; a
//! handle to a removed city must not silently point at something else.
//! Expected:
//!   reachable: Ashby Bree Crick Dunmore Eastfold
//!   hops a->e: 4
//!   after removing Dunmore: Ashby Bree Crick ; hops a->e: unreachable

use alx_rt::{Id, Pool};
use std::collections::{HashSet, VecDeque};

struct City {
    name: &'static str,
    roads: Vec<Id<Slot>>,
}

/// Removal leaves a tombstone so ids stay stable. If slots were ever reused,
/// ids would need a Vale-style generation check; here they never are.
enum Slot {
    Live(City),
    Dead,
}

struct Cities {
    pool: Pool<Slot>,
}

impl Cities {
    fn add(&mut self, name: &'static str) -> Id<Slot> {
        self.pool.put(Slot::Live(City { name, roads: Vec::new() }))
    }
    fn get(&self, id: Id<Slot>) -> Option<&City> {
        match self.pool.get(id) {
            Slot::Live(c) => Some(c),
            Slot::Dead => None,
        }
    }
    fn road(&mut self, from: Id<Slot>, to: Id<Slot>) {
        if let Slot::Live(c) = self.pool.get_mut(from) {
            c.roads.push(to);
        }
    }
    fn remove(&mut self, id: Id<Slot>) {
        *self.pool.get_mut(id) = Slot::Dead;
    }

    /// BFS from `start`; returns visit order and hop counts.
    fn bfs(&self, start: Id<Slot>) -> Vec<(Id<Slot>, u32)> {
        let mut seen = HashSet::from([start.index()]);
        let mut queue = VecDeque::from([(start, 0)]);
        let mut out = Vec::new();
        while let Some((id, d)) = queue.pop_front() {
            let Some(c) = self.get(id) else { continue }; // dangling road
            out.push((id, d));
            for &n in &c.roads {
                if seen.insert(n.index()) {
                    queue.push_back((n, d + 1));
                }
            }
        }
        out
    }

    fn reachable(&self, start: Id<Slot>) -> String {
        let names: Vec<_> = self.bfs(start).iter().map(|&(id, _)| self.get(id).unwrap().name).collect();
        names.join(" ")
    }

    fn hops(&self, from: Id<Slot>, to: Id<Slot>) -> Option<u32> {
        self.bfs(from).into_iter().find(|&(id, _)| id == to).map(|(_, d)| d)
    }
}

const CITIES: usize = 6;

fn main() {
    let mut g = Cities { pool: Pool::with_headroom(CITIES) };
    let a = g.add("Ashby");
    let b = g.add("Bree");
    let c = g.add("Crick");
    let d = g.add("Dunmore");
    let e = g.add("Eastfold");
    let f = g.add("Fallow");

    for (x, y) in [(a, b), (b, c), (c, a), (c, d), (d, e), (e, c), (f, a)] {
        g.road(x, y);
    }

    println!("reachable: {}", g.reachable(a));
    println!("hops a->e: {:?}", g.hops(a, e));

    g.remove(d);
    println!("after removing Dunmore: {}", g.reachable(a));
    match g.hops(a, e) {
        Some(h) => println!("hops a->e: {h}"),
        None => println!("hops a->e: unreachable"),
    }
    println!("pool len={} cap={}", g.pool.len(), g.pool.capacity());
    alx_rt::report("03");
}
