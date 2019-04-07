#!/usr/bin/env sh

set -ex

# compile-test
rm -r compiletest-rs || true
git clone https://github.com/laumann/compiletest-rs
(
    cd compiletest-rs
    sed -i '' 's@libtest = "0.0.2"@libtest = { path = "..", features = [ "unstable" ] }@g' Cargo.toml
    echo "[workspace]" >> Cargo.toml
    cargo build
    cargo build --features=unstable
)
