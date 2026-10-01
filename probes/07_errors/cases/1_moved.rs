pub struct Order { pub id: i64, pub name: String }

pub fn ship(o: Order) -> i64 { o.id } // @6:1 def=ship sink=o

pub fn main() {
    let o = Order { id: 1, name: String::from("ann") }; // @9:3 bind=o
    ship(o); // @10:3 call=ship sink=o
    println!("{}", o.name); // @11:3 use=o
}
