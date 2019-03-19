#!/usr/bin/env sh

set -ex

# compile-test
git clone git@github.com:laumann/compiletest-rs.git
(
    cd compiletest-rs
    sed -i '' 's@libtest = "0.0.1"@libtest = { path = ".." }@g' Cargo.toml
    cargo build
)
