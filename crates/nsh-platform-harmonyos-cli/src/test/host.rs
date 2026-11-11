//! One installed test host, with a new dev server and app state for each open.

use super::{
    device::{Device, Forward},
    native::HdcNative,
};
use anyhow::{Context, bail};
use nsh_dev::{
    AppInfo, ClientEvent, DevServer,
    protocol::{StepOutcome, TestStep},
};
use nsh_testing::{
    Host, HostOptions, StartHost,
    agent::{Agent, Native, RemoteScene},
};
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const ASK: Duration = Duration::from_secs(10);
const CONNECT: Duration = Duration::from_secs(20);

pub(super) struct Hosts {
    pub(super) device: Arc<Device>,
    pub(super) bundle: Arc<str>,
    pub(super) work: PathBuf,
    pub(super) app: PathBuf,
    pub(super) rust_root: bool,
    pub(super) verbose: bool,
    active: Arc<AtomicBool>,
    built: Mutex<BTreeMap<PathBuf, Arc<Build>>>,
}

impl Hosts {
    pub(super) fn new(
        device: Arc<Device>,
        bundle: String,
        work: PathBuf,
        app: PathBuf,
        rust_root: bool,
        verbose: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            device,
            bundle: bundle.into(),
            work,
            app,
            rust_root,
            verbose,
            active: Arc::default(),
            built: Mutex::default(),
        })
    }

    pub(super) fn starter(self: &Arc<Self>) -> StartHost {
        let hosts = self.clone();
        Arc::new(move |entry, options| {
            hosts
                .start(entry, options)
                .map(|h| Box::new(h) as Box<dyn Host>)
                .map_err(|e| format!("{e:#}"))
        })
    }

    pub(super) fn finish(&self) -> anyhow::Result<()> {
        // A timed-out JS test can leave open() or a native step in flight.
        // Stop those workers and retire every forward before the CLI exits.
        self.device.cancelled.store(true, Ordering::Release);
        self.device.stop(&self.bundle);
        self.device.cleanup_forwards()
    }

    fn start(&self, entry: &Path, options: &HostOptions) -> anyhow::Result<DeviceHost> {
        if self
            .active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            bail!(
                "one HarmonyOS app can be open per device; close the previous app before opening another"
            );
        }
        let slot = Slot(self.active.clone());
        let server = Arc::new(self.serve(entry, options)?);
        let port = server.local_addr().port();
        let forward = Forward::new(self.device.clone(), port)?;
        self.device.stop(&self.bundle);
        let running = Running {
            device: self.device.clone(),
            bundle: self.bundle.clone(),
        };
        self.device.launch(&self.bundle, port)?;
        let deadline = Instant::now() + CONNECT;
        while !server.has_tester() {
            if self.device.cancelled.load(Ordering::Acquire) {
                bail!("test run interrupted");
            }
            if Instant::now() >= deadline {
                bail!(
                    "the HarmonyOS test host did not connect in {} seconds",
                    CONNECT.as_secs()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let native = HdcNative::new(self.device.clone(), server.clone());
        loop {
            match native.refresh() {
                Ok(()) => break,
                Err(error)
                    if Instant::now() >= deadline
                        || self.device.cancelled.load(Ordering::Acquire) =>
                {
                    bail!("test host geometry: {error}")
                }
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        let asked = server.clone();
        let cancelled = self.device.cancelled.clone();
        let scene = Arc::new(
            RemoteScene::new(move |step| {
                if cancelled.load(Ordering::Acquire) {
                    return Err("test run interrupted".into());
                }
                asked.step(step, ASK)
            })
            .with_manual_clock(options.environment.clock.is_some()),
        );
        Ok(DeviceHost {
            agent: Agent::new(scene, Box::new(native.clone())),
            native,
            server,
            screenshot: crate::paths::join_all(&self.work, ["artifacts", "last.png"]),
            captured: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            _running: running,
            _forward: forward,
            _slot: slot,
        })
    }

    fn serve(&self, entry: &Path, options: &HostOptions) -> anyhow::Result<DevServer> {
        let trace = options.trace.clone();
        let verbose = self.verbose;
        // nsh build is an external compiler. Ask DevClient to negotiate source,
        // so its QuickJS bytecode version never has to match this runner's.
        let server = DevServer::start(
            "127.0.0.1:0".parse()?,
            "",
            Some(self.app.clone()),
            move |event| match event {
                ClientEvent::Trace { items } => {
                    if let Some(trace) = &trace {
                        trace(items);
                    }
                }
                ClientEvent::Log(log)
                    if verbose || log.level == nsh_dev::protocol::LogLevel::Error =>
                {
                    eprintln!("{}: {}", log.target, log.message);
                }
                _ => {}
            },
        )
        .map_err(anyhow::Error::msg)?;
        server.test_session();
        server.set_test_storage(options.storage.clone());
        server.set_test_trace(options.trace.is_some());
        server.set_test_environment(options.environment.clone());
        if let Some(network) = &options.network {
            server.set_test_network(network.clone());
        }
        let canonical = entry.canonicalize()?;
        let root =
            self.rust_root && (canonical == self.app || canonical == self.app.join("nsh.json"));
        let built = self.build(&canonical, root)?;
        server.serve_files(built.assets.clone());
        if root {
            server.publish_root();
        } else {
            server.publish(
                Vec::new(),
                built.source.clone(),
                built.map.clone(),
                AppInfo {
                    capabilities: built.capabilities.clone(),
                    id: built.id.clone(),
                    window: built.window.clone(),
                },
                None,
                built.ui_sets.clone(),
            );
        }
        Ok(server)
    }

    fn build(&self, entry: &Path, root: bool) -> anyhow::Result<Arc<Build>> {
        let mut cache = self.built.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(built) = cache.get(entry) {
            return Ok(built.clone());
        }
        let out = self.work.join("bundles").join(cache.len().to_string());
        let mut command = Command::new(crate::nsh());
        command.arg("build").arg(entry).arg("--out").arg(&out);
        if !self.verbose {
            command.stdout(std::process::Stdio::null());
        }
        if !command
            .status()
            .context("building the app with nsh")?
            .success()
        {
            bail!("nsh build failed for {}", entry.display());
        }
        let metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(out.join("app.json"))?)?;
        let ui_sets = metadata["uiSets"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_else(|| vec!["nsh/base".into()]);
        let built = Arc::new(Build {
            source: if root {
                String::new()
            } else {
                std::fs::read_to_string(out.join("bundle.js"))?
            },
            map: Build::optional_text(&out.join("bundle.js.map"))?,
            capabilities: Build::optional_text(&out.join("capabilities.json"))?,
            id: metadata["id"].as_str().map(str::to_owned),
            window: metadata.get("window").map(ToString::to_string),
            ui_sets,
            assets: Assets::collect(&out.join("assets"))?,
        });
        cache.insert(entry.to_path_buf(), built.clone());
        Ok(built)
    }
}

struct Build {
    source: String,
    map: Option<String>,
    capabilities: Option<String>,
    id: Option<String>,
    window: Option<String>,
    ui_sets: Vec<String>,
    assets: Vec<(String, PathBuf)>,
}

struct Assets;
impl Assets {
    fn collect(root: &Path) -> anyhow::Result<Vec<(String, PathBuf)>> {
        let mut files = Vec::new();
        if root.is_dir() {
            Self::walk(root, root, &mut files)?;
        }
        Ok(files)
    }
    fn walk(root: &Path, directory: &Path, out: &mut Vec<(String, PathBuf)>) -> anyhow::Result<()> {
        let mut entries = std::fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let kind = entry.file_type()?;
            if kind.is_dir() {
                Self::walk(root, &entry.path(), out)?;
            } else if kind.is_file() {
                out.push((
                    artifact_path(entry.path().strip_prefix(root)?)?,
                    entry.path(),
                ));
            }
        }
        Ok(())
    }
}

fn artifact_path(path: &Path) -> anyhow::Result<String> {
    path.components()
        .map(|part| match part {
            Component::Normal(name) => name
                .to_str()
                .map(str::to_owned)
                .context("asset paths must be UTF-8"),
            _ => bail!("asset path must stay inside assets/"),
        })
        .collect::<anyhow::Result<Vec<_>>>()
        .map(|parts| parts.join("/"))
}

impl Build {
    fn optional_text(path: &Path) -> anyhow::Result<Option<String>> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

struct Slot(Arc<AtomicBool>);
impl Drop for Slot {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
struct Running {
    device: Arc<Device>,
    bundle: Arc<str>,
}
impl Drop for Running {
    fn drop(&mut self) {
        self.device.stop(&self.bundle);
    }
}

struct DeviceHost {
    agent: Agent,
    native: HdcNative,
    server: Arc<DevServer>,
    screenshot: PathBuf,
    captured: AtomicBool,
    closed: AtomicBool,
    _running: Running,
    _forward: Forward,
    _slot: Slot,
}

impl Host for DeviceHost {
    fn step(&self, step: TestStep, _: Duration) -> Result<serde_json::Value, String> {
        if matches!(step, TestStep::Quit) {
            let result = self.server.step(step, Duration::from_secs(12));
            self.closed.store(result.is_ok(), Ordering::Release);
            return result;
        }
        let result = self.agent.take(step);
        if let Some(error) = self.native.take_error() {
            self.capture();
            return Err(error);
        }
        match result {
            StepOutcome::Done { value } => Ok(value),
            StepOutcome::Failed { message } => {
                self.capture();
                Err(message)
            }
        }
    }
}

impl DeviceHost {
    fn capture(&self) {
        if !self.captured.load(Ordering::Acquire)
            && let Ok(bytes) = self.native.screenshot()
            && std::fs::write(&self.screenshot, bytes).is_ok()
        {
            self.captured.store(true, Ordering::Release);
        }
    }
}

impl Drop for DeviceHost {
    fn drop(&mut self) {
        if !self.closed.load(Ordering::Acquire) {
            let _ = self.server.step(TestStep::Quit, Duration::from_secs(12));
        }
    }
}
