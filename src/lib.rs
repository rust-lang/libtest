//! Rust's built-in unit-test and micro-benchmarking framework.
#![cfg_attr(
    feature = "unstable",
    feature(set_stdio, panic_unwind, termination_trait_lib, test,)
)]
#![deny(rust_2018_idioms)]
#![allow(
    clippy::pub_enum_variant_names,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

use getopts;

#[cfg(feature = "unstable")]
extern crate test;

#[cfg(any(unix, target_os = "cloudabi"))]
extern crate libc;

// FIXME(#54291): rustc and/or LLVM don't yet support building with panic-unwind
//                on aarch64-pc-windows-msvc, so we don't link libtest against
//                libunwind (for the time being), even though it means that
//                libtest won't be fully functional on this platform.
//
// See also: https://github.com/rust-lang/rust/issues/54190#issuecomment-422904437
#[cfg(all(features = "unstable", not(all(windows, target_arch = "aarch64"))))]
extern crate panic_unwind;

use std::{
    any::Any,
    borrow::Cow,
    cmp,
    collections::BTreeMap,
    env, fmt,
    fs::File,
    io::{self, prelude::*},
    panic::{catch_unwind, AssertUnwindSafe},
    path::PathBuf,
    process,
    sync::{
        mpsc::{channel, Sender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(feature = "unstable")]
use std::process::Termination;
use termcolor::ColorChoice;

const TEST_WARN_TIMEOUT_S: u64 = 60;
const QUIET_MODE_MAX_COLUMN: usize = 100; // insert a '\n' after 100 tests in quiet mode

mod formatters;
pub mod stats;

fn set_print(
    sink: Option<Box<dyn Write + Send>>,
) -> Option<Box<dyn Write + Send>> {
    #[cfg(feature = "unstable")]
    {
        io::set_print(sink)
    }
    #[cfg(not(feature = "unstable"))]
    {
        sink
    }
}

fn set_panic(
    sink: Option<Box<dyn Write + Send>>,
) -> Option<Box<dyn Write + Send>> {
    #[cfg(feature = "unstable")]
    {
        io::set_panic(sink)
    }
    #[cfg(not(feature = "unstable"))]
    {
        sink
    }
}

use crate::formatters::{
    JsonFormatter, OutputFormatter, PrettyFormatter, TerseFormatter,
};

/// Whether to execute tests concurrently or not
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Concurrent {
    Yes,
    No,
}

// The name of a test. By convention this follows the rules for rust
// paths; i.e., it should be a series of identifiers separated by double
// colons. This way if some test runner wants to arrange the tests
// hierarchically it may.

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum TestName {
    StaticTestName(&'static str),
    DynTestName(String),
    AlignedTestName(Cow<'static, str>, NamePadding),
}
impl TestName {
    fn as_slice(&self) -> &str {
        match *self {
            TestName::StaticTestName(s) => s,
            TestName::DynTestName(ref s) => s,
            TestName::AlignedTestName(ref s, _) => &*s,
        }
    }

    fn padding(&self) -> NamePadding {
        match self {
            TestName::AlignedTestName(_, p) => *p,
            _ => NamePadding::PadNone,
        }
    }

    fn with_padding(&self, padding: NamePadding) -> Self {
        let name: Cow<'static, str> = match self {
            TestName::StaticTestName(name) => Cow::Borrowed(name),
            TestName::DynTestName(name) => Cow::Owned(name.to_owned()),
            TestName::AlignedTestName(name, _) => name.clone(),
        };

        TestName::AlignedTestName(name, padding)
    }
}
impl fmt::Display for TestName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.as_slice(), f)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum NamePadding {
    PadNone,
    PadOnRight,
}

impl TestDesc {
    fn padded_name(&self, column_count: usize, align: NamePadding) -> String {
        let mut name = String::from(self.name.as_slice());
        let fill = column_count.saturating_sub(name.len());
        let pad = " ".repeat(fill);
        match align {
            NamePadding::PadNone => name,
            NamePadding::PadOnRight => {
                name.push_str(&pad);
                name
            }
        }
    }
}

/// Represents a benchmark function.
pub trait TDynBenchFn: Send {
    fn run(&self, harness: &mut Bencher);
}

// A function that runs a test. If the function returns successfully,
// the test succeeds; if the function panics then the test fails. We
// may need to come up with a more clever definition of test in order
// to support isolation of tests into threads.
pub enum TestFn {
    StaticTestFn(fn()),
    StaticBenchFn(fn(&mut Bencher)),
    DynTestFn(Box<dyn FnMut() + Send>),
    DynBenchFn(Box<dyn TDynBenchFn + 'static>),
}

impl TestFn {
    fn padding(&self) -> NamePadding {
        match *self {
            TestFn::StaticTestFn(..) | TestFn::DynTestFn(..) => {
                NamePadding::PadNone
            }
            TestFn::StaticBenchFn(..) | TestFn::DynBenchFn(..) => {
                NamePadding::PadOnRight
            }
        }
    }
}

impl fmt::Debug for TestFn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match *self {
            TestFn::StaticTestFn(..) => "StaticTestFn(..)",
            TestFn::StaticBenchFn(..) => "StaticBenchFn(..)",
            TestFn::DynTestFn(..) => "DynTestFn(..)",
            TestFn::DynBenchFn(..) => "DynBenchFn(..)",
        })
    }
}

/// Manager of the benchmarking runs.
///
/// This is fed into functions marked with `#[bench]` to allow for
/// set-up & tear-down before running a piece of code repeatedly via a
/// call to `iter`.
#[derive(Clone)]
pub struct Bencher {
    mode: BenchMode,
    summary: Option<stats::Summary>,
    pub bytes: u64,
}

#[derive(Clone, PartialEq, Eq)]
pub enum BenchMode {
    Auto,
    Single,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ShouldPanic {
    No,
    Yes,
    YesWithMessage(&'static str),
}

// The definition of a single test. A test runner will run a list of
// these.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TestDesc {
    pub name: TestName,
    pub ignore: bool,
    pub should_panic: ShouldPanic,
    pub allow_fail: bool,
}

#[derive(Debug)]
pub struct TestDescAndFn {
    pub desc: TestDesc,
    pub testfn: TestFn,
}

#[derive(Clone, PartialEq, Debug, Copy)]
pub struct Metric {
    value: f64,
    noise: f64,
}

impl Metric {
    pub fn new(value: f64, noise: f64) -> Self {
        Self { value, noise }
    }
}

/// In case we want to add other options as well, just add them in this struct.
#[derive(Copy, Clone, Debug, Default)]
pub struct Options {
    display_output: bool,
}

impl Options {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn display_output(mut self, display_output: bool) -> Self {
        self.display_output = display_output;
        self
    }
}

// The default console test runner. It accepts the command line
// arguments and a vector of test_descs.
pub fn test_main(
    args: &[String],
    tests: Vec<TestDescAndFn>,
    options: Options,
) {
    let mut opts = match parse_opts(args) {
        Some(Ok(o)) => o,
        Some(Err(msg)) => {
            eprintln!("error: {}", msg);
            process::exit(101);
        }
        None => return,
    };

    opts.options = options;
    if opts.list {
        if let Err(e) = list_tests_console(&opts, tests) {
            eprintln!("error: io error when listing tests: {:?}", e);
            process::exit(101);
        }
    } else {
        match run_tests_console(&opts, tests) {
            Ok(true) => {}
            Ok(false) => process::exit(101),
            Err(e) => {
                eprintln!("error: io error when listing tests: {:?}", e);
                process::exit(101);
            }
        }
    }
}

// A variant optimized for invocation with a static test vector.
// This will panic (intentionally) when fed any dynamic tests, because
// it is copying the static values out into a dynamic vector and cannot
// copy dynamic values. It is doing this because from this point on
// a Vec<TestDescAndFn> is used in order to effect ownership-transfer
// semantics into parallel test runners, which in turn requires a Vec<>
// rather than a &[].
pub fn test_main_static(tests: &[&TestDescAndFn]) {
    let args = env::args().collect::<Vec<_>>();
    let owned_tests = tests
        .iter()
        .map(|t| match t.testfn {
            TestFn::StaticTestFn(f) => TestDescAndFn {
                testfn: TestFn::StaticTestFn(f),
                desc: t.desc.clone(),
            },
            TestFn::StaticBenchFn(f) => TestDescAndFn {
                testfn: TestFn::StaticBenchFn(f),
                desc: t.desc.clone(),
            },
            _ => panic!("non-static tests passed to test::test_main_static"),
        })
        .collect();
    test_main(&args, owned_tests, Options::new())
}

/// Invoked when unit tests terminate. Should panic if the unit
/// Tests is considered a failure. By default, invokes `report()`
/// and checks for a `0` result.
#[cfg(feature = "unstable")]
pub fn assert_test_result<T: Termination>(result: T) {
    let code = result.report();
    if code != 0 {
        panic!(
            "the test returned a termination value with a non-zero status code ({}) \
                which indicates a failure (this most likely means your test returned \
                an `Err(_)` value)",
            code,
        );
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    Pretty,
    Terse,
    Json,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RunIgnored {
    Yes,
    No,
    Only,
}

#[derive(Debug)]
pub struct TestOpts {
    pub list: bool,
    pub filter: Option<String>,
    pub filter_exact: bool,
    pub exclude_should_panic: bool,
    pub run_ignored: RunIgnored,
    pub run_tests: bool,
    pub bench_benchmarks: bool,
    pub logfile: Option<PathBuf>,
    pub nocapture: bool,
    pub color: ColorChoice,
    pub format: OutputFormat,
    pub test_threads: Option<usize>,
    pub skip: Vec<String>,
    pub options: Options,
}

impl TestOpts {
    #[cfg(test)]
    fn new() -> TestOpts {
        TestOpts {
            list: false,
            filter: None,
            filter_exact: false,
            exclude_should_panic: false,
            run_ignored: RunIgnored::No,
            run_tests: false,
            bench_benchmarks: false,
            logfile: None,
            nocapture: false,
            color: ColorChoice::Auto,
            format: OutputFormat::Pretty,
            test_threads: None,
            skip: vec![],
            options: Options::new(),
        }
    }
}

/// Result of parsing the options.
pub type OptRes = Result<TestOpts, String>;

fn optgroups() -> getopts::Options {
    let mut opts = getopts::Options::new();
    opts.optflag("", "include-ignored", "Run ignored and not ignored tests")
        .optflag("", "ignored", "Run only ignored tests")
        .optflag("", "exclude-should-panic", "Excludes tests marked as should_panic")
        .optflag("", "test", "Run tests and not benchmarks")
        .optflag("", "bench", "Run benchmarks instead of tests")
        .optflag("", "list", "List all tests and benchmarks")
        .optflag("h", "help", "Display this message (longer with --help)")
        .optopt(
            "",
            "logfile",
            "Write logs to the specified file instead \
             of stdout",
            "PATH",
        )
        .optflag(
            "",
            "nocapture",
            "don't capture stdout/stderr of each \
             task, allow printing directly",
        )
        .optopt(
            "",
            "test-threads",
            "Number of threads used for running tests \
             in parallel",
            "n_threads",
        )
        .optmulti(
            "",
            "skip",
            "Skip tests whose names contain FILTER (this flag can \
             be used multiple times)",
            "FILTER",
        )
        .optflag(
            "q",
            "quiet",
            "Display one character per test instead of one line. \
             Alias to --format=terse",
        )
        .optflag(
            "",
            "exact",
            "Exactly match filters rather than by substring",
        )
        .optopt(
            "",
            "color",
            "Configure coloring of output:
            auto   = colorize if stdout is a tty and tests are run on serially (default);
            always = always colorize output;
            never  = never colorize output;",
            "auto|always|never",
        )
        .optopt(
            "",
            "format",
            "Configure formatting of output:
            pretty = Print verbose output;
            terse  = Display one character per test;
            json   = Output a json document",
            "pretty|terse|json",
        )
        .optopt(
            "Z",
            "",
            "Enable nightly-only flags:
            unstable-options = Allow use of experimental features",
            "unstable-options",
        );
    opts
}

fn usage(binary: &str, options: &getopts::Options) {
    let message = format!("Usage: {} [OPTIONS] [FILTER]", binary);
    println!(
        r#"{usage}

The FILTER string is tested against the name of all tests, and only those
tests whose names contain the filter are run.

By default, all tests are run in parallel. This can be altered with the
--test-threads flag or the RUST_TEST_THREADS environment variable when running
tests (set it to 1).

All tests have their standard output and standard error captured by default.
This can be overridden with the --nocapture flag or setting RUST_TEST_NOCAPTURE
environment variable to a value other than "0". Logging is not captured by default.

Test Attributes:

    #[test]        - Indicates a function is a test to be run. This function
                     takes no arguments.
    #[bench]       - Indicates a function is a benchmark to be run. This
                     function takes one argument (test::Bencher).
    #[should_panic] - This function (also labeled with #[test]) will only pass if
                     the code causes a panic (an assertion failure or panic!)
                     A message may be provided, which the failure string must
                     contain: #[should_panic(expected = "foo")].
    #[ignore]      - When applied to a function which is already attributed as a
                     test, then the test runner will ignore these tests during
                     normal test runs. Running with --ignored or --include-ignored will run
                     these tests."#,
        usage = options.usage(&message)
    );
}

// FIXME: Copied from libsyntax until linkage errors are resolved. Issue #47566
fn is_nightly() -> bool {
    // Whether this is a feature-staged build, i.e., on the beta or stable channel
    let disable_unstable_features =
        option_env!("CFG_DISABLE_UNSTABLE_FEATURES").is_some();
    // Whether we should enable unstable features for bootstrapping
    let bootstrap = env::var("RUSTC_BOOTSTRAP").is_ok();

    bootstrap || !disable_unstable_features
}

// Parses command line arguments into test options
pub fn parse_opts(args: &[String]) -> Option<OptRes> {
    let mut allow_unstable = false;
    let opts = optgroups();
    let args = args.get(1..).unwrap_or(args);
    let matches = match opts.parse(args) {
        Ok(m) => m,
        Err(f) => return Some(Err(f.to_string())),
    };

    if let Some(opt) = matches.opt_str("Z") {
        if !is_nightly() {
            return Some(Err(
                "the option `Z` is only accepted on the nightly compiler"
                    .into(),
            ));
        }

        if let "unstable-options" = &*opt {
            allow_unstable = true;
        } else {
            return Some(Err("Unrecognized option to `Z`".into()));
        }
    };

    if matches.opt_present("h") {
        usage(&args[0], &opts);
        return None;
    }

    let filter = if matches.free.is_empty() {
        None
    } else {
        Some(matches.free[0].clone())
    };

    let exclude_should_panic = matches.opt_present("exclude-should-panic");
    if !allow_unstable && exclude_should_panic {
        return Some(Err(
            "The \"exclude-should-panic\" flag is only accepted on the nightly compiler".into(),
        ));
    }

    let include_ignored = matches.opt_present("include-ignored");
    if !allow_unstable && include_ignored {
        return Some(Err(
            "The \"include-ignored\" flag is only accepted on the nightly compiler".into(),
        ));
    }

    let run_ignored = match (include_ignored, matches.opt_present("ignored")) {
        (true, true) => {
            return Some(Err(
                "the options --include-ignored and --ignored are mutually exclusive".into(),
            ));
        }
        (true, false) => RunIgnored::Yes,
        (false, true) => RunIgnored::Only,
        (false, false) => RunIgnored::No,
    };
    let quiet = matches.opt_present("quiet");
    let exact = matches.opt_present("exact");
    let list = matches.opt_present("list");

    let logfile = matches.opt_str("logfile");
    let logfile = logfile.map(|s| PathBuf::from(&s));

    let bench_benchmarks = matches.opt_present("bench");
    let run_tests = !bench_benchmarks || matches.opt_present("test");

    let mut nocapture = matches.opt_present("nocapture");
    if !nocapture {
        nocapture = match env::var("RUST_TEST_NOCAPTURE") {
            Ok(val) => &val != "0",
            Err(_) => false,
        };
    }

    let test_threads = match matches.opt_str("test-threads") {
        Some(n_str) => match n_str.parse::<usize>() {
            Ok(0) => {
                return Some(Err(
                    "argument for --test-threads must not be 0".to_string()
                ))
            }
            Ok(n) => Some(n),
            Err(e) => {
                return Some(Err(format!(
                    "argument for --test-threads must be a number > 0 \
                     (error: {})",
                    e
                )));
            }
        },
        None => None,
    };

    let color = match matches.opt_str("color").as_ref().map(|s| &**s) {
        Some("auto") | None => ColorChoice::Auto,
        Some("always") => ColorChoice::Always,
        Some("never") => ColorChoice::Never,

        Some(v) => {
            return Some(Err(format!(
                "argument for --color must be auto, always, or never (was \
                 {})",
                v
            )));
        }
    };

    let format = match matches.opt_str("format").as_ref().map(|s| &**s) {
        None if quiet => OutputFormat::Terse,
        Some("pretty") | None => OutputFormat::Pretty,
        Some("terse") => OutputFormat::Terse,
        Some("json") => {
            if !allow_unstable {
                return Some(Err(
                    "The \"json\" format is only accepted on the nightly compiler".into(),
                ));
            }
            OutputFormat::Json
        }

        Some(v) => {
            return Some(Err(format!(
                "argument for --format must be pretty, terse, or json (was \
                 {})",
                v
            )));
        }
    };

    let test_opts = TestOpts {
        list,
        filter,
        filter_exact: exact,
        exclude_should_panic,
        run_ignored,
        run_tests,
        bench_benchmarks,
        logfile,
        nocapture,
        color,
        format,
        test_threads,
        skip: matches.opt_strs("skip"),
        options: Options::new(),
    };

    Some(Ok(test_opts))
}

#[derive(Clone, PartialEq)]
pub struct BenchSamples {
    ns_iter_summ: stats::Summary,
    mb_s: usize,
}

#[derive(Clone, PartialEq)]
pub enum TestResult {
    TrOk,
    TrFailed,
    TrFailedMsg(String),
    TrIgnored,
    TrAllowedFail,
    TrBench(BenchSamples),
}

unsafe impl Send for TestResult {}

enum OutputLocation<T> {
    Pretty(termcolor::StandardStream),
    #[allow(dead_code)]
    Raw(T), // used in tests
}

impl<T: Write> Write for OutputLocation<T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match *self {
            OutputLocation::Pretty(ref mut term) => term.write(buf),
            OutputLocation::Raw(ref mut stdout) => stdout.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match *self {
            OutputLocation::Pretty(ref mut term) => term.flush(),
            OutputLocation::Raw(ref mut stdout) => stdout.flush(),
        }
    }
}

struct ConsoleTestState {
    log_out: Option<File>,
    total: usize,
    passed: usize,
    failed: usize,
    ignored: usize,
    allowed_fail: usize,
    filtered_out: usize,
    measured: usize,
    metrics: MetricMap,
    failures: Vec<(TestDesc, Vec<u8>)>,
    not_failures: Vec<(TestDesc, Vec<u8>)>,
    options: Options,
}

impl ConsoleTestState {
    pub fn new(opts: &TestOpts) -> io::Result<Self> {
        let log_out = match opts.logfile {
            Some(ref path) => Some(File::create(path)?),
            None => None,
        };

        Ok(Self {
            log_out,
            total: 0,
            passed: 0,
            failed: 0,
            ignored: 0,
            allowed_fail: 0,
            filtered_out: 0,
            measured: 0,
            metrics: MetricMap::new(),
            failures: Vec::new(),
            not_failures: Vec::new(),
            options: opts.options,
        })
    }

    pub fn write_log<S: AsRef<str>>(&mut self, msg: S) -> io::Result<()> {
        let msg = msg.as_ref();
        match self.log_out {
            None => Ok(()),
            Some(ref mut o) => o.write_all(msg.as_bytes()),
        }
    }

    pub fn write_log_result(
        &mut self,
        test: &TestDesc,
        result: &TestResult,
    ) -> io::Result<()> {
        self.write_log(format!(
            "{} {}\n",
            match *result {
                TestResult::TrOk => "ok".to_owned(),
                TestResult::TrFailed => "failed".to_owned(),
                TestResult::TrFailedMsg(ref msg) => format!("failed: {}", msg),
                TestResult::TrIgnored => "ignored".to_owned(),
                TestResult::TrAllowedFail => "failed (allowed)".to_owned(),
                TestResult::TrBench(ref bs) => fmt_bench_samples(bs),
            },
            test.name
        ))
    }

    fn current_test_count(&self) -> usize {
        self.passed
            + self.failed
            + self.ignored
            + self.measured
            + self.allowed_fail
    }
}

// Format a number with thousands separators
fn fmt_thousands_sep(mut n: usize, sep: char) -> String {
    use std::fmt::Write;
    let mut output = String::new();
    let mut trailing = false;
    for &pow in &[9, 6, 3, 0] {
        let base = 10_usize.pow(pow);
        if pow == 0 || trailing || n / base != 0 {
            if trailing {
                output.write_fmt(format_args!("{:03}", n / base)).unwrap();
            } else {
                output.write_fmt(format_args!("{}", n / base)).unwrap();
            }
            if pow != 0 {
                output.push(sep);
            }
            trailing = true;
        }
        n %= base;
    }

    output
}

pub fn fmt_bench_samples(bs: &BenchSamples) -> String {
    use std::fmt::Write;
    let mut output = String::new();

    let median = bs.ns_iter_summ.median as usize;
    let deviation = (bs.ns_iter_summ.max - bs.ns_iter_summ.min) as usize;

    output
        .write_fmt(format_args!(
            "{:>11} ns/iter (+/- {})",
            fmt_thousands_sep(median, ','),
            fmt_thousands_sep(deviation, ',')
        ))
        .unwrap();
    if bs.mb_s != 0 {
        output
            .write_fmt(format_args!(" = {} MB/s", bs.mb_s))
            .unwrap();
    }
    output
}

// List the tests to console, and optionally to logfile. Filters are honored.
pub fn list_tests_console(
    opts: &TestOpts,
    tests: Vec<TestDescAndFn>,
) -> io::Result<()> {
    fn plural(count: u32, s: &str) -> String {
        match count {
            1 => format!("{} {}", 1, s),
            n => format!("{} {}s", n, s),
        }
    }

    let mut output: OutputLocation<Vec<u8>> =
        OutputLocation::Pretty(termcolor::StandardStream::stdout(opts.color));

    let quiet = opts.format == OutputFormat::Terse;
    let mut st = ConsoleTestState::new(opts)?;

    let mut ntest = 0;
    let mut nbench = 0;

    for test in filter_tests(&opts, tests) {
        let TestDescAndFn {
            desc: TestDesc { name, .. },
            testfn,
        } = test;

        let fntype = match testfn {
            TestFn::StaticTestFn(..) | TestFn::DynTestFn(..) => {
                ntest += 1;
                "test"
            }
            TestFn::StaticBenchFn(..) | TestFn::DynBenchFn(..) => {
                nbench += 1;
                "benchmark"
            }
        };

        writeln!(output, "{}: {}", name, fntype)?;
        st.write_log(format!("{} {}\n", fntype, name))?;
    }

    if !quiet {
        if ntest != 0 || nbench != 0 {
            writeln!(output)?;
        }

        writeln!(
            output,
            "{}, {}",
            plural(ntest, "test"),
            plural(nbench, "benchmark")
        )?;
    }

    Ok(())
}

// A simple console test runner
pub fn run_tests_console(
    opts: &TestOpts,
    tests: Vec<TestDescAndFn>,
) -> io::Result<bool> {
    fn callback(
        event: &TestEvent,
        st: &mut ConsoleTestState,
        out: &mut dyn OutputFormatter,
    ) -> io::Result<()> {
        match (*event).clone() {
            TestEvent::TeFiltered(ref filtered_tests) => {
                st.total = filtered_tests.len();
                out.write_run_start(filtered_tests.len())
            }
            TestEvent::TeFilteredOut(filtered_out) => {
                st.filtered_out = filtered_out;
                Ok(())
            }
            TestEvent::TeWait(ref test) => out.write_test_start(test),
            TestEvent::TeTimeout(ref test) => out.write_timeout(test),
            TestEvent::TeResult(test, result, stdout) => {
                st.write_log_result(&test, &result)?;
                out.write_result(&test, &result, &*stdout)?;
                match result {
                    TestResult::TrOk => {
                        st.passed += 1;
                        st.not_failures.push((test, stdout));
                    }
                    TestResult::TrIgnored => st.ignored += 1,
                    TestResult::TrAllowedFail => st.allowed_fail += 1,
                    TestResult::TrBench(bs) => {
                        st.metrics.insert_metric(
                            test.name.as_slice(),
                            bs.ns_iter_summ.median,
                            bs.ns_iter_summ.max - bs.ns_iter_summ.min,
                        );
                        st.measured += 1
                    }
                    TestResult::TrFailed => {
                        st.failed += 1;
                        st.failures.push((test, stdout));
                    }
                    TestResult::TrFailedMsg(msg) => {
                        st.failed += 1;
                        let mut stdout = stdout;
                        stdout.extend_from_slice(
                            format!("note: {}", msg).as_bytes(),
                        );
                        st.failures.push((test, stdout));
                    }
                }
                Ok(())
            }
        }
    }

    fn len_if_padded(t: &TestDescAndFn) -> usize {
        match t.testfn.padding() {
            NamePadding::PadNone => 0,
            NamePadding::PadOnRight => t.desc.name.as_slice().len(),
        }
    }

    let output: OutputLocation<Vec<u8>> =
        OutputLocation::Pretty(termcolor::StandardStream::stdout(opts.color));
    let max_name_len = tests
        .iter()
        .max_by_key(|t| len_if_padded(*t))
        .map_or(0, |t| t.desc.name.as_slice().len());

    let is_multithreaded =
        opts.test_threads.unwrap_or_else(get_concurrency) > 1;

    let mut out: Box<dyn OutputFormatter> = match opts.format {
        OutputFormat::Pretty => Box::new(PrettyFormatter::new(
            output,
            use_color(opts),
            max_name_len,
            is_multithreaded,
        )),
        OutputFormat::Terse => Box::new(TerseFormatter::new(
            output,
            use_color(opts),
            max_name_len,
            is_multithreaded,
        )),
        OutputFormat::Json => Box::new(JsonFormatter::new(output)),
    };
    let mut st = ConsoleTestState::new(opts)?;

    run_tests(opts, tests, |x| callback(&x, &mut st, &mut *out))?;

    assert!(st.current_test_count() == st.total);

    out.write_run_finish(&st)
}

#[test]
fn should_sort_failures_before_printing_them() {
    let test_a = TestDesc {
        name: TestName::StaticTestName("a"),
        ignore: false,
        should_panic: ShouldPanic::No,
        allow_fail: false,
    };

    let test_b = TestDesc {
        name: TestName::StaticTestName("b"),
        ignore: false,
        should_panic: ShouldPanic::No,
        allow_fail: false,
    };

    let mut out = PrettyFormatter::new(
        OutputLocation::Raw(Vec::new()),
        false,
        10,
        false,
    );

    let st = ConsoleTestState {
        log_out: None,
        total: 0,
        passed: 0,
        failed: 0,
        ignored: 0,
        allowed_fail: 0,
        filtered_out: 0,
        measured: 0,
        metrics: MetricMap::new(),
        failures: vec![(test_b, Vec::new()), (test_a, Vec::new())],
        options: Options::new(),
        not_failures: Vec::new(),
    };

    out.write_failures(&st).unwrap();
    let s = match out.output_location() {
        &OutputLocation::Raw(ref m) => String::from_utf8_lossy(&m[..]),
        &OutputLocation::Pretty(_) => unreachable!(),
    };

    let apos = s.find("a").unwrap();
    let bpos = s.find("b").unwrap();
    assert!(apos < bpos);
}

fn use_color(opts: &TestOpts) -> bool {
    match opts.color {
        ColorChoice::Auto => !opts.nocapture && stdout_isatty(),
        ColorChoice::Always | ColorChoice::AlwaysAnsi => true,
        ColorChoice::Never => false,
    }
}

#[cfg(any(
    target_os = "cloudabi",
    target_os = "redox",
    all(target_arch = "wasm32", not(target_os = "emscripten")),
    all(target_vendor = "fortanix", target_env = "sgx")
))]
fn stdout_isatty() -> bool {
    // FIXME: Implement isatty on Redox and SGX
    false
}
#[cfg(unix)]
fn stdout_isatty() -> bool {
    unsafe { libc::isatty(libc::STDOUT_FILENO) != 0 }
}
#[cfg(windows)]
fn stdout_isatty() -> bool {
    type DWORD = u32;
    type BOOL = i32;
    type HANDLE = *mut u8;
    type LPDWORD = *mut u32;
    const STD_OUTPUT_HANDLE: DWORD = -11i32 as DWORD;
    extern "system" {
        fn GetStdHandle(which: DWORD) -> HANDLE;
        fn GetConsoleMode(hConsoleHandle: HANDLE, lpMode: LPDWORD) -> BOOL;
    }
    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut out = 0;
        GetConsoleMode(handle, &mut out) != 0
    }
}

#[allow(clippy::large_enum_variant)] // FIXME
#[derive(Clone)]
pub enum TestEvent {
    TeFiltered(Vec<TestDesc>),
    TeWait(TestDesc),
    TeResult(TestDesc, TestResult, Vec<u8>),
    TeTimeout(TestDesc),
    TeFilteredOut(usize),
}

pub type MonitorMsg = (TestDesc, TestResult, Vec<u8>);

struct Sink(Arc<Mutex<Vec<u8>>>);
impl Write for Sink {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        Write::write(&mut *self.0.lock().unwrap(), data)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn run_tests<F>(
    opts: &TestOpts,
    tests: Vec<TestDescAndFn>,
    mut callback: F,
) -> io::Result<()>
where
    F: FnMut(TestEvent) -> io::Result<()>,
{
    use std::collections::{self, HashMap};
    use std::hash::BuildHasherDefault;
    use std::sync::mpsc::RecvTimeoutError;
    // Use a deterministic hasher
    type TestMap = HashMap<
        TestDesc,
        Instant,
        BuildHasherDefault<collections::hash_map::DefaultHasher>,
    >;
    fn get_timed_out_tests(running_tests: &mut TestMap) -> Vec<TestDesc> {
        let now = Instant::now();
        let timed_out = running_tests
            .iter()
            .filter_map(|(desc, timeout)| {
                if now >= *timeout {
                    Some(desc.clone())
                } else {
                    None
                }
            })
            .collect();
        for test in &timed_out {
            running_tests.remove(test);
        }
        timed_out
    };

    fn calc_timeout(running_tests: &TestMap) -> Option<Duration> {
        running_tests.values().min().map(|next_timeout| {
            let now = Instant::now();
            if *next_timeout >= now {
                *next_timeout - now
            } else {
                Duration::new(0, 0)
            }
        })
    };

    let tests_len = tests.len();

    let mut filtered_tests = filter_tests(opts, tests);
    if !opts.bench_benchmarks {
        filtered_tests = convert_benchmarks_to_tests(filtered_tests);
    }

    let filtered_tests = {
        let mut filtered_tests = filtered_tests;
        for test in &mut filtered_tests {
            test.desc.name =
                test.desc.name.with_padding(test.testfn.padding());
        }

        filtered_tests
    };

    let filtered_out = tests_len - filtered_tests.len();
    callback(TestEvent::TeFilteredOut(filtered_out))?;

    let filtered_descs =
        filtered_tests.iter().map(|t| t.desc.clone()).collect();

    callback(TestEvent::TeFiltered(filtered_descs))?;

    let (filtered_tests, filtered_benchs): (Vec<_>, _) =
        filtered_tests.into_iter().partition(|e| match e.testfn {
            TestFn::StaticTestFn(_) | TestFn::DynTestFn(_) => true,
            _ => false,
        });

    let concurrency = opts.test_threads.unwrap_or_else(get_concurrency);

    let mut remaining = filtered_tests;
    remaining.reverse();
    let mut pending = 0;

    let (tx, rx) = channel::<MonitorMsg>();

    let mut running_tests: TestMap = HashMap::default();

    if concurrency == 1 {
        while !remaining.is_empty() {
            let test = remaining.pop().unwrap();
            callback(TestEvent::TeWait(test.desc.clone()))?;
            run_test(opts, !opts.run_tests, test, tx.clone(), Concurrent::No);
            let (test, result, stdout) = rx.recv().unwrap();
            callback(TestEvent::TeResult(test, result, stdout))?;
        }
    } else {
        while pending > 0 || !remaining.is_empty() {
            while pending < concurrency && !remaining.is_empty() {
                let test = remaining.pop().unwrap();
                let timeout =
                    Instant::now() + Duration::from_secs(TEST_WARN_TIMEOUT_S);
                running_tests.insert(test.desc.clone(), timeout);
                callback(TestEvent::TeWait(test.desc.clone()))?; //here no pad
                run_test(
                    opts,
                    !opts.run_tests,
                    test,
                    tx.clone(),
                    Concurrent::Yes,
                );
                pending += 1;
            }

            let mut res;
            loop {
                if let Some(timeout) = calc_timeout(&running_tests) {
                    res = rx.recv_timeout(timeout);
                    for test in get_timed_out_tests(&mut running_tests) {
                        callback(TestEvent::TeTimeout(test))?;
                    }
                    if res != Err(RecvTimeoutError::Timeout) {
                        break;
                    }
                } else {
                    res =
                        rx.recv().map_err(|_| RecvTimeoutError::Disconnected);
                    break;
                }
            }

            let (desc, result, stdout) = res.unwrap();
            running_tests.remove(&desc);

            callback(TestEvent::TeResult(desc, result, stdout))?;
            pending -= 1;
        }
    }

    if opts.bench_benchmarks {
        // All benchmarks run at the end, in serial.
        for b in filtered_benchs {
            callback(TestEvent::TeWait(b.desc.clone()))?;
            run_test(opts, false, b, tx.clone(), Concurrent::No);
            let (test, result, stdout) = rx.recv().unwrap();
            callback(TestEvent::TeResult(test, result, stdout))?;
        }
    }
    Ok(())
}

#[allow(deprecated)]
fn get_concurrency() -> usize {
    #[cfg(windows)]
    #[allow(nonstandard_style)]
    fn num_cpus() -> usize {
        #[repr(C)]
        struct SYSTEM_INFO {
            wProcessorArchitecture: u16,
            wReserved: u16,
            dwPageSize: u32,
            lpMinimumApplicationAddress: *mut u8,
            lpMaximumApplicationAddress: *mut u8,
            dwActiveProcessorMask: *mut u8,
            dwNumberOfProcessors: u32,
            dwProcessorType: u32,
            dwAllocationGranularity: u32,
            wProcessorLevel: u16,
            wProcessorRevision: u16,
        }
        extern "system" {
            fn GetSystemInfo(info: *mut SYSTEM_INFO) -> i32;
        }
        unsafe {
            let mut sysinfo = std::mem::zeroed();
            GetSystemInfo(&mut sysinfo);
            sysinfo.dwNumberOfProcessors as usize
        }
    }

    #[cfg(target_os = "redox")]
    fn num_cpus() -> usize {
        // FIXME: Implement num_cpus on Redox
        1
    }

    #[cfg(any(
        all(target_arch = "wasm32", not(target_os = "emscripten")),
        all(target_vendor = "fortanix", target_env = "sgx")
    ))]
    fn num_cpus() -> usize {
        1
    }

    #[cfg(any(
        target_os = "android",
        target_os = "cloudabi",
        target_os = "emscripten",
        target_os = "fuchsia",
        target_os = "ios",
        target_os = "linux",
        target_os = "macos",
        target_os = "solaris"
    ))]
    fn num_cpus() -> usize {
        unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) as usize }
    }

    #[cfg(any(
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "bitrig",
        target_os = "netbsd"
    ))]
    fn num_cpus() -> usize {
        use std::ptr;

        let mut cpus: libc::c_uint = 0;
        let mut cpus_size = std::mem::size_of_val(&cpus);

        unsafe {
            cpus = libc::sysconf(libc::_SC_NPROCESSORS_ONLN) as libc::c_uint;
        }
        if cpus < 1 {
            let mut mib = [libc::CTL_HW, libc::HW_NCPU, 0, 0];
            unsafe {
                libc::sysctl(
                    mib.as_mut_ptr(),
                    2,
                    &mut cpus as *mut _ as *mut _,
                    &mut cpus_size as *mut _ as *mut _,
                    ptr::null_mut(),
                    0,
                );
            }
            if cpus < 1 {
                cpus = 1;
            }
        }
        cpus as usize
    }

    #[cfg(target_os = "openbsd")]
    fn num_cpus() -> usize {
        use std::ptr;

        let mut cpus: libc::c_uint = 0;
        let mut cpus_size = std::mem::size_of_val(&cpus);
        let mut mib = [libc::CTL_HW, libc::HW_NCPU, 0, 0];

        unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                &mut cpus as *mut _ as *mut _,
                &mut cpus_size as *mut _ as *mut _,
                ptr::null_mut(),
                0,
            );
        }
        if cpus < 1 {
            cpus = 1;
        }
        cpus as usize
    }

    #[cfg(target_os = "haiku")]
    fn num_cpus() -> usize {
        // FIXME: implement
        1
    }

    #[cfg(target_os = "l4re")]
    fn num_cpus() -> usize {
        // FIXME: implement
        1
    }

    match env::var("RUST_TEST_THREADS") {
        Ok(s) => {
            let opt_n: Option<usize> = s.parse().ok();
            match opt_n {
                Some(n) if n > 0 => n,
                _ => panic!(
                    "RUST_TEST_THREADS is `{}`, should be a positive integer.",
                    s
                ),
            }
        }
        Err(..) => num_cpus(),
    }
}

pub fn filter_tests(
    opts: &TestOpts,
    tests: Vec<TestDescAndFn>,
) -> Vec<TestDescAndFn> {
    let mut filtered = tests;
    let matches_filter = |test: &TestDescAndFn, filter: &str| {
        let test_name = test.desc.name.as_slice();

        if opts.filter_exact {
            test_name == filter
        } else {
            test_name.contains(filter)
        }
    };

    // Remove tests that don't match the test filter
    if let Some(ref filter) = opts.filter {
        filtered.retain(|test| matches_filter(test, filter));
    }

    // Skip tests that match any of the skip filters
    filtered
        .retain(|test| !opts.skip.iter().any(|sf| matches_filter(test, sf)));

    // Excludes #[should_panic] tests
    if opts.exclude_should_panic {
        filtered.retain(|test| test.desc.should_panic == ShouldPanic::No);
    }

    // maybe unignore tests
    match opts.run_ignored {
        RunIgnored::Yes => {
            filtered
                .iter_mut()
                .for_each(|test| test.desc.ignore = false);
        }
        RunIgnored::Only => {
            filtered.retain(|test| test.desc.ignore);
            filtered
                .iter_mut()
                .for_each(|test| test.desc.ignore = false);
        }
        RunIgnored::No => {}
    }

    // Sort the tests alphabetically
    filtered.sort_by(|t1, t2| {
        t1.desc.name.as_slice().cmp(t2.desc.name.as_slice())
    });

    filtered
}

pub fn convert_benchmarks_to_tests(
    tests: Vec<TestDescAndFn>,
) -> Vec<TestDescAndFn> {
    // convert benchmarks to tests, if we're not benchmarking them
    tests
        .into_iter()
        .map(|x| {
            let testfn = match x.testfn {
                TestFn::DynBenchFn(bench) => {
                    TestFn::DynTestFn(Box::new(move || {
                        bench::run_once(|b| {
                            __rust_begin_short_backtrace(|| bench.run(b))
                        })
                    }))
                }
                TestFn::StaticBenchFn(benchfn) => {
                    TestFn::DynTestFn(Box::new(move || {
                        bench::run_once(|b| {
                            __rust_begin_short_backtrace(|| benchfn(b))
                        })
                    }))
                }
                f => f,
            };
            TestDescAndFn {
                desc: x.desc,
                testfn,
            }
        })
        .collect()
}

#[allow(clippy::redundant_closure)]
pub fn run_test(
    opts: &TestOpts,
    force_ignore: bool,
    test: TestDescAndFn,
    monitor_ch: Sender<MonitorMsg>,
    concurrency: Concurrent,
) {
    fn run_test_inner(
        desc: TestDesc,
        monitor_ch: Sender<MonitorMsg>,
        nocapture: bool,
        mut testfn: Box<dyn FnMut() + Send>,
        concurrency: Concurrent,
    ) {
        // Buffer for capturing standard I/O
        let data = Arc::new(Mutex::new(Vec::new()));
        let data2 = data.clone();

        let name = desc.name.clone();
        let runtest = move || {
            let oldio = if nocapture {
                None
            } else {
                Some((
                    crate::set_print(Some(Box::new(Sink(data2.clone())))),
                    crate::set_panic(Some(Box::new(Sink(data2)))),
                ))
            };

            let result = catch_unwind(AssertUnwindSafe(move || testfn()));

            if let Some((printio, panicio)) = oldio {
                crate::set_print(printio);
                crate::set_panic(panicio);
            };

            let test_result = calc_result(&desc, result);
            let stdout = data.lock().unwrap().to_vec();
            monitor_ch
                .send((desc.clone(), test_result, stdout))
                .unwrap();
        };

        // If the platform is single-threaded we're just going to run
        // the test synchronously, regardless of the concurrency
        // level.
        let supports_threads =
            !cfg!(any(target_os = "emscripten", target_arch = "wasm32"));
        if concurrency == Concurrent::Yes && supports_threads {
            let cfg = thread::Builder::new().name(name.as_slice().to_owned());
            cfg.spawn(runtest).unwrap();
        } else {
            runtest();
        }
    }

    let TestDescAndFn { desc, testfn } = test;

    let ignore_because_panic_abort = cfg!(target_arch = "wasm32")
        && !cfg!(target_os = "emscripten")
        && desc.should_panic != ShouldPanic::No;

    if force_ignore || desc.ignore || ignore_because_panic_abort {
        monitor_ch
            .send((desc, TestResult::TrIgnored, Vec::new()))
            .unwrap();
        return;
    }

    match testfn {
        TestFn::DynBenchFn(bencher) => {
            crate::bench::benchmark(
                desc,
                &monitor_ch,
                opts.nocapture,
                |harness| bencher.run(harness),
            );
        }
        TestFn::StaticBenchFn(benchfn) => {
            crate::bench::benchmark(
                desc,
                &monitor_ch,
                opts.nocapture,
                |harness| (benchfn)(harness),
            );
        }
        TestFn::DynTestFn(mut f) => {
            let cb = move || __rust_begin_short_backtrace(|| f());
            run_test_inner(
                desc,
                monitor_ch,
                opts.nocapture,
                Box::new(cb),
                concurrency,
            )
        }
        TestFn::StaticTestFn(f) => run_test_inner(
            desc,
            monitor_ch,
            opts.nocapture,
            Box::new(move || __rust_begin_short_backtrace(f)),
            concurrency,
        ),
    }
}

/// Fixed frame used to clean the backtrace with `RUST_BACKTRACE=1`.
#[inline(never)]
fn __rust_begin_short_backtrace<F: FnMut()>(mut f: F) {
    f()
}

fn calc_result(
    desc: &TestDesc,
    task_result: Result<(), Box<dyn Any + Send>>,
) -> TestResult {
    match (&desc.should_panic, task_result) {
        (&ShouldPanic::No, Ok(())) | (&ShouldPanic::Yes, Err(_)) => {
            TestResult::TrOk
        }
        (&ShouldPanic::YesWithMessage(msg), Err(ref err)) => {
            if err
                .downcast_ref::<String>()
                .map(|e| &**e)
                .or_else(|| err.downcast_ref::<&'static str>().cloned())
                .map_or(false, |e| e.contains(msg))
            {
                TestResult::TrOk
            } else if desc.allow_fail {
                TestResult::TrAllowedFail
            } else {
                TestResult::TrFailedMsg(format!(
                    "Panic did not include expected string '{}'",
                    msg
                ))
            }
        }
        _ if desc.allow_fail => TestResult::TrAllowedFail,
        _ => TestResult::TrFailed,
    }
}

#[derive(Clone, PartialEq, Default)]
pub struct MetricMap(BTreeMap<String, Metric>);

impl MetricMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a named `value` (+/- `noise`) metric into the map. The value
    /// must be non-negative. The `noise` indicates the uncertainty of the
    /// metric, which doubles as the "noise range" of acceptable
    /// pairwise-regressions on this named value, when comparing from one
    /// metric to the next using `compare_to_old`.
    ///
    /// If `noise` is positive, then it means this metric is of a value
    /// you want to see grow smaller, so a change larger than `noise` in the
    /// positive direction represents a regression.
    ///
    /// If `noise` is negative, then it means this metric is of a value
    /// you want to see grow larger, so a change larger than `noise` in the
    /// negative direction represents a regression.
    pub fn insert_metric(&mut self, name: &str, value: f64, noise: f64) {
        let m = Metric { value, noise };
        self.0.insert(name.to_owned(), m);
    }

    pub fn fmt_metrics(&self) -> String {
        let v = self
            .0
            .iter()
            .map(|(k, v)| format!("{}: {} (+/- {})", *k, v.value, v.noise))
            .collect::<Vec<_>>();
        v.join(", ")
    }
}

// Benchmarking

impl Bencher {
    /// Callback for benchmark functions to run in their body.
    pub fn iter<T, F>(&mut self, mut inner: F)
    where
        F: FnMut() -> T,
    {
        if self.mode == BenchMode::Single {
            ns_iter_inner(&mut inner, 1);
            return;
        }

        self.summary = Some(iter(&mut inner));
    }

    pub fn bench<F>(&mut self, mut f: F) -> Option<stats::Summary>
    where
        F: FnMut(&mut Self),
    {
        f(self);
        self.summary
    }
}

fn ns_from_dur(dur: Duration) -> u64 {
    dur.as_secs() * 1_000_000_000 + u64::from(dur.subsec_nanos())
}

#[inline(never)]
#[allow(clippy::needless_pass_by_value)]
fn black_box<T>(x: T) -> T {
    #[cfg(feature = "unstable")]
    {
        test::black_box(x)
    }
    #[cfg(not(feature = "unstable"))]
    {
        unsafe { std::ptr::read_volatile(&x as *const T) }
    }
}

fn ns_iter_inner<T, F>(inner: &mut F, k: u64) -> u64
where
    F: FnMut() -> T,
{
    let start = Instant::now();
    for _ in 0..k {
        black_box(inner());
    }
    ns_from_dur(start.elapsed())
}

pub fn iter<T, F>(inner: &mut F) -> stats::Summary
where
    F: FnMut() -> T,
{
    // Initial bench run to get ballpark figure.
    let ns_single = ns_iter_inner(inner, 1);

    // Try to estimate iter count for 1ms falling back to 1m
    // iterations if first run took < 1ns.
    let ns_target_total = 1_000_000; // 1ms
    let mut n = ns_target_total / cmp::max(1, ns_single);

    // if the first run took more than 1ms we don't want to just
    // be left doing 0 iterations on every loop. The unfortunate
    // side effect of not being able to do as many runs is
    // automatically handled by the statistical analysis below
    // (i.e., larger error bars).
    n = cmp::max(1, n);

    let mut total_run = Duration::new(0, 0);
    let samples: &mut [f64] = &mut [0.0_f64; 50];
    loop {
        let loop_start = Instant::now();

        for p in &mut *samples {
            *p = ns_iter_inner(inner, n) as f64 / n as f64;
        }

        stats::winsorize(samples, 5.0);
        let summ = stats::Summary::new(samples);

        for p in &mut *samples {
            let ns = ns_iter_inner(inner, 5 * n);
            *p = ns as f64 / (5 * n) as f64;
        }

        stats::winsorize(samples, 5.0);
        let summ5 = stats::Summary::new(samples);

        let loop_run = loop_start.elapsed();

        // If we've run for 100ms and seem to have converged to a
        // stable median.
        if loop_run > Duration::from_millis(100)
            && summ.median_abs_dev_pct < 1.0
            && summ.median - summ5.median < summ5.median_abs_dev
        {
            return summ5;
        }

        total_run += loop_run;
        // Longest we ever run for is 3s.
        if total_run > Duration::from_secs(3) {
            return summ5;
        }

        // If we overflow here just return the results so far. We check a
        // multiplier of 10 because we're about to multiply by 2 and the
        // next iteration of the loop will also multiply by 5 (to calculate
        // the summ5 result)
        n = if n.checked_mul(10).is_some() {
            n * 2
        } else {
            return summ5;
        };
    }
}

pub mod bench {
    use super::{
        stats, BenchMode, BenchSamples, Bencher, MonitorMsg, Sender, Sink,
        TestDesc, TestResult,
    };
    use std::{
        cmp,
        panic::{catch_unwind, AssertUnwindSafe},
        sync::{Arc, Mutex},
    };

    pub fn benchmark<F>(
        desc: TestDesc,
        monitor_ch: &Sender<MonitorMsg>,
        nocapture: bool,
        f: F,
    ) where
        F: FnMut(&mut Bencher),
    {
        let mut bs = Bencher {
            mode: BenchMode::Auto,
            summary: None,
            bytes: 0,
        };

        let data = Arc::new(Mutex::new(Vec::new()));
        let data2 = data.clone();

        let oldio = if nocapture {
            None
        } else {
            Some((
                crate::set_print(Some(Box::new(Sink(data2.clone())))),
                crate::set_panic(Some(Box::new(Sink(data2)))),
            ))
        };

        let result = catch_unwind(AssertUnwindSafe(|| bs.bench(f)));

        if let Some((printio, panicio)) = oldio {
            crate::set_print(printio);
            crate::set_panic(panicio);
        };

        let test_result = match result {
            //bs.bench(f) {
            Ok(Some(ns_iter_summ)) => {
                let ns_iter = cmp::max(ns_iter_summ.median as u64, 1);
                let mb_s = bs.bytes * 1000 / ns_iter;

                let bs = BenchSamples {
                    ns_iter_summ,
                    mb_s: mb_s as usize,
                };
                TestResult::TrBench(bs)
            }
            Ok(None) => {
                // iter not called, so no data.
                // FIXME: error in this case?
                let samples: &mut [f64] = &mut [0.0_f64; 1];
                let bs = BenchSamples {
                    ns_iter_summ: stats::Summary::new(samples),
                    mb_s: 0,
                };
                TestResult::TrBench(bs)
            }
            Err(_) => TestResult::TrFailed,
        };

        let stdout = data.lock().unwrap().to_vec();
        monitor_ch.send((desc, test_result, stdout)).unwrap();
    }

    pub fn run_once<F>(f: F)
    where
        F: FnMut(&mut Bencher),
    {
        let mut bs = Bencher {
            mode: BenchMode::Single,
            summary: None,
            bytes: 0,
        };
        bs.bench(f);
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        bench, filter_tests, parse_opts, run_test, Bencher, Concurrent,
        MetricMap, RunIgnored, ShouldPanic, TestDesc, TestDescAndFn, TestFn,
        TestName, TestOpts, TestResult,
    };
    use std::sync::mpsc::channel;

    fn one_ignored_one_unignored_test() -> Vec<TestDescAndFn> {
        vec![
            TestDescAndFn {
                desc: TestDesc {
                    name: TestName::StaticTestName("1"),
                    ignore: true,
                    should_panic: ShouldPanic::No,
                    allow_fail: false,
                },
                testfn: TestFn::DynTestFn(Box::new(move || {})),
            },
            TestDescAndFn {
                desc: TestDesc {
                    name: TestName::StaticTestName("2"),
                    ignore: false,
                    should_panic: ShouldPanic::No,
                    allow_fail: false,
                },
                testfn: TestFn::DynTestFn(Box::new(move || {})),
            },
        ]
    }

    #[test]
    pub fn do_not_run_ignored_tests() {
        fn f() {
            panic!();
        }
        let desc = TestDescAndFn {
            desc: TestDesc {
                name: TestName::StaticTestName("whatever"),
                ignore: true,
                should_panic: ShouldPanic::No,
                allow_fail: false,
            },
            testfn: TestFn::DynTestFn(Box::new(f)),
        };
        let (tx, rx) = channel();
        run_test(&TestOpts::new(), false, desc, tx, Concurrent::No);
        let (_, res, _) = rx.recv().unwrap();
        assert!(res != TestResult::TrOk);
    }

    #[test]
    pub fn ignored_tests_result_in_ignored() {
        fn f() {}
        let desc = TestDescAndFn {
            desc: TestDesc {
                name: TestName::StaticTestName("whatever"),
                ignore: true,
                should_panic: ShouldPanic::No,
                allow_fail: false,
            },
            testfn: TestFn::DynTestFn(Box::new(f)),
        };
        let (tx, rx) = channel();
        run_test(&TestOpts::new(), false, desc, tx, Concurrent::No);
        let (_, res, _) = rx.recv().unwrap();
        assert!(res == TestResult::TrIgnored);
    }

    #[test]
    fn test_should_panic() {
        fn f() {
            panic!();
        }
        let desc = TestDescAndFn {
            desc: TestDesc {
                name: TestName::StaticTestName("whatever"),
                ignore: false,
                should_panic: ShouldPanic::Yes,
                allow_fail: false,
            },
            testfn: TestFn::DynTestFn(Box::new(f)),
        };
        let (tx, rx) = channel();
        run_test(&TestOpts::new(), false, desc, tx, Concurrent::No);
        let (_, res, _) = rx.recv().unwrap();
        assert!(res == TestResult::TrOk);
    }

    #[test]
    fn test_should_panic_good_message() {
        fn f() {
            panic!("an error message");
        }
        let desc = TestDescAndFn {
            desc: TestDesc {
                name: TestName::StaticTestName("whatever"),
                ignore: false,
                should_panic: ShouldPanic::YesWithMessage("error message"),
                allow_fail: false,
            },
            testfn: TestFn::DynTestFn(Box::new(f)),
        };
        let (tx, rx) = channel();
        run_test(&TestOpts::new(), false, desc, tx, Concurrent::No);
        let (_, res, _) = rx.recv().unwrap();
        assert!(res == TestResult::TrOk);
    }

    #[test]
    fn test_should_panic_bad_message() {
        fn f() {
            panic!("an error message");
        }
        let expected = "foobar";
        let failed_msg = "Panic did not include expected string";
        let desc = TestDescAndFn {
            desc: TestDesc {
                name: TestName::StaticTestName("whatever"),
                ignore: false,
                should_panic: ShouldPanic::YesWithMessage(expected),
                allow_fail: false,
            },
            testfn: TestFn::DynTestFn(Box::new(f)),
        };
        let (tx, rx) = channel();
        run_test(&TestOpts::new(), false, desc, tx, Concurrent::No);
        let (_, res, _) = rx.recv().unwrap();
        assert!(
            res == TestResult::TrFailedMsg(format!(
                "{} '{}'",
                failed_msg, expected
            ))
        );
    }

    #[test]
    fn test_should_panic_but_succeeds() {
        fn f() {}
        let desc = TestDescAndFn {
            desc: TestDesc {
                name: TestName::StaticTestName("whatever"),
                ignore: false,
                should_panic: ShouldPanic::Yes,
                allow_fail: false,
            },
            testfn: TestFn::DynTestFn(Box::new(f)),
        };
        let (tx, rx) = channel();
        run_test(&TestOpts::new(), false, desc, tx, Concurrent::No);
        let (_, res, _) = rx.recv().unwrap();
        assert!(res == TestResult::TrFailed);
    }

    #[test]
    fn parse_ignored_flag() {
        let args = vec![
            "progname".to_string(),
            "filter".to_string(),
            "--ignored".to_string(),
        ];
        let opts = parse_opts(&args).unwrap().unwrap();
        assert_eq!(opts.run_ignored, RunIgnored::Only);
    }

    #[test]
    fn parse_include_ignored_flag() {
        let args = vec![
            "progname".to_string(),
            "filter".to_string(),
            "-Zunstable-options".to_string(),
            "--include-ignored".to_string(),
        ];
        let opts = parse_opts(&args).unwrap().unwrap();
        assert_eq!(opts.run_ignored, RunIgnored::Yes);
    }

    #[test]
    pub fn filter_for_ignored_option() {
        // When we run ignored tests the test filter should filter out all the
        // unignored tests and flip the ignore flag on the rest to false

        let mut opts = TestOpts::new();
        opts.run_tests = true;
        opts.run_ignored = RunIgnored::Only;

        let tests = one_ignored_one_unignored_test();
        let filtered = filter_tests(&opts, tests);

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].desc.name.to_string(), "1");
        assert!(!filtered[0].desc.ignore);
    }

    #[test]
    pub fn run_include_ignored_option() {
        // When we "--include-ignored" tests, the ignore flag should be set to false on
        // all tests and no test filtered out

        let mut opts = TestOpts::new();
        opts.run_tests = true;
        opts.run_ignored = RunIgnored::Yes;

        let tests = one_ignored_one_unignored_test();
        let filtered = filter_tests(&opts, tests);

        assert_eq!(filtered.len(), 2);
        assert!(!filtered[0].desc.ignore);
        assert!(!filtered[1].desc.ignore);
    }

    #[test]
    pub fn exclude_should_panic_option() {
        let mut opts = TestOpts::new();
        opts.run_tests = true;
        opts.exclude_should_panic = true;

        let mut tests = one_ignored_one_unignored_test();
        tests.push(TestDescAndFn {
            desc: TestDesc {
                name: TestName::StaticTestName("3"),
                ignore: false,
                should_panic: ShouldPanic::Yes,
                allow_fail: false,
            },
            testfn: TestFn::DynTestFn(Box::new(move || {})),
        });

        let filtered = filter_tests(&opts, tests);

        assert_eq!(filtered.len(), 2);
        assert!(filtered
            .iter()
            .all(|test| test.desc.should_panic == ShouldPanic::No));
    }

    #[test]
    pub fn exact_filter_match() {
        fn tests() -> Vec<TestDescAndFn> {
            vec!["base", "base::test", "base::test1", "base::test2"]
                .into_iter()
                .map(|name| TestDescAndFn {
                    desc: TestDesc {
                        name: TestName::StaticTestName(name),
                        ignore: false,
                        should_panic: ShouldPanic::No,
                        allow_fail: false,
                    },
                    testfn: TestFn::DynTestFn(Box::new(move || {})),
                })
                .collect()
        }

        let substr = filter_tests(
            &TestOpts {
                filter: Some("base".into()),
                ..TestOpts::new()
            },
            tests(),
        );
        assert_eq!(substr.len(), 4);

        let substr = filter_tests(
            &TestOpts {
                filter: Some("bas".into()),
                ..TestOpts::new()
            },
            tests(),
        );
        assert_eq!(substr.len(), 4);

        let substr = filter_tests(
            &TestOpts {
                filter: Some("::test".into()),
                ..TestOpts::new()
            },
            tests(),
        );
        assert_eq!(substr.len(), 3);

        let substr = filter_tests(
            &TestOpts {
                filter: Some("base::test".into()),
                ..TestOpts::new()
            },
            tests(),
        );
        assert_eq!(substr.len(), 3);

        let exact = filter_tests(
            &TestOpts {
                filter: Some("base".into()),
                filter_exact: true,
                ..TestOpts::new()
            },
            tests(),
        );
        assert_eq!(exact.len(), 1);

        let exact = filter_tests(
            &TestOpts {
                filter: Some("bas".into()),
                filter_exact: true,
                ..TestOpts::new()
            },
            tests(),
        );
        assert_eq!(exact.len(), 0);

        let exact = filter_tests(
            &TestOpts {
                filter: Some("::test".into()),
                filter_exact: true,
                ..TestOpts::new()
            },
            tests(),
        );
        assert_eq!(exact.len(), 0);

        let exact = filter_tests(
            &TestOpts {
                filter: Some("base::test".into()),
                filter_exact: true,
                ..TestOpts::new()
            },
            tests(),
        );
        assert_eq!(exact.len(), 1);
    }

    #[test]
    pub fn sort_tests() {
        let mut opts = TestOpts::new();
        opts.run_tests = true;

        let names = vec![
            "sha1::test".to_string(),
            "isize::test_to_str".to_string(),
            "isize::test_pow".to_string(),
            "test::do_not_run_ignored_tests".to_string(),
            "test::ignored_tests_result_in_ignored".to_string(),
            "test::first_free_arg_should_be_a_filter".to_string(),
            "test::parse_ignored_flag".to_string(),
            "test::parse_include_ignored_flag".to_string(),
            "test::filter_for_ignored_option".to_string(),
            "test::run_include_ignored_option".to_string(),
            "test::sort_tests".to_string(),
        ];
        let tests = {
            fn testfn() {}
            let mut tests = Vec::new();
            for name in &names {
                let test = TestDescAndFn {
                    desc: TestDesc {
                        name: TestName::DynTestName((*name).clone()),
                        ignore: false,
                        should_panic: ShouldPanic::No,
                        allow_fail: false,
                    },
                    testfn: TestFn::DynTestFn(Box::new(testfn)),
                };
                tests.push(test);
            }
            tests
        };
        let filtered = filter_tests(&opts, tests);

        let expected = vec![
            "isize::test_pow".to_string(),
            "isize::test_to_str".to_string(),
            "sha1::test".to_string(),
            "test::do_not_run_ignored_tests".to_string(),
            "test::filter_for_ignored_option".to_string(),
            "test::first_free_arg_should_be_a_filter".to_string(),
            "test::ignored_tests_result_in_ignored".to_string(),
            "test::parse_ignored_flag".to_string(),
            "test::parse_include_ignored_flag".to_string(),
            "test::run_include_ignored_option".to_string(),
            "test::sort_tests".to_string(),
        ];

        for (a, b) in expected.iter().zip(filtered) {
            assert!(*a == b.desc.name.to_string());
        }
    }

    #[test]
    pub fn test_metricmap_compare() {
        let mut m1 = MetricMap::new();
        let mut m2 = MetricMap::new();
        m1.insert_metric("in-both-noise", 1000.0, 200.0);
        m2.insert_metric("in-both-noise", 1100.0, 200.0);

        m1.insert_metric("in-first-noise", 1000.0, 2.0);
        m2.insert_metric("in-second-noise", 1000.0, 2.0);

        m1.insert_metric("in-both-want-downwards-but-regressed", 1000.0, 10.0);
        m2.insert_metric("in-both-want-downwards-but-regressed", 2000.0, 10.0);

        m1.insert_metric("in-both-want-downwards-and-improved", 2000.0, 10.0);
        m2.insert_metric("in-both-want-downwards-and-improved", 1000.0, 10.0);

        m1.insert_metric("in-both-want-upwards-but-regressed", 2000.0, -10.0);
        m2.insert_metric("in-both-want-upwards-but-regressed", 1000.0, -10.0);

        m1.insert_metric("in-both-want-upwards-and-improved", 1000.0, -10.0);
        m2.insert_metric("in-both-want-upwards-and-improved", 2000.0, -10.0);
    }

    #[test]
    pub fn test_bench_once_no_iter() {
        fn f(_: &mut Bencher) {}
        bench::run_once(f);
    }

    #[test]
    pub fn test_bench_once_iter() {
        fn f(b: &mut Bencher) {
            b.iter(|| {})
        }
        bench::run_once(f);
    }

    #[test]
    pub fn test_bench_no_iter() {
        fn f(_: &mut Bencher) {}

        let (tx, rx) = channel();

        let desc = TestDesc {
            name: TestName::StaticTestName("f"),
            ignore: false,
            should_panic: ShouldPanic::No,
            allow_fail: false,
        };

        crate::bench::benchmark(desc, &tx, true, f);
        rx.recv().unwrap();
    }

    #[test]
    pub fn test_bench_iter() {
        fn f(b: &mut Bencher) {
            b.iter(|| {})
        }

        let (tx, rx) = channel();

        let desc = TestDesc {
            name: TestName::StaticTestName("f"),
            ignore: false,
            should_panic: ShouldPanic::No,
            allow_fail: false,
        };

        crate::bench::benchmark(desc, &tx, true, f);
        rx.recv().unwrap();
    }
}