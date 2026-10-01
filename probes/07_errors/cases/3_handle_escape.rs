use alx_rt::{with_pool, BPool, Handle};

pub struct City { pub name: String }

pub fn founding() {
    let _h = with_pool(4, |mut cities: BPool<'_, City>| { // @6:3 pool=cities
        cities.put(City { name: String::from("Ashby") }) // @7:3 handle=_ pool=cities escapes=return
    });
}

// Unused: keeps the import honest.
pub fn _t<'a>(_: Handle<'a, City>) {}
