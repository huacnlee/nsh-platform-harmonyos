//! Device tests use NSH's public runner and agent, with hdc as the native driver.

mod device;
mod host;
mod native;
mod report;

use anyhow::{Context, bail};
use nsh_testing::{HostTarget, Platform, TestRun};
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Options {
    pub(crate) filter: Option<String>,
    pub(crate) trace: bool,
    pub(crate) junit: Option<PathBuf>,
}

pub(crate) struct Runner<'a> {
    options: &'a crate::Options,
    plan: Plan,
}

impl<'a> Runner<'a> {
    pub(crate) fn new(options: &'a crate::Options) -> anyhow::Result<Self> {
        if !options.hap {
            bail!("test requires a HAP; --no-hap is for package");
        }
        if options.out.is_some() {
            bail!("--out is for package; test artifacts are under .nsh/harmonyos/test");
        }
        let plan = Plan::read(options.app.as_deref().unwrap_or(Path::new(".")))?;
        Ok(Self { options, plan })
    }

    pub(crate) fn run(self) -> anyhow::Result<()> {
        fs::create_dir_all(&self.plan.work)?;
        let app_lock = fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.plan.work.join("run.lock"))?;
        app_lock
            .try_lock()
            .context("another device test is already running for this app")?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = cancelled.clone();
        ctrlc::set_handler(move || {
            signal.store(true, Ordering::Release);
        })?;
        let deveco = crate::sdk::DevEco::find()
            .context("test needs DevEco Studio or hvigor to build a HAP")?;
        let hdc = deveco
            .hdc()
            .context("hdc was not found in PATH or the SDK")?;
        let device = device::Device::discover(
            hdc,
            self.options.device.as_deref(),
            self.plan.work.join("artifacts"),
            cancelled.clone(),
        )?;
        let project = self.plan.app.join(".nsh/harmonyos/project");
        let release = self.options.release.unwrap_or(false);
        let config = crate::package(&self.plan.app, &project, release, true, self.options)?;
        let hap = crate::hap(&deveco, &project, &config, self.options, release)?;
        if cancelled.load(Ordering::Acquire) {
            bail!("test run interrupted");
        }
        device.install(&hap.path).with_context(|| {
            if hap.signed {
                format!("installing test host {}", config.id)
            } else {
                format!(
                    "installing unsigned test host: if this device requires signing, open {} in DevEco Studio and configure signing for test bundle {}",
                    project.display(), config.id
                )
            }
        })?;
        let hosts = host::Hosts::new(
            device,
            config.id,
            self.plan.work.clone(),
            self.plan.app.clone(),
            crate::is_rust_app(&self.plan.app),
            self.options.verbose,
        );
        let sets = vec![nsh_base::registry()];
        let mut target = HostTarget::default();
        target.name = "harmonyos".into();
        target.platform = Platform::Android;
        target.sized = false;
        let run = TestRun {
            ui_sets: sets.clone(),
            compile: nsh_testing::compiler(sets),
            work_dir: self.plan.work.join("files"),
            filter: self.options.test.filter.clone(),
            trace: self
                .options
                .test
                .trace
                .then(|| Arc::new(Mutex::new(Box::new(io::stdout()) as _))),
            host: Some(hosts.starter()),
            target: Some(target),
            page_host: None,
        };
        let start = Instant::now();
        let mut report = report::Report::default();
        for file in &self.plan.files {
            let _ = fs::remove_file(self.plan.work.join("artifacts/last.png"));
            if cancelled.load(Ordering::Acquire) {
                report.error(file, "test run interrupted");
                break;
            }
            if let Err(error) = nsh_testing::run_file(&run, file, &mut |result| {
                report.record(file, result, &self.plan.work)
            }) {
                report.error(file, &error);
            }
        }
        if let Err(error) = hosts.finish() {
            report.error(&self.plan.app, &format!("test cleanup: {error:#}"));
        }
        if let Some(path) = &self.options.test.junit {
            report.write(path, start.elapsed())?;
        }
        report.finish(start.elapsed())
    }
}

struct Plan {
    app: PathBuf,
    work: PathBuf,
    files: Vec<PathBuf>,
}
impl Plan {
    fn read(given: &Path) -> anyhow::Result<Self> {
        let path = given
            .canonicalize()
            .with_context(|| format!("no test input {}", given.display()))?;
        let directory = if path.is_dir() {
            path.as_path()
        } else {
            path.parent().context("test file has no parent")?
        };
        let app = directory
            .ancestors()
            .find(|dir| dir.join("nsh.json").is_file())
            .context("test input is not inside an NSH app (no nsh.json)")?
            .to_path_buf();
        let work = app.join(".nsh/harmonyos/test");
        let mut files = Vec::new();
        if path.is_dir() {
            Self::discover(&path, &mut files)?;
        } else if is_test(&path) {
            files.push(path.clone());
        } else {
            let mut steps = String::new();
            io::stdin().read_to_string(&mut steps)?;
            fs::create_dir_all(work.join("files"))?;
            let file = work.join(format!("files/steps-{}.test.ts", std::process::id()));
            let entry =
                serde_json::to_string(path.to_str().context("test entry path must be UTF-8")?)?;
            fs::write(
                &file,
                format!(
                    "import {{ test, open, expect }} from 'nsh/test';\ntest('steps', async () => {{\nconst app = await open({entry});\n{steps}\n}});\n"
                ),
            )?;
            files.push(file);
        }
        files.sort();
        if files.is_empty() {
            bail!("no *.test.tsx or *.test.ts files in {}", path.display());
        }
        Ok(Self { app, work, files })
    }
    fn discover(directory: &Path, files: &mut Vec<PathBuf>) -> anyhow::Result<()> {
        let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let kind = entry.file_type()?;
            if kind.is_dir() {
                if ![
                    ".nsh",
                    ".git",
                    "node_modules",
                    "oh_modules",
                    "target",
                    "build",
                ]
                .iter()
                .any(|skip| entry.file_name() == *skip)
                {
                    Self::discover(&entry.path(), files)?;
                }
            } else if kind.is_file() && is_test(&entry.path()) {
                files.push(entry.path());
            }
        }
        Ok(())
    }
}

fn is_test(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.ends_with(".test.tsx") || s.ends_with(".test.ts"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_test_files_resolve_their_app_and_generated_files_are_skipped() {
        let root =
            std::env::temp_dir().join(format!("nsh-harmonyos-discovery-{}", std::process::id()));
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::create_dir_all(root.join(".nsh")).unwrap();
        fs::write(root.join("nsh.json"), "{}").unwrap();
        fs::write(root.join("tests/a.test.tsx"), "").unwrap();
        fs::write(root.join(".nsh/old.test.tsx"), "").unwrap();
        let plan = Plan::read(&root).unwrap();
        assert_eq!(
            plan.files,
            [root.join("tests/a.test.tsx").canonicalize().unwrap()]
        );
        assert_eq!(
            Plan::read(&root.join("tests/a.test.tsx")).unwrap().app,
            root.canonicalize().unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
