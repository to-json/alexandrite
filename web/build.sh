#!/bin/sh
# Build the in-browser compiler: web/www/pkg/alx_web{.js,_bg.wasm}.
set -e
cd "$(dirname "$0")/.."
cargo build --release --target wasm32-unknown-unknown -p alx-web
"$(command -v wasm-bindgen || echo "$HOME/.cargo/bin/wasm-bindgen")" --target web --no-typescript --out-dir web/www/pkg target/wasm32-unknown-unknown/release/alx_web.wasm
wasm-opt -O3 --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext --enable-mutable-globals web/www/pkg/alx_web_bg.wasm -o web/www/pkg/alx_web_bg.wasm
ls -la web/www/pkg
node web/gen-examples.mjs
