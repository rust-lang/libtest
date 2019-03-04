#!/usr/bin/env sh

set -ex

: "${TARGET?The TARGET environment variable must be set.}"

cargo test -vv --all --target="${TARGET}"
cargo test -vv --all --target="${TARGET}" --release
