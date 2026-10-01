pub const fn shout(s: &str, out: &mut String) { // @1:9 def=shout pure
    out.push_str(&s.to_uppercase()); // @1:37 call=upcase rust=str::to_uppercase
}
