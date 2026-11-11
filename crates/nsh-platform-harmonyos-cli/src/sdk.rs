//! The tools this target builds with: the OpenHarmony NDK (the native
//! library, cross-compiled for `arm64-v8a`, `armeabi-v7a` and `x86_64`), and
//! DevEco Studio's or the command-line tools' `hvigorw`, `ohpm` and `hdc` (the
//! HAP, signed, installed on a device). Without DevEco, hvigor installed from
//! npm (`HVIGOR_HOME`) builds an OpenHarmony project with an OpenHarmony SDK
//! (`OHOS_BASE_SDK_HOME`), and the SDK's own tools sign it.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::bail;

use crate::paths::join_all;

/// A native target architecture an OpenHarmony / HarmonyOS HAP can carry
/// under `libs/<abi>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Arch {
    #[default]
    Arm64,
    Armv7a,
    X86_64,
}

impl Arch {
    pub(crate) fn parse(value: &str) -> anyhow::Result<Arch> {
        match value.to_ascii_lowercase().as_str() {
            "arm64" => Ok(Arch::Arm64),
            "armv7a" => Ok(Arch::Armv7a),
            "x86_64" => Ok(Arch::X86_64),
            other => bail!("--arch is arm64, armv7a or x86_64, not {other}"),
        }
    }

    /// Directory name under `entry/libs/<abi>`.
    pub(crate) const fn abi(self) -> &'static str {
        match self {
            Arch::Arm64 => "arm64-v8a",
            Arch::Armv7a => "armeabi-v7a",
            Arch::X86_64 => "x86_64",
        }
    }

    /// Rust target triple for `cargo build --target`.
    pub(crate) const fn triple(self) -> &'static str {
        match self {
            Arch::Arm64 => "aarch64-unknown-linux-ohos",
            Arch::Armv7a => "armv7-unknown-linux-ohos",
            Arch::X86_64 => "x86_64-unknown-linux-ohos",
        }
    }

    /// Clang `--target` triple in the OpenHarmony NDK sysroot.
    const fn clang_target(self) -> &'static str {
        match self {
            Arch::Arm64 => "aarch64-linux-ohos",
            Arch::Armv7a => "arm-linux-ohos",
            Arch::X86_64 => "x86_64-linux-ohos",
        }
    }

    /// Target-specific flags matching the OpenHarmony NDK's `<triple>-clang` wrapper.
    const fn extra_clang_flags(self) -> &'static [&'static str] {
        match self {
            Arch::Arm64 | Arch::X86_64 => &[],
            Arch::Armv7a => &[
                "-march=armv7-a",
                "-mfloat-abi=softfp",
                "-mtune=generic-armv7-a",
                "-mthumb",
            ],
        }
    }
}

/// The OpenHarmony NDK: the SDK's `native` directory.
pub(crate) struct Ndk {
    pub(crate) native: PathBuf,
}

/// Where an SDK may be: the environment, then DevEco Studio's and the
/// command-line tools' usual places.
fn candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for var in [
        "OHOS_NDK_HOME",
        "OHOS_SDK_HOME",
        "DEVECO_SDK_HOME",
        "DEVECO_STUDIO_HOME",
    ] {
        if let Some(value) = env::var_os(var) {
            out.push(PathBuf::from(value));
        }
    }
    let home = env::var_os("HOME").map(PathBuf::from);
    if cfg!(target_os = "macos") {
        out.push(join_all(
            "/",
            ["Applications", "DevEco-Studio.app", "Contents", "sdk"],
        ));
        if let Some(home) = &home {
            out.push(join_all(
                home,
                ["Applications", "DevEco-Studio.app", "Contents", "sdk"],
            ));
            out.push(join_all(home, ["Library", "OpenHarmony", "Sdk"]));
            out.push(join_all(home, ["Library", "Huawei", "Sdk"]));
            out.push(join_all(home, ["command-line-tools", "sdk"]));
        }
    } else if cfg!(windows) {
        if let Some(local) = env::var_os("LOCALAPPDATA").map(PathBuf::from) {
            out.push(join_all(&local, ["OpenHarmony", "Sdk"]));
            out.push(join_all(&local, ["Huawei", "Sdk"]));
        }
        out.push(r"C:\Program Files\Huawei\DevEco Studio\sdk".into());
    } else {
        if let Some(home) = &home {
            out.push(join_all(home, ["OpenHarmony", "Sdk"]));
            out.push(join_all(home, ["command-line-tools", "sdk"]));
        }
        out.push(join_all("/", ["opt", "command-line-tools", "sdk"]));
    }
    out
}

impl Ndk {
    pub(crate) fn find() -> anyhow::Result<Ndk> {
        if let Some(ndk) = candidates().iter().find_map(|c| Self::resolve(c, 4)) {
            return Ok(ndk);
        }
        bail!(
            "the OpenHarmony NDK was not found: install DevEco Studio, or set OHOS_NDK_HOME to an \
             SDK's `openharmony` directory (the one with `native`), API 20 or later"
        )
    }

    fn resolve(path: &Path, depth: u32) -> Option<Ndk> {
        if depth == 0 || !path.is_dir() {
            return None;
        }
        if join_all(path, ["llvm", "bin"]).is_dir() && path.join("sysroot").is_dir() {
            return Some(Ndk {
                native: path.to_path_buf(),
            });
        }
        for sub in [
            &["native"][..],
            &["openharmony", "native"],
            &["default", "openharmony", "native"],
        ] {
            let native = join_all(path, sub.iter().copied());
            if join_all(&native, ["llvm", "bin"]).is_dir() && native.join("sysroot").is_dir() {
                return Some(Ndk { native });
            }
        }
        // A versioned SDK directory (`<Sdk>/20/…`): the newest first.
        let mut versions: Vec<PathBuf> = fs::read_dir(path)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        versions.sort_by_key(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .split(['.', '-'])
                .map(|part| part.parse::<u64>().unwrap_or(0))
                .collect::<Vec<_>>()
        });
        versions
            .iter()
            .rev()
            .find_map(|version| Self::resolve(version, depth - 1))
    }

    /// A cargo command that cross-compiles for `arch` with this NDK.
    pub(crate) fn cargo(&self, arch: Arch) -> Command {
        let bin = join_all(&self.native, ["llvm", "bin"]);
        let sysroot = self.native.join("sysroot");
        let exe = env::consts::EXE_SUFFIX;
        let triple_name = arch.triple();
        let clang_target = arch.clang_target();
        let clang = bin.join(format!("clang{exe}"));
        let wrapper = bin.join(format!("{triple_name}-clang"));
        let wrapper_xx = bin.join(format!("{triple_name}-clang++"));
        let use_wrapper = cfg!(unix) && wrapper.exists() && wrapper_xx.exists();
        let (cc, cxx) = if use_wrapper {
            (wrapper, wrapper_xx)
        } else {
            (clang.clone(), bin.join(format!("clang++{exe}")))
        };
        let triple = triple_name.replace('-', "_");
        let sysroot_slash = sysroot.display().to_string().replace('\\', "/");
        let mut flag_parts = vec![
            format!("--target={clang_target}"),
            format!("--sysroot=\"{sysroot_slash}\""),
            "-D__MUSL__".to_owned(),
        ];
        flag_parts.extend(arch.extra_clang_flags().iter().map(|f| (*f).to_owned()));
        let flags = flag_parts.join(" ");
        let mut cmd = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
        // N-API's build script finds the system libraries under
        // `$OHOS_NDK_HOME/native`.
        if let Some(openharmony) = self.native.parent() {
            cmd.env("OHOS_NDK_HOME", openharmony);
        }
        cmd.env(format!("CC_{triple}"), &cc)
            .env(format!("CXX_{triple}"), &cxx)
            .env(format!("AR_{triple}"), bin.join(format!("llvm-ar{exe}")))
            .env(
                format!("RANLIB_{triple}"),
                bin.join(format!("llvm-ranlib{exe}")),
            )
            .env(
                format!("CARGO_TARGET_{}_LINKER", triple.to_uppercase()),
                if use_wrapper { &cc } else { &clang },
            )
            .env(format!("BINDGEN_EXTRA_CLANG_ARGS_{triple}"), &flags);
        if !use_wrapper {
            cmd.env(format!("CFLAGS_{triple}"), &flags)
                .env(format!("CXXFLAGS_{triple}"), &flags);
            let mut rustflags = vec![
                format!("-Clink-arg=--target={clang_target}"),
                format!("-Clink-arg=--sysroot={}", sysroot.display()),
            ];
            for extra in arch.extra_clang_flags() {
                rustflags.push(format!("-Clink-arg={extra}"));
            }
            cmd.env("CARGO_ENCODED_RUSTFLAGS", rustflags.join("\x1f"));
        }
        // bindgen (QuickJS's bindings) loads the NDK's libclang.
        if env::var_os("LIBCLANG_PATH").is_none() {
            let lib = if cfg!(windows) {
                bin.clone()
            } else {
                join_all(&self.native, ["llvm", "lib"])
            };
            cmd.env("LIBCLANG_PATH", lib);
        }
        cmd
    }
}

/// How hvigor runs.
pub(crate) enum Hvigor {
    /// DevEco Studio's or the command-line tools' `hvigorw`.
    Wrapper(PathBuf),
    /// `@ohos/hvigor` installed from npm: `node <bin/hvigor.js>`, its
    /// packages on `NODE_PATH`.
    Script { js: PathBuf, modules: PathBuf },
}

/// An OpenHarmony SDK as hvigor reads it (`OHOS_BASE_SDK_HOME`, the
/// project's `local.properties` `sdk.dir`): a directory per API, each with
/// `ets`, `js`, `native`, `toolchains`, `previewer`.
pub(crate) struct BaseSdk {
    pub(crate) dir: PathBuf,
    /// The newest API in it.
    pub(crate) api: u32,
}

impl BaseSdk {
    pub(crate) fn find() -> Option<BaseSdk> {
        let dir = PathBuf::from(env::var_os("OHOS_BASE_SDK_HOME")?);
        let api = fs::read_dir(&dir)
            .ok()?
            .flatten()
            .filter(|e| e.path().join("toolchains").is_dir())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.split('.').next()?.parse::<u32>().ok()
            })
            .max()?;
        Some(BaseSdk { dir, api })
    }

    /// The newest API's directory.
    pub(crate) fn openharmony(&self) -> PathBuf {
        let plain = self.dir.join(self.api.to_string());
        if plain.is_dir() {
            plain
        } else {
            self.dir.join(format!("{}.0.0", self.api))
        }
    }
}

/// DevEco Studio's (or the command-line tools') build and device tools, or
/// hvigor from npm.
pub(crate) struct DevEco {
    /// Builds the HAP.
    pub(crate) hvigor: Hvigor,
    /// `ohpm`: installs the project's ArkTS packages.
    pub(crate) ohpm: Option<PathBuf>,
    /// The SDK directory hvigor reads (`DEVECO_SDK_HOME`).
    pub(crate) sdk: Option<PathBuf>,
    /// Node.js for hvigor, when the tools bring their own.
    pub(crate) node: Option<PathBuf>,
}

impl DevEco {
    /// The tools, when they are installed: DevEco's, or hvigor from npm.
    pub(crate) fn find() -> Option<DevEco> {
        Self::find_deveco().or_else(Self::find_npm)
    }

    /// `@ohos/hvigor` under `HVIGOR_HOME` (an npm prefix, or its
    /// `node_modules`), run by the `node` on `PATH`.
    fn find_npm() -> Option<DevEco> {
        let home = PathBuf::from(env::var_os("HVIGOR_HOME")?);
        let hvigor_js = ["@ohos", "hvigor", "bin", "hvigor.js"];
        let modules = [home.join("node_modules"), home]
            .into_iter()
            .find(|m| join_all(m, hvigor_js).is_file())?;
        let ohpm = env::var_os("PATH").and_then(|path| {
            let name = if cfg!(windows) { "ohpm.bat" } else { "ohpm" };
            env::split_paths(&path)
                .map(|dir| dir.join(name))
                .find(|p| p.is_file())
        });
        Some(DevEco {
            hvigor: Hvigor::Script {
                js: join_all(&modules, hvigor_js),
                modules,
            },
            ohpm,
            sdk: None,
            node: None,
        })
    }

    /// Custom installs can use DEVECO_STUDIO_HOME or DEVECO_CONTENTS.
    fn find_deveco() -> Option<DevEco> {
        let mut roots: Vec<PathBuf> = Vec::new();
        for var in [
            "DEVECO_STUDIO_HOME",
            "DEVECO_CONTENTS",
            "OHOS_COMMAND_LINE_TOOLS",
        ] {
            if let Some(value) = env::var_os(var) {
                roots.push(value.into());
            }
        }
        if cfg!(target_os = "macos") {
            roots.push(join_all(
                "/",
                ["Applications", "DevEco-Studio.app", "Contents"],
            ));
            if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
                roots.push(join_all(
                    &home,
                    ["Applications", "DevEco-Studio.app", "Contents"],
                ));
                roots.push(home.join("command-line-tools"));
            }
        } else if cfg!(windows) {
            roots.push(r"C:\Program Files\Huawei\DevEco Studio".into());
        } else {
            if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
                roots.push(home.join("command-line-tools"));
            }
            roots.push(join_all("/", ["opt", "command-line-tools"]));
        }
        let script = if cfg!(windows) {
            "hvigorw.bat"
        } else {
            "hvigorw"
        };
        roots.into_iter().find_map(|root| {
            let hvigorw = [
                join_all(&root, ["tools", "hvigor", "bin", script]),
                join_all(&root, ["bin", script]),
                join_all(&root, ["hvigor", "bin", script]),
            ]
            .into_iter()
            .find(|p| p.is_file())?;
            let ohpm_name = if cfg!(windows) { "ohpm.bat" } else { "ohpm" };
            let ohpm = [
                join_all(&root, ["tools", "ohpm", "bin", ohpm_name]),
                join_all(&root, ["bin", ohpm_name]),
                join_all(&root, ["ohpm", "bin", ohpm_name]),
            ]
            .into_iter()
            .find(|p| p.is_file());
            let sdk = env::var_os("DEVECO_SDK_HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_dir())
                .or_else(|| Some(root.join("sdk")).filter(|p| p.is_dir()));
            let node = [
                join_all(&root, ["tools", "node", "bin"]),
                join_all(&root, ["tool", "node", "bin"]),
            ]
            .into_iter()
            .find(|p| p.is_dir());
            Some(DevEco {
                hvigor: Hvigor::Wrapper(hvigorw),
                ohpm,
                sdk,
                node,
            })
        })
    }

    /// A command running `program` with the tools' environment.
    pub(crate) fn command(&self, program: &Path) -> Command {
        let mut cmd = Command::new(program);
        if let Some(sdk) = &self.sdk {
            cmd.env("DEVECO_SDK_HOME", sdk);
        }
        if let Some(node) = &self.node {
            let path = env::var_os("PATH").unwrap_or_default();
            let mut paths = vec![node.clone()];
            paths.extend(env::split_paths(&path));
            if let Ok(joined) = env::join_paths(paths) {
                cmd.env("PATH", joined);
            }
        }
        cmd
    }

    /// The command that runs hvigor.
    pub(crate) fn hvigor(&self) -> Command {
        match &self.hvigor {
            Hvigor::Wrapper(hvigorw) => self.command(hvigorw),
            Hvigor::Script { js, modules } => {
                let mut cmd = self.command(Path::new("node"));
                cmd.arg(js).env("NODE_PATH", modules);
                cmd
            }
        }
    }

    /// `hdc`, which installs and starts the HAP on a device.
    pub(crate) fn hdc(&self) -> Option<PathBuf> {
        hdc(self.sdk.as_deref())
    }
}

/// `hdc` on `PATH`, in the SDK's `toolchains`, or beside the NDK.
pub(crate) fn hdc(sdk: Option<&Path>) -> Option<PathBuf> {
    let name = if cfg!(windows) { "hdc.exe" } else { "hdc" };
    if let Some(path) = env::var_os("PATH")
        && let Some(found) = env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|p| p.is_file())
    {
        return Some(found);
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(sdk) = sdk {
        dirs.push(join_all(sdk, ["default", "openharmony", "toolchains"]));
        dirs.push(join_all(sdk, ["openharmony", "toolchains"]));
    }
    if let Some(base) = BaseSdk::find() {
        dirs.push(base.openharmony().join("toolchains"));
    }
    if let Ok(ndk) = Ndk::find()
        && let Some(parent) = ndk.native.parent()
    {
        dirs.push(parent.join("toolchains"));
    }
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

/// The OpenHarmony SDK's signing tools and debug keys
/// (`toolchains/lib`: `hap-sign-tool.jar`, `OpenHarmony.p12`, the profile
/// template and certificate), in the base SDK or beside the NDK.
pub(crate) fn signing_lib() -> Option<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(base) = BaseSdk::find() {
        dirs.push(join_all(base.openharmony(), ["toolchains", "lib"]));
    }
    if let Ok(ndk) = Ndk::find()
        && let Some(parent) = ndk.native.parent()
    {
        dirs.push(join_all(parent, ["toolchains", "lib"]));
    }
    dirs.into_iter().find(|d| {
        [
            "hap-sign-tool.jar",
            "OpenHarmony.p12",
            "UnsgnedReleasedProfileTemplate.json",
        ]
        .iter()
        .all(|f| d.join(f).is_file())
    })
}

/// Picks the first supported [`Arch`] advertised in a comma-separated
/// `const.product.cpu.abilist` value from a device or emulator.
pub(crate) fn parse_device_arch(abilist: &str) -> Option<Arch> {
    abilist.split(',').find_map(|abi| match abi.trim() {
        "arm64-v8a" => Some(Arch::Arm64),
        "armeabi-v7a" => Some(Arch::Armv7a),
        "x86_64" => Some(Arch::X86_64),
        _ => None,
    })
}

/// Queries the connected device's primary ABI via `hdc` when available.
pub(crate) fn device_arch(sdk: Option<&Path>, device: Option<&str>) -> Option<Arch> {
    let hdc = hdc(sdk)?;
    let mut cmd = Command::new(hdc);
    if let Some(device) = device {
        cmd.args(["-t", device]);
    }
    let out = cmd
        .args(["shell", "param", "get", "const.product.cpu.abilist"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_device_arch(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_arch_names_and_device_abilists() {
        assert_eq!(Arch::default(), Arch::Arm64);
        assert_eq!(Arch::parse("arm64").unwrap(), Arch::Arm64);
        assert_eq!(Arch::parse("armv7a").unwrap(), Arch::Armv7a);
        assert_eq!(Arch::parse("x86_64").unwrap(), Arch::X86_64);
        assert!(Arch::parse("arm64-v8a").is_err());
        assert!(Arch::parse("arm64,armv7a").is_err());
        assert!(Arch::parse("mips").is_err());

        let all = [Arch::Arm64, Arch::Armv7a, Arch::X86_64];
        assert_eq!(all.map(Arch::abi), ["arm64-v8a", "armeabi-v7a", "x86_64"]);
        assert_eq!(
            all.map(Arch::triple),
            [
                "aarch64-unknown-linux-ohos",
                "armv7-unknown-linux-ohos",
                "x86_64-unknown-linux-ohos"
            ]
        );
        assert_eq!(
            parse_device_arch("arm64-v8a,armeabi-v7a\r\n"),
            Some(Arch::Arm64)
        );
        assert_eq!(parse_device_arch("x86_64\n"), Some(Arch::X86_64));
        assert_eq!(parse_device_arch("default,armeabi-v7a"), Some(Arch::Armv7a));
        assert_eq!(parse_device_arch(""), None);
    }
}
