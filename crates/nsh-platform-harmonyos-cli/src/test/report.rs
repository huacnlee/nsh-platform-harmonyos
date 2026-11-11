//! Test results and JUnit XML, including file-level failures.

use nsh_testing::{TestResult, TestStatus};
use quick_xml::{
    Writer,
    events::{BytesDecl, Event},
};
#[cfg(test)]
use serde::Deserialize;
use serde::Serialize;
use std::{collections::BTreeMap, fs, path::Path, time::Duration};

#[derive(Default)]
pub(super) struct Report {
    suites: BTreeMap<String, Vec<Case>>,
}
struct Case {
    name: String,
    elapsed: Duration,
    status: TestStatus,
    failure: Option<String>,
    error: bool,
}

impl Report {
    pub(super) fn record(&mut self, file: &Path, result: &TestResult, work: &Path) {
        let failure = result.failure.as_ref().map(|f| {
            let mut message = f.message.clone();
            if let Some(stack) = &f.stack {
                message.push('\n');
                message.push_str(stack);
            }
            if let Some(screen) = &f.screen {
                message.push_str("\nScreen:\n");
                message.push_str(&screen.join("\n"));
            }
            message
        });
        let symbol = match result.status {
            TestStatus::Passed => "✓",
            TestStatus::Failed => "✗",
            TestStatus::Skipped | TestStatus::Todo => "−",
        };
        println!("{symbol} {}: {}", file.display(), result.name);
        if let Some(message) = &failure {
            eprintln!("{message}");
            let path = work
                .join("artifacts")
                .join(format!("failure-{}.png", self.count()));
            if fs::copy(
                crate::paths::join_all(work, ["artifacts", "last.png"]),
                &path,
            )
            .is_ok()
            {
                eprintln!("screenshot: {}", path.display());
            }
        }
        self.suites
            .entry(file.display().to_string())
            .or_default()
            .push(Case {
                name: result.name.clone(),
                elapsed: result.elapsed,
                status: result.status,
                failure,
                error: false,
            });
        let _ = fs::remove_file(crate::paths::join_all(work, ["artifacts", "last.png"]));
    }
    pub(super) fn error(&mut self, file: &Path, message: &str) {
        eprintln!("✗ {}: {message}", file.display());
        self.suites
            .entry(file.display().to_string())
            .or_default()
            .push(Case {
                name: "test file".into(),
                elapsed: Duration::ZERO,
                status: TestStatus::Failed,
                failure: Some(message.into()),
                error: true,
            });
    }
    fn count(&self) -> usize {
        self.suites.values().map(Vec::len).sum()
    }
    fn failed(&self) -> usize {
        self.suites
            .values()
            .flatten()
            .filter(|c| c.status == TestStatus::Failed)
            .count()
    }
    pub(super) fn finish(&self, elapsed: Duration) -> anyhow::Result<()> {
        let total = self.count();
        let failed = self.failed();
        let passed = self
            .suites
            .values()
            .flatten()
            .filter(|c| c.status == TestStatus::Passed)
            .count();
        let skipped = total - passed - failed;
        let summary = format!(
            "{total} tests: {passed} passed, {failed} failed, {skipped} skipped ({:.2}s)",
            elapsed.as_secs_f64()
        );
        if failed > 0 {
            anyhow::bail!(summary);
        }
        println!("✓ {summary}");
        Ok(())
    }
    pub(super) fn write(&self, path: &Path, elapsed: Duration) -> anyhow::Result<()> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, self.xml(elapsed)?)?;
        Ok(())
    }
    fn xml(&self, elapsed: Duration) -> anyhow::Result<String> {
        let total = self.count();
        let errors = self.suites.values().flatten().filter(|c| c.error).count();
        let failures = self.failed() - errors;
        let skipped = self
            .suites
            .values()
            .flatten()
            .filter(|c| matches!(c.status, TestStatus::Skipped | TestStatus::Todo))
            .count();
        let testsuite = self
            .suites
            .iter()
            .map(|(file, cases)| {
                let file_name = xml_text(file);
                let suite_failures = cases
                    .iter()
                    .filter(|c| c.status == TestStatus::Failed && !c.error)
                    .count();
                let suite_errors = cases.iter().filter(|c| c.error).count();
                let suite_skipped = cases
                    .iter()
                    .filter(|c| matches!(c.status, TestStatus::Skipped | TestStatus::Todo))
                    .count();
                let suite_time: Duration = cases.iter().map(|c| c.elapsed).sum();
                let testcase = cases
                    .iter()
                    .map(|case| {
                        let detail = case.failure.as_deref().map(|raw| {
                            let body = xml_text(raw);
                            FailureTag {
                                message: body.lines().next().unwrap_or(&body).to_string(),
                                body,
                            }
                        });
                        let (failure, error) = if case.error {
                            (None, detail)
                        } else {
                            (detail, None)
                        };
                        let skipped = match case.status {
                            TestStatus::Skipped => Some(SkippedTag { message: None }),
                            TestStatus::Todo => Some(SkippedTag {
                                message: Some("todo".into()),
                            }),
                            TestStatus::Passed | TestStatus::Failed => None,
                        };
                        TestCase {
                            name: xml_text(&case.name),
                            classname: file_name.clone(),
                            time: format!("{:.6}", case.elapsed.as_secs_f64()),
                            failure,
                            error,
                            skipped,
                        }
                    })
                    .collect();
                TestSuite {
                    name: file_name,
                    tests: cases.len(),
                    failures: suite_failures,
                    errors: suite_errors,
                    skipped: suite_skipped,
                    time: format!("{:.6}", suite_time.as_secs_f64()),
                    testcase,
                }
            })
            .collect();
        let suites = TestSuites {
            name: "nsh test harmonyos".into(),
            tests: total,
            failures,
            errors,
            skipped,
            time: format!("{:.6}", elapsed.as_secs_f64()),
            testsuite,
        };

        let mut bytes = Vec::new();
        let mut writer = Writer::new_with_indent(&mut bytes, b' ', 2);
        writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;
        writer.write_serializable("testsuites", &suites)?;
        bytes.push(b'\n');
        Ok(String::from_utf8(bytes)?)
    }
}

#[derive(Serialize)]
#[cfg_attr(test, derive(Deserialize, Debug, PartialEq))]
struct TestSuites {
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "@tests")]
    tests: usize,
    #[serde(rename = "@failures")]
    failures: usize,
    #[serde(rename = "@errors")]
    errors: usize,
    #[serde(rename = "@skipped")]
    skipped: usize,
    #[serde(rename = "@time")]
    time: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    testsuite: Vec<TestSuite>,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(Deserialize, Debug, PartialEq))]
struct TestSuite {
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "@tests")]
    tests: usize,
    #[serde(rename = "@failures")]
    failures: usize,
    #[serde(rename = "@errors")]
    errors: usize,
    #[serde(rename = "@skipped")]
    skipped: usize,
    #[serde(rename = "@time")]
    time: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    testcase: Vec<TestCase>,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(Deserialize, Debug, PartialEq))]
struct TestCase {
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "@classname")]
    classname: String,
    #[serde(rename = "@time")]
    time: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failure: Option<FailureTag>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<FailureTag>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    skipped: Option<SkippedTag>,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(Deserialize, Debug, PartialEq))]
struct FailureTag {
    #[serde(rename = "@message")]
    message: String,
    #[serde(rename = "$text", default)]
    body: String,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(Deserialize, Debug, PartialEq))]
struct SkippedTag {
    #[serde(rename = "@message", default, skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

fn xml_text(text: &str) -> String {
    text.chars()
        .filter(|&c| {
            matches!(c, '\t' | '\n' | '\r')
                || (!c.is_control() && c != '\u{fffe}' && c != '\u{ffff}')
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nsh_testing::Failure;
    use quick_xml::{Reader, de::from_str};

    #[test]
    fn file_errors_are_junit_errors_and_xml_special_characters_roundtrip() {
        let mut report = Report::default();
        report.error(
            Path::new("a&b.test.ts"),
            "bad <file> \"quote\" 'apos'\u{1b}\nat load & eval",
        );
        let xml = report.xml(Duration::ZERO).unwrap();

        let mut reader = Reader::from_str(&xml);
        reader.config_mut().check_end_names = true;
        let mut saw_decl = false;
        loop {
            match reader.read_event().unwrap() {
                Event::Decl(decl) => {
                    assert_eq!(decl.version().unwrap().as_ref(), "1.0");
                    assert_eq!(decl.encoding().unwrap().unwrap().as_ref(), "UTF-8");
                    saw_decl = true;
                }
                Event::Eof => break,
                _ => {}
            }
        }
        assert!(saw_decl);

        let parsed: TestSuites = from_str(&xml).unwrap();
        assert_eq!(
            parsed,
            TestSuites {
                name: "nsh test harmonyos".into(),
                tests: 1,
                failures: 0,
                errors: 1,
                skipped: 0,
                time: "0.000000".into(),
                testsuite: vec![TestSuite {
                    name: "a&b.test.ts".into(),
                    tests: 1,
                    failures: 0,
                    errors: 1,
                    skipped: 0,
                    time: "0.000000".into(),
                    testcase: vec![TestCase {
                        name: "test file".into(),
                        classname: "a&b.test.ts".into(),
                        time: "0.000000".into(),
                        failure: None,
                        error: Some(FailureTag {
                            message: "bad <file> \"quote\" 'apos'".into(),
                            body: "bad <file> \"quote\" 'apos'\nat load & eval".into(),
                        }),
                        skipped: None,
                    }],
                }],
            }
        );
        assert!(report.finish(Duration::ZERO).is_err());
    }

    #[test]
    fn junit_report_serializes_passed_failed_skipped_and_todo_cases() {
        let work =
            std::env::temp_dir().join(format!("nsh-harmonyos-report-{}", std::process::id()));
        let file = Path::new("suite.test.tsx");
        let mut report = Report::default();
        report.record(
            file,
            &TestResult::new(
                "passes",
                Duration::from_millis(250),
                TestStatus::Passed,
                None,
            ),
            &work,
        );
        report.record(
            file,
            &TestResult::new(
                "fails <View>",
                Duration::from_millis(500),
                TestStatus::Failed,
                Some(Failure {
                    message: "expected \"a&b\"".into(),
                    stack: Some("suite.test.tsx:10:5".into()),
                    screen: Some(vec!["<Button>".into()]),
                }),
            ),
            &work,
        );
        report.record(
            file,
            &TestResult::new("skips", Duration::ZERO, TestStatus::Skipped, None),
            &work,
        );
        report.record(
            file,
            &TestResult::new("later", Duration::ZERO, TestStatus::Todo, None),
            &work,
        );

        let junit_path = work.join("junit.xml");
        report.write(&junit_path, Duration::from_secs(1)).unwrap();
        let xml = fs::read_to_string(&junit_path).unwrap();
        fs::remove_dir_all(&work).unwrap();

        let parsed: TestSuites = from_str(&xml).unwrap();
        assert_eq!(parsed.tests, 4);
        assert_eq!(parsed.failures, 1);
        assert_eq!(parsed.errors, 0);
        assert_eq!(parsed.skipped, 2);
        assert_eq!(parsed.time, "1.000000");
        assert_eq!(parsed.testsuite.len(), 1);

        let suite = &parsed.testsuite[0];
        assert_eq!(suite.name, "suite.test.tsx");
        assert_eq!(suite.tests, 4);
        assert_eq!(suite.failures, 1);
        assert_eq!(suite.errors, 0);
        assert_eq!(suite.skipped, 2);
        assert_eq!(suite.time, "0.750000");

        assert_eq!(suite.testcase[0].name, "passes");
        assert_eq!(suite.testcase[0].failure, None);
        assert_eq!(suite.testcase[0].error, None);
        assert_eq!(suite.testcase[0].skipped, None);

        assert_eq!(suite.testcase[1].name, "fails <View>");
        assert_eq!(
            suite.testcase[1].failure,
            Some(FailureTag {
                message: "expected \"a&b\"".into(),
                body: "expected \"a&b\"\nsuite.test.tsx:10:5\nScreen:\n<Button>".into(),
            })
        );

        assert_eq!(
            suite.testcase[2].skipped,
            Some(SkippedTag { message: None })
        );
        assert_eq!(
            suite.testcase[3].skipped,
            Some(SkippedTag {
                message: Some("todo".into()),
            })
        );
    }
}
