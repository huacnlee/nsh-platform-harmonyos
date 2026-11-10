//! `nsh-platform-harmonyos`: the `harmonyos` target of the nsh command line.
//! `nsh package --target harmonyos` (and `run`, `dev`, `test`) finds this
//! program on `PATH` and runs it with the command and its arguments, `NSH`
//! set to the nsh executable (NSH's external target protocol, its
//! `docs/dev/platforms.md`).
//!
//! `package` turns an NSH app into a DevEco Studio project:
//!
//! 1. `$NSH build <app>` bundles a TSX app, or describes a Rust app's crates
//!    and writes NSH's generated workspace, whose crate `rust-app`
//!    (`nsh_rust_app`) has the app's UI sets and root;
//! 2. a runner crate in `.nsh/harmonyos/runner` links this platform's
//!    backend (and the Rust app) into `libnsh_platform_harmonyos.so`, built
//!    with the OpenHarmony NDK ([`runner`], [`sdk`]);
//! 3. the project template, the ArkTS package and the library are written
//!    out ([`project`]); with DevEco Studio's tools (or hvigor from npm)
//!    installed, hvigor builds the HAP, an OpenHarmony one is signed with the
//!    SDK's debug keys when the project has no signature ([`sign`]), and
//!    `run` installs and starts it with `hdc`.

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/embedded.rs"));
}
mod project;
mod runner;
mod sdk;
mod sign;

use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

use anyhow::{Context as _, bail};

const USAGE: &str = "\
nsh-platform-harmonyos: run Native Shell apps on HarmonyOS and OpenHarmony

Usage (through nsh, which finds this program on PATH):
  nsh package [APP] --target harmonyos [--out DIR] [--runtime-os OS]
  nsh run [APP] --target harmonyos [--device ID] [--release]
  nsh dev [APP] --target harmonyos [--device ID] [--port PORT] [--release]

Commands:
  package   Build the app into a DevEco Studio project (default: .nsh/out/harmonyos),
            and its HAP when DevEco Studio's tools are installed
  run       Build it into .nsh/harmonyos/project, install the HAP on a device
            (hdc) and start it
  dev       Build it with the development client, install and start it on a device,
            forward the dev server port (`hdc rport`) and run `nsh dev --server-only`
  test      Not on this platform: `nsh test` runs the app's tests headless

Options:
  -o, --out DIR          Where `package` writes the project
      --runtime-os OS    harmonyos (default: Huawei's devices, AppGallery) or openharmony
      --device ID        The device `hdc` installs on (hdc list targets)
      --port PORT        Dev server port for `dev` (default: first free from 8790)
      --release          Build the native library optimized (`package` always does)
      --debug            Build it unoptimized (faster to build)
      --no-hap           Write the project only, without building the HAP
  -v, --verbose          Show cargo's and hvigor's output
  -h, --help             Show this help
  -V, --version          Show the version

Environment:
  OHOS_NDK_HOME          An OpenHarmony SDK's `openharmony` directory (found in DevEco Studio otherwise)
  DEVECO_STUDIO_HOME     DevEco Studio's `Contents` or the command-line tools' directory
  HVIGOR_HOME            Without DevEco: where @ohos/hvigor is installed from npm (run by `node`)
  OHOS_BASE_SDK_HOME     Without DevEco: the OpenHarmony SDKs hvigor builds with (one directory per API)";

/// What a command was asked to do.
#[derive(Debug, Default, PartialEq)]
struct Options {
    command: String,
    app: Option<PathBuf>,
    out: Option<PathBuf>,
    runtime_os: project::RuntimeOs,
    device: Option<String>,
    port: Option<u16>,
    release: Option<bool>,
    hap: bool,
    verbose: bool,
}

fn parse(args: impl IntoIterator<Item = OsString>) -> anyhow::Result<Options> {
    let mut options = Options {
        hap: true,
        ..Options::default()
    };
    let mut args = args.into_iter().map(|a| a.to_string_lossy().into_owned());
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().with_context(|| format!("{name} needs a value"));
        match arg.as_str() {
            "-h" | "--help" => options.command = "help".into(),
            "-V" | "--version" => options.command = "version".into(),
            "-v" | "--verbose" => options.verbose = true,
            "-o" | "--out" => options.out = Some(value(&arg)?.into()),
            "--runtime-os" => options.runtime_os = project::RuntimeOs::parse(&value(&arg)?)?,
            "--device" => options.device = Some(value(&arg)?),
            "--port" => {
                options.port = Some(
                    value(&arg)?
                        .parse()
                        .context("--port must be a port number")?,
                );
            }
            "--release" => options.release = Some(true),
            "--debug" => options.release = Some(false),
            "--no-hap" => options.hap = false,
            flag if flag.starts_with('-') => {
                bail!("{flag} is not an option of the harmonyos target\n\n{USAGE}")
            }
            _ if options.command.is_empty() => options.command = arg,
            _ if options.app.is_none() => options.app = Some(arg.into()),
            _ => bail!("unexpected argument {arg}\n\n{USAGE}"),
        }
    }
    Ok(options)
}

fn main() -> ExitCode {
    match run(parse(env::args_os().skip(1))) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(options: anyhow::Result<Options>) -> anyhow::Result<()> {
    let options = options?;
    match options.command.as_str() {
        "" | "help" => println!("{USAGE}"),
        "version" => println!("nsh-platform-harmonyos {}", env!("CARGO_PKG_VERSION")),
        "package" => {
            let app = app_dir(&options)?;
            let out = match &options.out {
                Some(out) => out.clone(),
                None => app.join(".nsh/out/harmonyos"),
            };
            let config = package(&app, &out, options.release.unwrap_or(true), false, &options)?;
            println!("✓ wrote the DevEco Studio project to {}", out.display());
            let mut signed = config.signed;
            if options.hap {
                match sdk::DevEco::find() {
                    Some(deveco) => {
                        let built = hap(&deveco, &out, &config, &options, true)?;
                        println!("✓ built {}", built.path.display());
                        signed = built.signed;
                    }
                    None => no_deveco(&out),
                }
            }
            if !signed {
                println!(
                    "  sign it before installing or publishing: open the project in DevEco Studio, \
                     File → Project Structure → Signing Configs (packaging again keeps them)"
                );
            }
        }
        "run" | "dev" => {
            let is_dev = options.command == "dev";
            let app = app_dir(&options)?;
            let out = app.join(".nsh/harmonyos/project");
            let release = options.release.unwrap_or(false);
            let config = package(&app, &out, release, is_dev, &options)?;
            let Some(deveco) = sdk::DevEco::find() else {
                no_deveco(&out);
                bail!(
                    "hvigor was not found (DevEco Studio's tools, or HVIGOR_HOME): it builds the HAP"
                );
            };
            let built = hap(&deveco, &out, &config, &options, release)?;
            if !built.signed {
                bail!(
                    "{} is not signed, and devices install signed HAPs only: open {} in DevEco \
                     Studio, sign in and let File → Project Structure → Signing Configs generate \
                     a signature, then run again (it is kept)",
                    built.path.display(),
                    out.display()
                );
            }
            if is_dev {
                let port = options.port.unwrap_or_else(free_dev_port);
                dev(
                    &deveco,
                    &app,
                    &built.path,
                    &config.id,
                    options.device.as_deref(),
                    port,
                    release,
                )?;
            } else {
                install(
                    &deveco,
                    &built.path,
                    &config.id,
                    options.device.as_deref(),
                    None,
                )?;
            }
        }
        "test" => bail!(
            "the harmonyos target runs no tests itself: `nsh test` runs the app's tests \
             headless, on NSH's engine, as this platform runs it"
        ),
        // Maintainers: regenerates the ArkTS declarations of the library's
        // N-API exports (`arkts/src/main/cpp/types/…/Index.d.ts`).
        "typedefs" => typedefs(options.verbose)?,
        other => bail!("{other} is not a command of the harmonyos target\n\n{USAGE}"),
    }
    Ok(())
}

/// The app's directory: `APP` (a directory or its entry file) or the
/// current one, with its `nsh.json`.
fn app_dir(options: &Options) -> anyhow::Result<PathBuf> {
    let given = options.app.clone().unwrap_or_else(|| ".".into());
    let dir = match given.is_file() {
        true => given.parent().map(Path::to_path_buf).unwrap_or_default(),
        false => given,
    };
    let dir = if dir.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        dir
    };
    if !dir.join("nsh.json").is_file() {
        bail!(
            "{} has no nsh.json: give the app's directory",
            dir.display()
        );
    }
    dir.canonicalize()
        .with_context(|| format!("no {}", dir.display()))
}

/// The nsh executable that ran this one (`NSH`), or the one on `PATH`.
fn nsh() -> OsString {
    env::var_os("NSH").unwrap_or_else(|| "nsh".into())
}

/// What `run` needs from a packaged project.
struct Packaged {
    id: String,
    /// Whether the project has a signing config (DevEco's).
    signed: bool,
}

fn package(
    app: &Path,
    out: &Path,
    release: bool,
    dev: bool,
    options: &Options,
) -> anyhow::Result<Packaged> {
    let config = project::AppConfig::read(app)?;
    let ndk = sdk::Ndk::find()?;
    let build = app.join(".nsh/harmonyos/build");
    // NSH bundles a TSX app, or writes its generated workspace with a Rust
    // app's root (`rust-app`).
    let mut nsh_build = Command::new(nsh());
    nsh_build.arg("build").arg(app).arg("--out").arg(&build);
    if release {
        nsh_build.arg("--release");
    }
    let status = nsh_build
        .status()
        .context("running nsh (set NSH or put nsh on PATH)")?;
    if !status.success() {
        bail!("nsh build failed");
    }
    let rust_app = app.join(".nsh/rust/rust-app");
    let is_rust = is_rust_app(app) && rust_app.join("Cargo.toml").is_file();
    let runner = app.join(".nsh/harmonyos/runner");
    runner::write(&runner, is_rust.then_some(rust_app.as_path()), dev)?;
    let library = runner::build(&runner, &ndk, release, options.verbose)?;
    project::write(
        out,
        &config,
        &project::Contents {
            library: &library,
            build: &build,
            runtime_os: options.runtime_os,
            compile_api: sdk::BaseSdk::find().map_or(project::MIN_API, |sdk| sdk.api),
        },
    )?;
    let profile = fs::read_to_string(out.join("build-profile.json5")).unwrap_or_default();
    Ok(Packaged {
        id: config.id,
        signed: profile.contains("\"signingConfig\":"),
    })
}

/// Whether the app's root is a Rust component (`nsh.json` `"root"`).
fn is_rust_app(app: &Path) -> bool {
    fs::read_to_string(app.join("nsh.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .is_some_and(|json| json["root"].is_string())
}

fn no_deveco(out: &Path) {
    println!(
        "  DevEco Studio's tools were not found (set DEVECO_STUDIO_HOME, or HVIGOR_HOME and \
         OHOS_BASE_SDK_HOME for hvigor from npm): open {} in DevEco Studio to build, sign and \
         run it",
        out.display()
    );
}

/// A HAP hvigor built.
struct Hap {
    path: PathBuf,
    signed: bool,
}

/// Builds the project's HAP with hvigor, and signs an OpenHarmony one the
/// project has no signature for with the SDK's debug keys.
fn hap(
    deveco: &sdk::DevEco,
    project: &Path,
    config: &Packaged,
    options: &Options,
    release: bool,
) -> anyhow::Result<Hap> {
    let verbose = options.verbose;
    let quiet = |cmd: &mut Command| {
        if !verbose {
            cmd.stdout(std::process::Stdio::null());
        }
    };
    if let sdk::Hvigor::Script { .. } = deveco.hvigor
        && let Some(base) = sdk::BaseSdk::find()
    {
        fs::write(
            project.join("local.properties"),
            format!(
                "sdk.dir={}\n",
                base.dir.display().to_string().replace('\\', "/")
            ),
        )?;
    }
    if deveco.ohpm.is_none() {
        link_packages(project)?;
    } else if let Some(ohpm) = &deveco.ohpm {
        println!("• installing the project's ArkTS packages (ohpm install)");
        let mut cmd = deveco.command(ohpm);
        cmd.arg("install").current_dir(project);
        quiet(&mut cmd);
        if !cmd.status().context("running ohpm")?.success() {
            bail!("ohpm install failed in {}", project.display());
        }
    }
    let mode = if release { "release" } else { "debug" };
    println!("• building the HAP (hvigor assembleHap, {mode})");
    let mut cmd = deveco.hvigor();
    cmd.args([
        "assembleHap",
        "--mode",
        "module",
        "-p",
        "product=default",
        "-p",
        &format!("buildMode={mode}"),
        "--no-daemon",
    ])
    .current_dir(project);
    quiet(&mut cmd);
    if !cmd.status().context("running hvigor")?.success() {
        bail!("hvigor could not build the HAP (run with -v to see why)");
    }
    let outputs = project.join("entry/build/default/outputs/default");
    let signed = outputs.join("entry-default-signed.hap");
    let unsigned = outputs.join("entry-default-unsigned.hap");
    if config.signed && signed.is_file() {
        return Ok(Hap {
            path: signed,
            signed: true,
        });
    }
    if !unsigned.is_file() {
        bail!("hvigor wrote no HAP in {}", outputs.display());
    }
    if options.runtime_os == project::RuntimeOs::OpenHarmony
        && let Some(lib) = sdk::signing_lib()
    {
        println!("• signing it with the OpenHarmony SDK's debug keys (hap-sign-tool)");
        sign::sign(
            &lib,
            &project.join("build/signing"),
            &config.id,
            &unsigned,
            &signed,
            verbose,
        )?;
        return Ok(Hap {
            path: signed,
            signed: true,
        });
    }
    Ok(Hap {
        path: unsigned,
        signed: false,
    })
}

/// What `ohpm install` would link without ohpm: the entry's dependencies on
/// the ArkTS package and the library's declarations, the package's on the
/// declarations.
fn link_packages(project: &Path) -> anyhow::Result<()> {
    let types = "src/main/cpp/types/libnsh_platform_harmonyos";
    for (link, target) in [
        ("entry/oh_modules/@nsh/harmonyos", "nsh".to_owned()),
        (
            "entry/oh_modules/libnsh_platform_harmonyos.so",
            format!("nsh/{types}"),
        ),
        (
            "nsh/oh_modules/libnsh_platform_harmonyos.so",
            format!("nsh/{types}"),
        ),
    ] {
        let link = project.join(link);
        if let Some(parent) = link.parent() {
            fs::create_dir_all(parent)?;
        }
        if fs::symlink_metadata(&link).is_ok() {
            match fs::remove_file(&link) {
                Ok(()) => {}
                Err(_) => fs::remove_dir_all(&link)?,
            }
        }
        let target = project.join(target);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link)
            .with_context(|| format!("linking {}", link.display()))?;
        #[cfg(not(unix))]
        copy_dir(&target, &link)?;
    }
    fs::create_dir_all(project.join("oh_modules"))?;
    Ok(())
}

#[cfg(not(unix))]
fn copy_dir(from: &Path, to: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Installs `hap` with `hdc` and starts its ability.
fn install(
    deveco: &sdk::DevEco,
    hap: &Path,
    bundle: &str,
    device: Option<&str>,
    dev_url: Option<&str>,
) -> anyhow::Result<()> {
    let hdc = deveco
        .hdc()
        .context("hdc was not found: it comes with DevEco Studio's SDK (toolchains)")?;
    let hdc_command = || {
        let mut cmd = Command::new(&hdc);
        if let Some(device) = device {
            cmd.args(["-t", device]);
        }
        cmd
    };
    println!("• installing on the device (hdc install)");
    let output = hdc_command()
        .args(["install", "-r"])
        .arg(hap)
        .output()
        .context("running hdc")?;
    let text = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || text.contains("fail") || text.contains("[Fail]") {
        bail!(
            "hdc could not install {}: {}{}",
            hap.display(),
            text.trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let mut start = hdc_command();
    start.args(["shell", "aa", "start", "-a", "EntryAbility", "-b", bundle]);
    if let Some(url) = dev_url {
        start.args(["--ps", "nsh.devUrl", url]);
    }
    let status = start.status().context("running hdc")?;
    if !status.success() {
        bail!("hdc could not start {bundle}");
    }
    println!("✓ {bundle} is running (hdc hilog shows its log)");
    Ok(())
}

fn free_dev_port() -> u16 {
    (8790..=8890)
        .find(|&port| std::net::TcpListener::bind(("127.0.0.1", port)).is_ok())
        .unwrap_or(8790)
}

/// Installs the development HAP, forwards `port` from the device to this
/// machine (`hdc rport`), launches the ability with `nsh.devUrl`, and runs
/// `nsh dev --server-only`.
fn dev(
    deveco: &sdk::DevEco,
    app: &Path,
    hap: &Path,
    bundle: &str,
    device: Option<&str>,
    port: u16,
    release: bool,
) -> anyhow::Result<()> {
    let hdc = deveco
        .hdc()
        .context("hdc was not found: it comes with DevEco Studio's SDK (toolchains)")?;
    let hdc_command = || {
        let mut cmd = Command::new(&hdc);
        if let Some(device) = device {
            cmd.args(["-t", device]);
        }
        cmd
    };
    let node = format!("tcp:{port}");
    let _ = hdc_command().args(["fport", "rm", &node, &node]).output();
    let rport = hdc_command()
        .args(["rport", &node, &node])
        .output()
        .context("running hdc rport")?;
    let rport_text = String::from_utf8_lossy(&rport.stdout);
    if !rport.status.success() || rport_text.contains("[Fail]") {
        bail!(
            "hdc could not forward port {port}: {}{}",
            rport_text.trim(),
            String::from_utf8_lossy(&rport.stderr).trim()
        );
    }
    let dev_url = format!("ws://127.0.0.1:{port}/ws");
    if let Err(err) = install(deveco, hap, bundle, device, Some(&dev_url)) {
        let _ = hdc_command().args(["fport", "rm", &node, &node]).output();
        return Err(err);
    }
    let mut server = Command::new(nsh());
    server
        .arg("dev")
        .arg(app)
        .arg("--server-only")
        .arg("--port")
        .arg(port.to_string());
    if release {
        server.arg("--release");
    }
    let status = server.status().context("running nsh dev --server-only");
    let _ = hdc_command().args(["fport", "rm", &node, &node]).output();
    let status = status?;
    if !status.success() {
        bail!("nsh dev exited with {status}");
    }
    Ok(())
}

/// Builds the backend once with N-API's type definitions on and writes
/// them as the ArkTS package's `Index.d.ts`.
fn typedefs(verbose: bool) -> anyhow::Result<()> {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .context("this repository")?
        .to_path_buf();
    let ndk = sdk::Ndk::find()?;
    let folder = repo.join("target/harmonyos-typedefs");
    if folder.exists() {
        fs::remove_dir_all(&folder)?;
    }
    fs::create_dir_all(&folder)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string();
    let mut cmd = ndk.cargo();
    cmd.args([
        "check",
        "-p",
        "nsh-platform-harmonyos",
        "--target",
        sdk::TRIPLE,
    ])
    .current_dir(&repo)
    .env("NAPI_TYPE_DEF_TMP_FOLDER", &folder)
    .env("NAPI_FORCE_BUILD_NSH_PLATFORM_HARMONYOS", stamp);
    if !verbose {
        cmd.stderr(std::process::Stdio::null());
    }
    if !cmd.status().context("running cargo")?.success() {
        bail!("the backend did not build");
    }
    let mut raw = String::new();
    for entry in fs::read_dir(&folder)? {
        raw.push_str(&fs::read_to_string(entry?.path())?);
        raw.push('\n');
    }
    let declarations = typedefs::format(&raw)?;
    let target = repo.join(
        "crates/nsh-platform-harmonyos/arkts/src/main/cpp/types/libnsh_platform_harmonyos/Index.d.ts",
    );
    fs::write(&target, declarations)?;
    println!("✓ wrote {}", target.display());
    Ok(())
}

mod typedefs {
    //! `napi-derive-ohos`'s type definitions (one JSON object per line) as
    //! an ArkTS declaration file.

    use std::collections::BTreeMap;

    use serde_json::Value;

    pub(crate) fn format(raw: &str) -> anyhow::Result<String> {
        let mut structs: BTreeMap<String, (String, String)> = BTreeMap::new();
        let mut impls: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut other: Vec<(String, String)> = Vec::new();
        for line in raw.lines().filter(|l| !l.trim().is_empty()) {
            let item: Value = serde_json::from_str(line)?;
            let kind = item["kind"].as_str().unwrap_or_default();
            let name = item["name"].as_str().unwrap_or_default().to_owned();
            let js_doc = item["js_doc"]
                .as_str()
                .unwrap_or_default()
                .replace("\n *", "\n  *");
            let def = item["def"].as_str().unwrap_or_default();
            let body = def.replace("\\n", "\n").replace('\n', "\n  ");
            match kind {
                "struct" => {
                    structs.insert(name, (js_doc, def.to_owned()));
                }
                "impl" if !def.is_empty() => impls.entry(name).or_default().push(def.to_owned()),
                "interface" => other.push((
                    name.clone(),
                    format!("{js_doc}export interface {name} {{\n  {body}\n}}"),
                )),
                "fn" => other.push((name, format!("{js_doc}export declare {body}"))),
                "enum" => other.push((
                    name.clone(),
                    format!("{js_doc}export declare const enum {name} {{\n  {body}\n}}"),
                )),
                "type" | "const" => other.push((name, format!("{js_doc}{def}"))),
                _ => {}
            }
        }
        other.sort_by(|a, b| a.0.cmp(&b.0));
        let mut defs: Vec<String> = other.into_iter().map(|(_, d)| d).collect();
        for (name, (js_doc, struct_def)) in structs {
            let mut parts = Vec::new();
            if !struct_def.is_empty() {
                parts.push(struct_def);
            }
            parts.extend(impls.remove(&name).unwrap_or_default());
            let body = parts
                .join("\n")
                .replace("\\n ", "\n")
                .replace("\\n", "\n")
                .replace("\n ", "\n")
                .trim_start_matches(' ')
                .replace("\n *", "\n  *")
                .replace('\n', "\n  ");
            defs.push(format!(
                "{js_doc}export declare class {name} {{\n  {body}\n}}"
            ));
        }
        let mut content = String::from(
            "/* Generated by `nsh-platform-harmonyos typedefs`: do not edit. */\n/* eslint-disable */\n\n",
        );
        content.push_str(&defs.join("\n\n"));
        content.push('\n');
        Ok(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<OsString> {
        line.split_whitespace().map(OsString::from).collect()
    }

    #[test]
    fn nsh_s_arguments() {
        let options = parse(args(
            "-v package examples/counter --out dist --runtime-os openharmony --debug",
        ))
        .unwrap();
        assert_eq!(options.command, "package");
        assert_eq!(options.app, Some("examples/counter".into()));
        assert_eq!(options.out, Some("dist".into()));
        assert_eq!(options.runtime_os, project::RuntimeOs::OpenHarmony);
        assert_eq!(options.release, Some(false));
        assert!(options.verbose && options.hap);
        assert!(parse(args("package --sign")).is_err());
        assert!(parse(args("package --runtime-os android")).is_err());
        let run = parse(args("run --device 127.0.0.1:5555 --no-hap")).unwrap();
        assert_eq!(run.device.as_deref(), Some("127.0.0.1:5555"));
        assert!(!run.hap);
        let dev = parse(args("dev examples/counter --port 8791")).unwrap();
        assert_eq!(dev.command, "dev");
        assert_eq!(dev.port, Some(8791));
    }

    #[test]
    fn formats_type_definitions() {
        let raw = r#"{"kind":"struct","name":"NshSurface","js_doc":"/** A surface. */\n","def":""}
{"kind":"impl","name":"NshSurface","js_doc":"","def":"runApp(): void"}
{"kind":"fn","name":"initLogging","js_doc":"","def":"function initLogging(): void"}"#;
        let out = typedefs::format(raw).unwrap();
        assert!(
            out.contains("export declare function initLogging(): void"),
            "{out}"
        );
        assert!(
            out.contains(
                "/** A surface. */\nexport declare class NshSurface {\n  runApp(): void\n}"
            ),
            "{out}"
        );
    }
}
