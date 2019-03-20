#!/usr/bin/env sh

set -ex

# compile-test
rm -r compiletest-rs || true
git clone git@github.com:laumann/compiletest-rs.git
(
    cd compiletest-rs
    sed -i '' 's@libtest = "0.0.1"@libtest = { path = "..", features = [ "unstable" ] }@g' Cargo.toml
    echo "[workspace]" >> Cargo.toml
    cargo build
)
