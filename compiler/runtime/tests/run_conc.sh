#!/bin/sh
# Test the oracle prelude's tasks and channels: prelude + conc_body.rs.
set -e
d=$(cd "$(dirname "$0")" && pwd)
t=$(mktemp -d)
cat "$d/../prelude.rs" "$d/conc_body.rs" > "$t/conc.rs"
rustc --edition 2024 -O "$t/conc.rs" -o "$t/conc"
"$t/conc"
