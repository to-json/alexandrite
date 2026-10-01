use alx_rt::{with_pool, BPool};

pub struct City { pub name: String }

pub fn main() {
    with_pool(4, |mut cities: BPool<'_, City>| { // @6:3 pool=cities
        with_pool(4, |towns: BPool<'_, City>| { // @7:3 pool=towns
            let c = cities.put(City { name: String::from("Ashby") }); // @8:3 handle=c pool=cities
            println!("{}", towns.get(c).name); // @9:8 index=towns handle=c
        })
    })
}
