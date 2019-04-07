[![Build Status](https://travis-ci.com/rust-lang/libtest.svg?branch=master)](https://travis-ci.com/rust-lang/libtest) [![Build Status](https://dev.azure.com/rust-lang/libtest/_apis/build/status/libtest-CI?branchName=master)](https://dev.azure.com/rust-lang/libtest/_build/latest?definitionId=1&branchName=master) [![Latest Version]][crates.io] [![docs]][docs.rs]

libtest - Rust's built-in unit-testing and benchmarking framework
===

See [The Rust Programming Language chapter on
Testing](https://doc.rust-lang.org/book/ch11-00-testing.html).

## Cargo features

* `unstable` (disabled by default): enables nightly features. Currently, this enables:
   * `feature(set_stdio)`: better output reporting
   * `feature(panic_unwind)`: explicitly links against the `panic_unwind` crate
     on platforms that support it, but avoid that on platforms that do not. This
     allows using `libtest` on platforms like `aarch64-pc-windows-msvc` which do
     not yet support `panic_unwind`.
   * `feature(termination_trait_lib)`: exposes the `assert_test_result` API     
   * `feature(test)`: uses `test::black_box` in benchmarks. On stable Rust, this is
     worked around with volatile loads and stores which aren't as good.

## Platform support

* "build" shows whether the library compiles
* "run" shows whether the full test-suite passes

| Target                            | Build | Run |
|-----------------------------------|-------|-----|
| `aarch64-linux-android`           | ✓     | ✓   |
| `aarch64-unknown-linux-gnu`       | ✓     | ✓   |
| `arm-linux-androideabi`           | ✓     | ✓   |
| `arm-unknown-linux-gnueabi`       | ✓     | ✓   |
| `arm-unknown-linux-musleabi`      | ✓     | ✓   |
| `armv7-linux-androideabi`         | ✓     | ✓   |
| `armv7-unknown-linux-gnueabihf`   | ✓     | ✓   |
| `armv7-unknown-linux-musleabihf`  | ✓     | ✓   |
| `i586-unknown-linux-gnu`          | ✓     | ✓   |
| `i586-unknown-linux-musl`         | ✓     | ✓   |
| `i686-linux-android`              | ✓     | ✓   |
| `i686-pc-windows-gnu`             | ✓     | ✓   |
| `i686-apple-darwin`               | ✓     | ✓   |
| `i686-unknown-freebsd`            | ✓     | ✗   |
| `i686-unknown-linux-gnu`          | ✓     | ✓   |
| `i686-unknown-linux-musl`         | ✓     | ✓   |
| `mips-unknown-linux-gnu`          | ✓     | ✓   |
| `mips64-unknown-linux-gnuabi64`   | ✓     | ✓   |
| `mips64el-unknown-linux-gnuabi64` | ✓     | ✓   |
| `mipsel-unknown-linux-gnu`        | ✓     | ✓   |
| `powerpc-unknown-linux-gnu`       | ✓     | ✓   |
| `powerpc64-unknown-linux-gnu`     | ✓     | ✓   |
| `powerpc64le-unknown-linux-gnu`   | ✓     | ✓   |
| `sparc64-unknown-linux-gnu`       | ✓     | ✗   |
| `s390x-unknown-linux-gnu`         | ✓     | ✓   |
| `x86_64-apple-darwin`             | ✓     | ✓   |
| `x86_64-sun-solaris`              | ✓     | ✗   |
| `x86_64-linux-android`            | ✓     | ✓   |
| `x86_64-pc-windows-gnu`           | ✓     | ✓   |
| `x86_64-pc-windows-msvc`          | ✓     | ✓   |
| `x86_64-unknown-freebsd`          | ✓     | ✗   |
| `x86_64-unknown-linux-gnu`        | ✓     | ✓   |
| `x86_64-unknown-linux-musl`       | ✓     | ✓   |
| `x86_64-unknown-netbsd`           | ✓     | ✗   |

## License

This project is licensed under either of

* [Apache License, Version 2.0](http://www.apache.org/licenses/LICENSE-2.0)
  ([LICENSE-APACHE](LICENSE-APACHE))

* [MIT License](http://opensource.org/licenses/MIT)
  ([LICENSE-MIT](LICENSE-MIT))

at your option.

## Contributing

We welcome all people who want to contribute.

Contributions in any form (issues, pull requests, etc.) to this project
must adhere to Rust's [Code of Conduct].

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in `libtest` by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.

[Code of Conduct]: https://www.rust-lang.org/en-US/conduct.html
[Latest Version]: https://img.shields.io/crates/v/libtest.svg
[crates.io]: https://crates.io/crates/libtest
[docs]: https://docs.rs/libtest/badge.svg
[docs.rs]: https://docs.rs/libtest/
