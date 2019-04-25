use super::*;
use ::chrono::prelude::*;
use chrono::SecondsFormat;

pub(crate) struct JUnitFormatter<T> {
    out: OutputLocation<T>,
    results: Vec<(TestDesc, TestResult)>,
}

impl<T: Write> JUnitFormatter<T> {
    pub fn new(out: OutputLocation<T>) -> Self {
        Self {
            out,
            results: Vec::new(),
        }
    }

    fn write_message(&mut self, s: &str) -> io::Result<()> {
        assert!(!s.contains('\n'));

        self.out.write_all(s.as_ref())?;
        self.out.write_all(b"\n")
    }
}

impl<T: Write> OutputFormatter for JUnitFormatter<T> {
    fn write_run_start(&mut self, _test_count: usize) -> io::Result<()> {
        self.write_message(&"<?xml version=\"1.0\" encoding=\"UTF-8\"?>")
    }

    fn write_test_start(&mut self, _desc: &TestDesc) -> io::Result<()> {
        // We do not output anything on test start.
        Ok(())
    }

    fn write_timeout(&mut self, _desc: &TestDesc) -> io::Result<()> {
        Ok(())
    }

    fn write_result(
        &mut self,
        desc: &TestDesc,
        result: &TestResult,
        _stdout: &[u8],
    ) -> io::Result<()> {
        self.results.push((desc.clone(), result.clone()));
        Ok(())
    }

    fn write_run_finish(
        &mut self,
        state: &ConsoleTestState,
    ) -> io::Result<bool> {
        self.write_message("<?xml version=\"1.0\" encoding=\"UTF-8\"?>")?;
        self.write_message("<testsuites>")?;

        // JUnit expects time in the ISO8601, which was proposed in RFC 3339.
        let timestamp =
            Local::now().to_rfc3339_opts(SecondsFormat::Secs, false);
        let elapsed_time =
            state.start_time.elapsed().as_millis() as f32 / 1000.0;
        self.write_message(&*format!(
            "<testsuite name=\"test\" package=\"test\" id=\"0\" \
             hostname=\"localhost\" \
             errors=\"0\" \
             failures=\"{}\" \
             tests=\"{}\" \
             time=\"{}\" \
             timestamp=\"{}\">",
            state.failed, state.total, elapsed_time, timestamp
        ))?;
        for (desc, result) in std::mem::replace(&mut self.results, Vec::new())
        {
            match result {
                TestResult::TrFailed => {
                    self.write_message(&*format!(
                        "<testcase classname=\"test.global\" \
                         name=\"{}\" time=\"0\">",
                        desc.name.as_slice()
                    ))?;
                    self.write_message("<failure type=\"assert\"/>")?;
                    self.write_message("</testcase>")?;
                }

                TestResult::TrFailedMsg(ref m) => {
                    self.write_message(&*format!(
                        "<testcase classname=\"test.global\" \
                         name=\"{}\" time=\"0\">",
                        desc.name.as_slice()
                    ))?;
                    self.write_message(&*format!(
                        "<failure message=\"{}\" type=\"assert\"/>",
                        m
                    ))?;
                    self.write_message("</testcase>")?;
                }

                TestResult::TrBench(ref b) => {
                    self.write_message(&*format!(
                        "<testcase classname=\"test.global\" \
                         name=\"{}\" time=\"{}\" />",
                        desc.name.as_slice(),
                        b.ns_iter_summ.sum
                    ))?;
                }

                _ => {
                    self.write_message(&*format!(
                        "<testcase classname=\"test.global\" \
                         name=\"{}\" time=\"0\"/>",
                        desc.name.as_slice()
                    ))?;
                }
            }
        }
        self.write_message("<system-out/>")?;
        self.write_message("<system-err/>")?;
        self.write_message("</testsuite>")?;
        self.write_message("</testsuites>")?;

        Ok(state.failed == 0)
    }
}
