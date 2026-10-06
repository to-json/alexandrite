#!/bin/sh
# Build the in-browser compiler: web/www/pkg/alx_web{.js,_bg.wasm}, and the
# threaded build in web/www/pkg/mt (shared memory and atomics, for
# cross-origin-isolated pages; needs nightly for -Z build-std).
set -e
cd "$(dirname "$0")/.."
bindgen="$(command -v wasm-bindgen || echo "$HOME/.cargo/bin/wasm-bindgen")"
cargo build --release --target wasm32-unknown-unknown -p alx-web
"$bindgen" --target web --no-typescript --out-dir web/www/pkg target/wasm32-unknown-unknown/release/alx_web.wasm
wasm-opt -O3 --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext --enable-mutable-globals web/www/pkg/alx_web_bg.wasm -o web/www/pkg/alx_web_bg.wasm

RUSTFLAGS='-C target-feature=+atomics,+bulk-memory,+mutable-globals -C link-arg=--shared-memory -C link-arg=--import-memory -C link-arg=--max-memory=4294967296 -C link-arg=--export=__wasm_init_tls -C link-arg=--export=__tls_size -C link-arg=--export=__tls_align -C link-arg=--export=__tls_base' \
  cargo +nightly build --release --target wasm32-unknown-unknown -p alx-web -Z build-std=std,panic_abort --target-dir target/mt
"$bindgen" --target web --no-typescript --out-dir web/www/pkg/mt target/mt/wasm32-unknown-unknown/release/alx_web.wasm
wasm-opt -O3 --enable-threads --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext --enable-mutable-globals web/www/pkg/mt/alx_web_bg.wasm -o web/www/pkg/mt/alx_web_bg.wasm

ls -la web/www/pkg web/www/pkg/mt
node web/gen-examples.mjs
