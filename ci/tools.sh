#!/usr/bin/env sh

set -ex

# rustfmt
if rustup component add rustfmt-preview ; then
    cargo-fmt --version
    cargo fmt --all -- --check
fi

# clippy
if rustup component add clippy-preview ; then
    cargo-clippy --version
    cargo clippy --all -- -D clippy::pedantic
fi

# sh-check
if command -v shellcheck ; then
    shellcheck --version
    shellcheck ci/*.sh
fi
