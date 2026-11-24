#!/bin/sh
# Builds the wasm converter into docs/. Needs an activated emsdk (emcc on PATH)
# and `rustup target add wasm32-unknown-emscripten`.
set -eu
cargo build --release -p sketch2pdf-wasm --target wasm32-unknown-emscripten
cp target/wasm32-unknown-emscripten/release/sketch2pdf-wasm.js docs/sketch2pdf.js
cp target/wasm32-unknown-emscripten/release/sketch2pdf_wasm.wasm docs/
