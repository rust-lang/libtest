#!/usr/bin/env sh

set -ex

# compile-test
rm -r compiletest-rs || true
git clone git@github.com:gnzlbg/compiletest-rs.git
(
    cd compiletest-rs
    git checkout libtest
    sed -i '' 's@libtest = { git = "https://github.com/gnzlbg/libtest", branch = "clippy_ci" }@libtest = { path = "../libtest" }@g' Cargo.toml
    echo "[workspace]" >> Cargo.toml
    cargo build
)
