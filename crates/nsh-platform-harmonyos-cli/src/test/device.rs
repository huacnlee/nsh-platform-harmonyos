//! Device ownership, bounded hdc commands and temporary remote artifacts.

use anyhow::{Context, bail};
use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

static NEXT: AtomicU64 = AtomicU64::new(0);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) struct Device {
    hdc: PathBuf,
    serial: String,
    _lock: File,
    pub(super) cancelled: Arc<AtomicBool>,
    artifacts: PathBuf,
    forwards: Mutex<BTreeSet<String>>,
}

impl Device {
    pub(super) fn discover(
        hdc: PathBuf,
        serial: Option<&str>,
        artifacts: PathBuf,
        cancelled: Arc<AtomicBool>,
    ) -> anyhow::Result<Arc<Self>> {
        let mut command = Command::new(&hdc);
        command.args(["list", "targets"]);
        let output = Commands::output(command, COMMAND_TIMEOUT, Some(&cancelled))?;
        let devices: Vec<_> = output
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.starts_with('['))
            .collect();
        let serial = match serial {
            Some(id) if devices.contains(&id) => id.to_owned(),
            Some(id) => bail!("hdc device {id:?} is not connected"),
            None if devices.len() == 1 => devices[0].to_owned(),
            None if devices.is_empty() => {
                bail!("no hdc device: connect a device or start an emulator")
            }
            None => bail!("multiple hdc devices: select one with --device ID"),
        };
        let key: String = serial
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let locks = std::env::temp_dir().join("nsh-harmonyos-device-locks");
        fs::create_dir_all(&locks)?;
        let mut lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(locks.join(key))?;
        lock.try_lock()
            .with_context(|| format!("hdc device {serial} is already used by another test run"))?;
        lock.set_len(0)?;
        writeln!(lock, "{}", std::process::id())?;
        fs::create_dir_all(&artifacts)?;
        let device = Arc::new(Self {
            hdc,
            serial,
            _lock: lock,
            cancelled,
            artifacts,
            forwards: Mutex::default(),
        });
        device.shell(&["uitest", "--version"])?;
        println!("• testing on hdc device {}", device.serial);
        Ok(device)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.hdc);
        command.args(["-t", &self.serial]);
        command
    }

    pub(super) fn run(
        &self,
        args: impl IntoIterator<Item = impl AsRef<OsStr>>,
    ) -> anyhow::Result<String> {
        let mut command = self.command();
        command.args(args);
        Commands::output(command, COMMAND_TIMEOUT, Some(&self.cancelled))
    }

    pub(super) fn shell(&self, args: &[&str]) -> anyhow::Result<String> {
        self.shell_inner(args, true)
    }

    fn shell_inner(&self, args: &[&str], cancellable: bool) -> anyhow::Result<String> {
        let script = args
            .iter()
            .map(|s| quote(s))
            .collect::<anyhow::Result<Vec<_>>>()?
            .join(" ");
        let script = format!(
            "{script}; nsh_test_status=$?; printf '\\n__NSH_TEST_EXIT__:%s\\n' \"$nsh_test_status\""
        );
        let mut command = self.command();
        command.args(["shell", &script]);
        let output = Commands::output(
            command,
            COMMAND_TIMEOUT,
            cancellable.then_some(&self.cancelled),
        )?;
        shell_result(&output)
    }

    pub(super) fn stop(&self, bundle: &str) {
        let _ = self.shell_inner(&["aa", "force-stop", bundle], false);
    }

    pub(super) fn release_touch(&self, x: i32, y: i32) {
        let _ = self.shell_inner(
            &["uinput", "-T", "-u", &x.to_string(), &y.to_string()],
            false,
        );
    }

    pub(super) fn install(&self, hap: &Path) -> anyhow::Result<()> {
        let output = self.run([OsStr::new("install"), OsStr::new("-r"), hap.as_os_str()])?;
        if !output.contains("install bundle successfully") {
            bail!("hdc did not install the test host: {}", output.trim());
        }
        Ok(())
    }

    pub(super) fn launch(&self, bundle: &str, port: u16) -> anyhow::Result<()> {
        self.shell(&[
            "aa",
            "start",
            "-a",
            "EntryAbility",
            "-b",
            bundle,
            "--ps",
            "nsh.devUrl",
            &format!("ws://127.0.0.1:{port}/ws"),
        ])?;
        Ok(())
    }

    pub(super) fn artifact(&self, extension: &str, command: &[&str]) -> anyhow::Result<Vec<u8>> {
        let nonce = format!(
            "{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let name = format!("nsh-test-{nonce}.{extension}");
        let remote = format!("/data/local/tmp/{name}");
        let local = self.artifacts.join(name);
        let result = (|| {
            let mut args = command.to_vec();
            args.extend(["-p", &remote]);
            self.shell(&args)?;
            self.run([
                OsStr::new("file"),
                OsStr::new("recv"),
                OsStr::new(&remote),
                local.as_os_str(),
            ])?;
            let file = File::open(&local).context("hdc wrote no artifact")?;
            if file.metadata()?.len() > 64 << 20 {
                bail!("device artifact exceeds 64 MiB");
            }
            let mut data = Vec::new();
            file.take((64 << 20) + 1).read_to_end(&mut data)?;
            if data.len() > 64 << 20 {
                bail!("device artifact exceeds 64 MiB");
            }
            Ok(data)
        })();
        let _ = fs::remove_file(local);
        let _ = self.shell_inner(&["rm", "-f", &remote], false);
        result
    }

    pub(super) fn cleanup_forwards(&self) -> anyhow::Result<()> {
        let mut forwards = self.forwards.lock().unwrap_or_else(|e| e.into_inner());
        let mut failure = None;
        forwards.retain(|node| match self.remove_forward(node) {
            Ok(()) => false,
            Err(error) => {
                failure.get_or_insert(error);
                true
            }
        });
        failure.map_or(Ok(()), Err)
    }

    fn remove_forward(&self, node: &str) -> anyhow::Result<()> {
        let mut command = self.command();
        command.args(["fport", "rm", node, node]);
        Commands::output(command, COMMAND_TIMEOUT, None)?;
        Ok(())
    }
}

pub(super) struct Forward {
    device: Arc<Device>,
    node: String,
}

impl Forward {
    pub(super) fn new(device: Arc<Device>, port: u16) -> anyhow::Result<Self> {
        let node = format!("tcp:{port}");
        let mut forwards = device.forwards.lock().unwrap_or_else(|e| e.into_inner());
        if device.cancelled.load(Ordering::Acquire) {
            bail!("test run interrupted");
        }
        // Finish creating the mapping so an interrupt cannot lose its ownership.
        let mut command = device.command();
        command.args(["rport", &node, &node]);
        Commands::output(command, COMMAND_TIMEOUT, None)?;
        forwards.insert(node.clone());
        drop(forwards);
        Ok(Self { device, node })
    }
}

impl Drop for Forward {
    fn drop(&mut self) {
        let mut forwards = self
            .device
            .forwards
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if forwards.contains(&self.node) {
            match self.device.remove_forward(&self.node) {
                Ok(()) => {
                    forwards.remove(&self.node);
                }
                Err(error) => eprintln!("could not remove test forward {}: {error:#}", self.node),
            }
        }
    }
}

struct Commands;

impl Commands {
    fn output(
        mut command: Command,
        timeout: Duration,
        cancelled: Option<&AtomicBool>,
    ) -> anyhow::Result<String> {
        if cancelled.is_some_and(|c| c.load(Ordering::Acquire)) {
            bail!("test run interrupted");
        }
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting hdc")?;
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let out = std::thread::spawn(move || Self::read(stdout));
        let err = std::thread::spawn(move || Self::read(stderr));
        let start = Instant::now();
        let mut stopped = None;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if start.elapsed() >= timeout || cancelled.is_some_and(|c| c.load(Ordering::Acquire)) {
                stopped = Some(if start.elapsed() >= timeout {
                    "hdc command timed out"
                } else {
                    "test run interrupted"
                });
                let _ = child.kill();
                break child.wait()?;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let output = format!(
            "{}{}",
            out.join()
                .map_err(|_| anyhow::anyhow!("hdc output reader panicked"))??,
            err.join()
                .map_err(|_| anyhow::anyhow!("hdc error reader panicked"))??
        );
        if let Some(reason) = stopped {
            bail!("{reason}: {output}");
        }
        if !status.success() || failed(&output) {
            bail!("hdc failed: {}", output.trim());
        }
        Ok(output)
    }

    fn read(mut reader: impl Read) -> std::io::Result<String> {
        let mut bytes = Vec::new();
        reader.by_ref().take(1 << 20).read_to_end(&mut bytes)?;
        std::io::copy(&mut reader, &mut std::io::sink())?;
        String::from_utf8(bytes)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }
}

fn quote(value: &str) -> anyhow::Result<String> {
    if value.contains('\0') {
        bail!("device arguments cannot contain NUL");
    }
    Ok(format!("'{}'", value.replace('\'', "'\\''")))
}

fn failed(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    let markers = [
        "[fail]",
        "[error]",
        "error:",
        "error code:",
        "failed to",
        "missing parameter",
        "unknown command",
        "unrecognized option",
        "connect server failed",
    ];
    lower.lines().any(|line| {
        markers
            .iter()
            .any(|needle| line.trim_start().starts_with(needle))
    })
}

fn shell_result(output: &str) -> anyhow::Result<String> {
    let (text, status) = output
        .rsplit_once("__NSH_TEST_EXIT__:")
        .context("hdc returned no remote exit status")?;
    let status: i32 = status
        .trim()
        .parse()
        .context("hdc returned an invalid remote exit status")?;
    if status != 0 {
        bail!("device command exited with {status}: {}", text.trim());
    }
    Ok(text.trim_end().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shell_arguments_preserve_unicode_quotes_and_shell_syntax() {
        assert_eq!(
            quote("a'中 $HOME; $(id)\n").unwrap(),
            "'a'\\''中 $HOME; $(id)\n'"
        );
        assert_eq!(quote("").unwrap(), "''");
        assert!(quote("a\0b").is_err());
    }
    #[test]
    fn zero_exit_status_is_not_enough_for_hdc() {
        assert!(failed("[Fail] target is offline"));
        assert!(failed("error: failed to start ability."));
        assert!(failed("Missing parameter.\nUSAGE:"));
        assert!(!failed("install bundle successfully.\n"));
        assert!(!failed("Input text: not found; error: quoted user text\n"));
        assert!(shell_result("output\n__NSH_TEST_EXIT__:127\n").is_err());
        assert!(shell_result("connection closed").is_err());
        assert_eq!(shell_result("ok\n__NSH_TEST_EXIT__:0\n").unwrap(), "ok");
    }
    #[test]
    #[cfg(unix)]
    fn commands_time_out_and_drain_large_output() {
        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 10"]);
        assert!(
            Commands::output(command, Duration::from_millis(30), None)
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
        let mut command = Command::new("sh");
        command.args(["-c", "head -c 2097152 /dev/zero"]);
        assert_eq!(
            Commands::output(command, Duration::from_secs(5), None)
                .unwrap()
                .len(),
            1 << 20
        );
    }
}
