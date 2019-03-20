#!/usr/bin/env sh

set -ex

: "${TARGET?The TARGET environment variable must be set.}"

CARGO="cargo"
if [ "${CROSS}" = "1" ]; then
    CARGO=cross
fi

CMD="test"
if [ "${NORUN}" = "1" ]; then
    CMD=build
fi

"${CARGO}" "${CMD}" -vv --all --target="${TARGET}"
"${CARGO}" "${CMD}" -vv --all --target="${TARGET}" --release

"${CARGO}" "${CMD}" -vv --all --target="${TARGET}" --features=unstable
"${CARGO}" "${CMD}" -vv --all --target="${TARGET}" --features=unstable --release
