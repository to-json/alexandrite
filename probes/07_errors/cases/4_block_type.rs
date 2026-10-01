pub struct Order { pub id: i64, pub name: String }

pub fn ids(orders: &[Order]) -> Vec<i64> { // @6:1 def=ids returns=[Int]
    let mut out = Vec::with_capacity(orders.len()); // @7:3 inlined=map
    for it in orders { // @7:3 inlined=map block_param=it
        out.push(it.name.clone()); // @7:16 block_result=map
    }
    out // @7:3 inlined=map
}
