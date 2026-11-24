//! The runner: a `cdylib` crate the CLI writes in the app's
//! `.nsh/harmonyos/runner`, linking this platform's backend with the app (an
//! app written in Rust: NSH's generated `nsh_rust_app`; a TSX app with Rust
//! crates of its own: those crates' UI sets), built for the target OHOS ABI
//! (`arm64-v8a` by default, or `armeabi-v7a` / `x86_64`) into
//! `libnsh_platform_harmonyos.so`, the native library the HAP loads.

use std::{
    fs,
    io::BufRead as _,
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::{Context as _, bail};

use crate::{
    nshm::{LinkedCrate, PatchEntry},
    paths::join_all,
    sdk::{ApiLevel, Arch, Ndk},
};

/// The library's name: the backend's N-API module is registered under its
/// crate's name, so the library must carry the same one.
pub(crate) const LIBRARY: &str = "libnsh_platform_harmonyos.so";

/// The backend this CLI was built with: `NSH_HARMONYOS_SOURCE` (a checkout)
/// when set, else the checkout it was built in when it is still there, else
/// this version's tag of the repository.
fn backend(dev: bool, api: ApiLevel) -> String {
    let mut enabled = Vec::new();
    if dev {
        enabled.push("\"dev\"");
    }
    if let Some(feature) = api.backend_feature() {
        enabled.push(match feature {
            "api-23" => "\"api-23\"",
            "api-26" => "\"api-26\"",
            _ => unreachable!(),
        });
    }
    let features = if enabled.is_empty() {
        String::new()
    } else {
        format!(", features = [{}]", enabled.join(", "))
    };
    let checkout = std::env::var_os("NSH_HARMONYOS_SOURCE")
        .map(PathBuf::from)
        .or_else(|| {
            // This repository: the nshm's directory `nsh` builds it in.
            Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")))
        })
        .filter(|dir| {
            join_all(dir, ["crates", "nsh-platform-harmonyos-core", "Cargo.toml"]).is_file()
        });
    match checkout {
        Some(dir) => format!(
            "{{ package = \"nsh-platform-harmonyos-core\", path = {}{features} }}",
            toml_string(
                &join_all(&dir, ["crates", "nsh-platform-harmonyos-core"]).to_string_lossy()
            )
        ),
        None => format!(
            "{{ package = \"nsh-platform-harmonyos-core\", version = \"={}\"{features} }}",
            env!("CARGO_PKG_VERSION")
        ),
    }
}

/// The value of `key = "…"` on a line of `manifest` that starts with
/// `line_start`.
fn value<'a>(manifest: &'a str, line_start: &str, key: &str) -> Option<&'a str> {
    let line = manifest.lines().find(|l| l.starts_with(line_start))?;
    let rest = &line[line.find(&format!("{key} = \""))? + key.len() + 4..];
    Some(&rest[..rest.find('"')?])
}

/// The `[package] name` of a `Cargo.toml` manifest.
pub(crate) fn package_name(manifest: &str) -> Option<&str> {
    let mut in_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
        } else if in_package
            && let Some(rest) = line.strip_prefix("name")
            && let Some(rest) = rest.trim_start().strip_prefix('=')
        {
            let rest = rest.trim_start();
            let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
            let rest = &rest[1..];
            return Some(&rest[..rest.find(quote)?]);
        }
    }
    None
}

/// What the runner links with the backend.
pub(crate) enum App<'a> {
    /// A TSX app: its bundle is a raw file of the HAP; `nsh/base`'s UI set.
    Tsx,
    /// An app written in Rust (`nsh.json` `"root"`): NSH's generated crate
    /// `rust-app` (`nsh_rust_app`), with its UI sets and root.
    Rust(&'a Path),
    /// A TSX app with Rust crates of its own (`nsh.json` `"rust"` without
    /// `"root"`) and/or Rust nshms (`nsh.lock` `"module"`): the bundle is a
    /// raw file of the HAP, and the crates' UI sets (each crate's
    /// `#[native_module]` `registry()`) are served after `nsh/base`'s for it
    /// to import. `workspace` is NSH's generated workspace (`.nsh/rust`),
    /// whose `[patch]` sections the runner takes, augmented with `patches`
    /// from `nsh.lock` when `.nsh/rust` is absent.
    TsxWithCrates {
        crates: &'a [LinkedCrate],
        patches: &'a [PatchEntry],
        workspace: &'a Path,
    },
}

/// The `[patch]` sections of NSH's generated workspace (`.nsh/rust`): the
/// runner takes them as they are, so the app and the backend share one
/// `nsh-core`.
fn workspace_patches(workspace_manifest: &str) -> String {
    let mut out = String::new();
    let mut in_patch = false;
    for line in workspace_manifest.lines() {
        if line.starts_with('[') {
            in_patch = line.starts_with("[patch");
        }
        if in_patch {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Formats `s` as a valid TOML basic string (JSON string escaping is a subset of TOML).
pub(crate) fn toml_string(s: &str) -> String {
    serde_json::to_string(s).expect("serializing TOML string")
}

/// Formats `key` as a TOML bare key when safe, otherwise a quoted basic string.
fn toml_key(key: &str) -> String {
    if !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        key.to_owned()
    } else {
        toml_string(key)
    }
}

fn unquote_toml_key(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2 {
        serde_json::from_str::<String>(trimmed).unwrap_or_else(|_| trimmed.to_owned())
    } else if trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2 {
        trimmed[1..trimmed.len() - 1].to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Parses `[patch.*]` sections from TOML text into ordered `(header, lines)` pairs.
fn parse_patch_sections(text: &str) -> Vec<(String, Vec<String>)> {
    let mut sections: Vec<(String, Vec<String>)> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            sections.push((trimmed.to_owned(), Vec::new()));
        } else if !trimmed.is_empty()
            && let Some((_, lines)) = sections.last_mut()
        {
            lines.push(line.to_owned());
        }
    }
    sections
}

fn format_patch_sections(sections: Vec<(String, Vec<String>)>) -> String {
    let mut out = String::new();
    for (header, lines) in sections {
        out.push_str(&header);
        out.push('\n');
        for line in lines {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

fn insert_patch_line(
    sections: &mut Vec<(String, Vec<String>)>,
    header: &str,
    pkg_key: &str,
    line: String,
) {
    if let Some((_, lines)) = sections.iter_mut().find(|(h, _)| h == header) {
        let already = lines.iter().any(|existing| {
            existing
                .trim()
                .split_once('=')
                .is_some_and(|(k, _)| unquote_toml_key(k) == pkg_key)
        });
        if !already {
            lines.push(line);
        }
    } else {
        sections.push((header.to_owned(), vec![line]));
    }
}

fn has_patch_package(sections: &[(String, Vec<String>)], header: &str, pkg_key: &str) -> bool {
    sections
        .iter()
        .find(|(h, _)| h == header)
        .is_some_and(|(_, lines)| {
            lines.iter().any(|existing| {
                existing
                    .trim()
                    .split_once('=')
                    .is_some_and(|(k, _)| unquote_toml_key(k) == pkg_key)
            })
        })
}

/// The `[patch]` sections of the generated workspace in `workspace`, merged with
/// the backend's own `NSH_PATCH` defaults whenever the workspace does not
/// already own the `nsh-core` patch (so non-NSH `[patch]` tables in
/// `.nsh/rust/Cargo.toml` never drop the backend's NSH crate pins, while local
/// NSH checkouts are never mixed with `NSH_PATCH`'s git rev).
fn patches_of(workspace: &Path) -> String {
    let ws_raw = fs::read_to_string(workspace.join("Cargo.toml"))
        .ok()
        .map(|m| workspace_patches(&m))
        .unwrap_or_default();
    if ws_raw.trim().is_empty() {
        return crate::embedded::NSH_PATCH.to_owned();
    }
    let mut sections = parse_patch_sections(&ws_raw);
    if has_patch_package(&sections, "[patch.crates-io]", "nsh-core") {
        return ws_raw;
    }
    for (header, lines) in parse_patch_sections(crate::embedded::NSH_PATCH) {
        for line in lines {
            if let Some((k, _)) = line.trim().split_once('=') {
                let key = unquote_toml_key(k);
                insert_patch_line(&mut sections, &header, &key, line);
            }
        }
    }
    format_patch_sections(sections)
}

/// Merges `[patch]` entries from `nsh.lock` into `base` without duplicating
/// existing table headers or already-patched packages.
fn merge_patch_entries(base: &str, entries: &[PatchEntry]) -> String {
    if entries.is_empty() {
        return base.to_owned();
    }
    let mut sections = parse_patch_sections(base);
    for entry in entries {
        let header = entry.table.header();
        let line = format!(
            "{} = {{ path = {} }}",
            toml_key(&entry.package),
            toml_string(&entry.path.to_string_lossy()),
        );
        insert_patch_line(&mut sections, &header, &entry.package, line);
    }
    format_patch_sections(sections)
}

/// Writes the runner crate in `dir` for `app`.
pub(crate) fn write(dir: &Path, app: App<'_>, dev: bool, api: ApiLevel) -> anyhow::Result<()> {
    let (app_dependency, patch, lib) = match app {
        App::Rust(rust_app) => {
            let manifest = fs::read_to_string(rust_app.join("Cargo.toml"))
                .with_context(|| format!("reading {}", rust_app.display()))?;
            let package =
                package_name(&manifest).context("NSH's rust-app crate has no [package] name")?;
            let patches = rust_app
                .parent()
                .map(patches_of)
                .unwrap_or_else(|| crate::embedded::NSH_PATCH.to_owned());
            let patches = if dev {
                testing_patches(&patches, Some(&manifest))
            } else {
                patches
            };
            (
                format!(
                    "nsh_rust_app = {{ package = {}, path = {} }}\n",
                    toml_string(package),
                    toml_string(&rust_app.to_string_lossy()),
                ),
                patches,
                "// An app written in Rust: its UI sets (`nsh/base`'s, then its crates')\n\
                 // and its root, which `NshSurface.runApp()` starts.\n\
                 backend::app!(nsh_rust_app::ui_sets, nsh_rust_app::root);\n"
                    .to_owned(),
            )
        }
        App::TsxWithCrates {
            crates,
            patches: lock_patches,
            workspace,
        } => {
            let mut dependencies = String::new();
            let mut sets = Vec::new();
            for (i, krate) in crates.iter().enumerate() {
                // Renamed, so the runner names each crate without knowing its
                // library's name.
                let alias = format!("app_{i}");
                dependencies.push_str(&krate.dependency_line(&alias));
                sets.push(format!("{alias}::registry"));
            }
            let patches = merge_patch_entries(&patches_of(workspace), lock_patches);
            let patches = if dev {
                testing_patches(&patches, None)
            } else {
                patches
            };
            (
                dependencies,
                patches,
                format!(
                    "// A TSX app with Rust crates of its own: its bundle is a raw file of the\n\
                     // HAP (`loadResource`), and it imports the crates' UI sets.\n\
                     backend::app!(sets = [{}]);\n",
                    sets.join(", ")
                ),
            )
        }
        App::Tsx => (
            String::new(),
            crate::embedded::NSH_PATCH.to_owned(),
            "// A TSX app: its bundle is a raw file of the HAP (`loadResource`).\n\
             backend::app!();\n"
                .to_owned(),
        ),
    };
    fs::create_dir_all(dir)?;
    fs::write(
        dir.join("Cargo.toml"),
        format!(
            "# Generated by nsh-platform-harmonyos: do not edit.\n\
             [package]\nname = \"nsh-harmonyos-runner\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n\n\
             [lib]\nname = \"nsh_platform_harmonyos\"\npath = \"lib.rs\"\ncrate-type = [\"cdylib\"]\n\n\
             [dependencies]\nbackend = {}\n{app_dependency}\n\
             [build-dependencies]\nnapi-build-ohos = \"1.2.0\"\n\n\
             [profile.release]\nlto = \"fat\"\ncodegen-units = 1\nstrip = true\n\n\
             [profile.dev.package.quickjs-jit-sys]\nopt-level = 3\n\
             [profile.dev.package.taffy]\nopt-level = 3\n\n\
             {patch}\n[workspace]\n",
            backend(dev, api),
        ),
    )?;
    fs::write(
        dir.join("lib.rs"),
        format!(
            "//! The app's native library for HarmonyOS: NSH's engine and the ArkUI\n\
             //! backend, with the app.\n\n{lib}"
        ),
    )?;
    fs::write(
        dir.join("build.rs"),
        "//! N-API's link setup for OHOS (the unwinder in place of libgcc).\n\n\
         fn main() {\n    napi_build_ohos::setup();\n}\n",
    )?;
    Ok(())
}

/// The agent must use the same core/engine as the app, including when its
/// generated workspace points to a local NSH checkout instead of crates.io.
fn testing_patches(patches: &str, app_manifest: Option<&str>) -> String {
    let mut in_crates_io = false;
    for line in patches.lines() {
        if line.starts_with('[') {
            in_crates_io = line.trim() == "[patch.crates-io]";
        }
        if in_crates_io && line.trim_start().starts_with("nsh-testing =") {
            return patches.to_owned();
        }
    }
    let core = value(patches, "nsh-core", "path")
        .or_else(|| app_manifest.and_then(|m| value(m, "nsh-core", "path")));
    let patch = if let Some(core) = core {
        format!(
            "nsh-testing = {{ path = {:?} }}",
            Path::new(core).with_file_name("nsh-testing")
        )
    } else if let Some(line) = patches
        .lines()
        .find(|line| line.trim_start().starts_with("nsh-core ="))
    {
        line.replacen("nsh-core", "nsh-testing", 1)
    } else if let Some(line) = crate::embedded::NSH_PATCH
        .lines()
        .find(|line| line.starts_with("nsh-testing ="))
    {
        line.to_owned()
    } else {
        return patches.to_owned();
    };
    let mut result = String::new();
    let mut inserted = false;
    for line in patches.lines() {
        result.push_str(line);
        result.push('\n');
        if line.trim() == "[patch.crates-io]" {
            result.push_str(&patch);
            result.push('\n');
            inserted = true;
        }
    }
    if !inserted {
        result.push_str(&format!("\n[patch.crates-io]\n{patch}\n"));
    }
    result
}

/// Builds the runner in `dir` for `arch` with the NDK; the library's path.
pub(crate) fn build(
    dir: &Path,
    ndk: &Ndk,
    arch: Arch,
    release: bool,
    verbose: bool,
) -> anyhow::Result<PathBuf> {
    let abi = arch.abi();
    let triple = arch.triple();
    println!(
        "• building {LIBRARY} for {abi} (cargo build{})",
        if release { " --release" } else { "" }
    );
    let mut cmd = ndk.cargo(arch);
    cmd.args([
        "build",
        "--target",
        triple,
        "--message-format=json-render-diagnostics",
    ])
    .current_dir(dir);
    if release {
        cmd.arg("--release");
    }
    run_cargo(&mut cmd, abi, triple, verbose)
}

const STDERR_TAIL_LIMIT: usize = 64 * 1024;

fn is_cargo_status_line(line: &str) -> bool {
    const VERBS: &[&str] = &[
        "Compiling ",
        "Checking ",
        "Downloading ",
        "Downloaded ",
        "Updating ",
        "Locking ",
        "Blocking ",
        "Packaging ",
        "Building ",
        "Finished ",
        "Fresh ",
    ];
    let trimmed = line.trim_start_matches(' ');
    line.len().saturating_sub(trimmed.len()) >= 2 && VERBS.iter().any(|v| trimmed.starts_with(v))
}

fn push_bounded_tail(tail: &mut std::collections::VecDeque<u8>, bytes: &[u8]) {
    if bytes.len() >= STDERR_TAIL_LIMIT {
        tail.clear();
        tail.extend(&bytes[bytes.len() - STDERR_TAIL_LIMIT..]);
        return;
    }
    let overflow = (tail.len() + bytes.len()).saturating_sub(STDERR_TAIL_LIMIT);
    if overflow > 0 {
        tail.drain(..overflow);
    }
    tail.extend(bytes);
}

fn run_cargo(
    cmd: &mut std::process::Command,
    abi: &str,
    triple: &str,
    verbose: bool,
) -> anyhow::Result<PathBuf> {
    cmd.stdout(Stdio::piped()).stderr(if verbose {
        Stdio::inherit()
    } else {
        Stdio::piped()
    });
    let mut child = cmd.spawn().context("running cargo")?;
    let stdout = match child.stdout.take().context("cargo's output") {
        Ok(stdout) => stdout,
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(err);
        }
    };
    let stderr_reader = child.stderr.take().map(|stderr| {
        std::thread::spawn(move || -> std::collections::VecDeque<u8> {
            use std::io::Write as _;
            let mut tail = std::collections::VecDeque::with_capacity(8192);
            let mut reader = std::io::BufReader::new(stderr);
            let mut buf = Vec::with_capacity(512);
            let mut in_warning = false;
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        push_bounded_tail(&mut tail, &buf);
                        let text = String::from_utf8_lossy(&buf);
                        if text.starts_with("warning") {
                            in_warning = true;
                        } else if text.starts_with("error")
                            || is_cargo_status_line(&text)
                            || (in_warning && !text.starts_with(' ') && !text.trim().is_empty())
                        {
                            in_warning = false;
                        }
                        if in_warning {
                            let _ = std::io::stderr().write_all(&buf);
                        }
                    }
                }
            }
            tail
        })
    });

    let mut library = None;
    let mut stdout_error = None;
    for line in std::io::BufReader::new(stdout).lines() {
        let line = match line {
            Ok(line) => line,
            Err(err) => {
                stdout_error = Some(err);
                break;
            }
        };
        let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if message["reason"] == "compiler-artifact"
            && message["target"]["name"] == "nsh_platform_harmonyos"
            && message["target"]["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|k| k == "cdylib"))
        {
            library = message["filenames"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|f| f.as_str())
                .find(|f| f.ends_with(".so"))
                .map(PathBuf::from);
        }
    }

    if let Some(err) = stdout_error {
        let _ = child.kill();
        let _ = child.wait();
        if let Some(handle) = stderr_reader {
            let _ = handle.join();
        }
        return Err(err).context("reading cargo output");
    }

    let status = child.wait().context("waiting for cargo")?;
    let stderr_tail = stderr_reader
        .and_then(|handle| handle.join().ok())
        .map(|mut d| String::from_utf8_lossy(d.make_contiguous()).into_owned())
        .unwrap_or_default();

    if !status.success() {
        bail!(
            "the native library did not build for {abi} (exit {status}; rustup target add {triple}?)\n{stderr_tail}"
        );
    }
    library.with_context(|| {
        if stderr_tail.trim().is_empty() {
            "cargo built no .so".to_string()
        } else {
            format!("cargo built no .so\n{stderr_tail}")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST_APP: &str = "[package]\nname = \"counter-rust-app\"\nversion = \"0.0.0\"\n\n\
        [dependencies]\nnsh-core = { path = \"/src/nsh/crates/nsh-core\" }\n";

    #[test]
    fn takes_the_generated_workspace_s_patches() {
        let workspace = "[workspace]\nmembers = []\n\n[profile.web]\nopt-level = \"z\"\n\n\
            [patch.crates-io]\nnsh-core = { path = \"/c/crates/nsh-core\" }\n";
        let patches = workspace_patches(workspace);
        assert!(patches.starts_with("[patch.crates-io]\n"), "{patches}");
        assert!(!patches.contains("opt-level"));
    }

    #[test]
    fn the_runner_links_the_app_and_the_backend() {
        let dir = std::env::temp_dir().join(format!("nsh-hm-runner-{}", std::process::id()));
        let app = dir.join("rust-app");
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join("Cargo.toml"), RUST_APP).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\n[patch.crates-io]\nnsh-core = { path = \"/src/nsh/crates/nsh-core\" }\n",
        )
        .unwrap();
        write(&dir.join("runner"), App::Rust(&app), false, ApiLevel::Api20).unwrap();
        let manifest = fs::read_to_string(dir.join("runner").join("Cargo.toml")).unwrap();
        assert!(manifest.contains("nsh_rust_app = { package = \"counter-rust-app\", path = "));
        assert!(manifest.contains("[lib]\nname = \"nsh_platform_harmonyos\""));
        assert!(manifest.contains("crate-type = [\"cdylib\"]"));
        assert!(manifest.contains("nsh-core = { path = \"/src/nsh/crates/nsh-core\" }"));
        assert!(!manifest.contains("features ="));
        let lib = fs::read_to_string(dir.join("runner").join("lib.rs")).unwrap();
        assert!(lib.contains("backend::app!(nsh_rust_app::ui_sets, nsh_rust_app::root)"));

        write(
            &dir.join("runner-dev"),
            App::Rust(&app),
            true,
            ApiLevel::Api20,
        )
        .unwrap();
        let dev_manifest = fs::read_to_string(dir.join("runner-dev").join("Cargo.toml")).unwrap();
        assert!(dev_manifest.contains("features = [\"dev\"]"));
        let expected_testing = format!(
            "nsh-testing = {{ path = {:?} }}",
            Path::new("/src/nsh/crates/nsh-core").with_file_name("nsh-testing")
        );
        assert!(dev_manifest.contains(&expected_testing), "{dev_manifest}");

        write(&dir.join("tsx"), App::Tsx, true, ApiLevel::Api23).unwrap();
        let tsx_manifest = fs::read_to_string(dir.join("tsx").join("Cargo.toml")).unwrap();
        assert!(tsx_manifest.contains("features = [\"dev\", \"api-23\"]"));
        let lib = fs::read_to_string(dir.join("tsx").join("lib.rs")).unwrap();
        assert!(lib.contains("backend::app!();"));

        write(&dir.join("tsx-api26"), App::Tsx, false, ApiLevel::Api26).unwrap();
        let api26_manifest = fs::read_to_string(dir.join("tsx-api26").join("Cargo.toml")).unwrap();
        assert!(api26_manifest.contains("features = [\"api-26\"]"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_tsx_app_s_runner_links_its_crates_ui_sets() {
        let dir = std::env::temp_dir().join(format!("nsh-hm-crates-{}", std::process::id()));
        let (one, two, ws) = (
            dir.join("native"),
            dir.join("charts"),
            join_all(&dir, [".nsh", "rust"]),
        );
        fs::create_dir_all(&one).unwrap();
        fs::write(
            one.join("Cargo.toml"),
            "[package]\nname = \"app-native\"\nversion = \"0.1.0\"\n\n[lib]\nname = \"other\"\n",
        )
        .unwrap();
        fs::create_dir_all(&two).unwrap();
        fs::write(
            two.join("Cargo.toml"),
            "[lib]\nname = \"other\"\n\n[package]\n  name = \"app-charts\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::create_dir_all(&ws).unwrap();
        fs::write(
            ws.join("Cargo.toml"),
            "[workspace]\n[patch.crates-io]\nnsh-core = { path = \"/src/nsh/crates/nsh-core\" }\n",
        )
        .unwrap();
        let mut two_linked = LinkedCrate::path("app-charts".into(), two.clone());
        two_linked.features = vec!["camera".into()];
        let crates = [
            LinkedCrate::path("app-native".into(), one.clone()),
            two_linked,
        ];
        let lock_patches = [
            PatchEntry {
                table: crate::nshm::PatchTable::CratesIo,
                package: "app-charts".into(),
                path: two.clone(),
            },
            PatchEntry {
                table: crate::nshm::PatchTable::Git("https://github.com/acme/ui".into()),
                package: "acme-ui".into(),
                path: one.clone(),
            },
        ];
        write(
            &dir.join("runner"),
            App::TsxWithCrates {
                crates: &crates,
                patches: &lock_patches,
                workspace: &ws,
            },
            false,
            ApiLevel::Api20,
        )
        .unwrap();
        let manifest = fs::read_to_string(dir.join("runner").join("Cargo.toml")).unwrap();
        assert!(
            manifest.contains(&format!(
                "app_0 = {{ package = \"app-native\", path = {:?} }}",
                one.to_string_lossy()
            )),
            "{manifest}"
        );
        assert!(
            manifest.contains(&format!(
                "app_1 = {{ package = \"app-charts\", path = {:?}, features = [\"camera\"] }}",
                two.to_string_lossy()
            )),
            "{manifest}"
        );
        assert!(
            manifest.contains("nsh-core = { path = \"/src/nsh/crates/nsh-core\" }"),
            "{manifest}"
        );
        // Existing [patch.crates-io] is extended in place without duplicating the header.
        assert_eq!(
            manifest.matches("[patch.crates-io]").count(),
            1,
            "{manifest}"
        );
        assert!(
            manifest.contains(&format!(
                "app-charts = {{ path = {:?} }}",
                two.to_string_lossy()
            )),
            "{manifest}"
        );
        assert!(
            manifest.contains("[patch.\"https://github.com/acme/ui\"]"),
            "{manifest}"
        );
        assert!(!manifest.contains("features = [\"dev\"]"));
        let lib = fs::read_to_string(dir.join("runner").join("lib.rs")).unwrap();
        assert!(
            lib.contains("backend::app!(sets = [app_0::registry, app_1::registry]);"),
            "{lib}"
        );

        write(
            &dir.join("runner-dev"),
            App::TsxWithCrates {
                crates: &crates,
                patches: &lock_patches,
                workspace: &ws,
            },
            true,
            ApiLevel::Api20,
        )
        .unwrap();
        let dev_manifest = fs::read_to_string(dir.join("runner-dev").join("Cargo.toml")).unwrap();
        assert!(dev_manifest.contains("features = [\"dev\"]"));
        let expected_testing = format!(
            "nsh-testing = {{ path = {:?} }}",
            Path::new("/src/nsh/crates/nsh-core").with_file_name("nsh-testing")
        );
        assert!(dev_manifest.contains(&expected_testing), "{dev_manifest}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dev_runners_inherit_backend_pins_without_app_patches() {
        let dir = std::env::temp_dir().join(format!("nsh-hm-released-{}", std::process::id()));
        let app = dir.join("rust-app");
        fs::create_dir_all(&app).unwrap();
        fs::write(
            app.join("Cargo.toml"),
            "[package]\nname = \"counter-rust-app\"\nversion = \"0.0.0\"\n\n\
             [dependencies]\nnsh-core = \"0.6\"\n",
        )
        .unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"rust-app\"]\n",
        )
        .unwrap();
        write(
            &dir.join("runner-dev"),
            App::Rust(&app),
            true,
            ApiLevel::Api20,
        )
        .unwrap();
        let dev_manifest = fs::read_to_string(dir.join("runner-dev").join("Cargo.toml")).unwrap();
        assert!(dev_manifest.contains("features = [\"dev\"]"));
        assert_eq!(
            workspace_patches(&dev_manifest).trim(),
            crate::embedded::NSH_PATCH.trim(),
            "{dev_manifest}",
        );
        if crate::embedded::NSH_PATCH.contains("git = \"https://github.com/longbridge/nsh\"") {
            assert!(
                crate::embedded::NSH_PATCH.contains("rev = \""),
                "expected locked rev in NSH_PATCH:\n{}",
                crate::embedded::NSH_PATCH
            );
            assert!(
                !crate::embedded::NSH_PATCH.contains("branch = \"main\""),
                "unpinned branch in NSH_PATCH:\n{}",
                crate::embedded::NSH_PATCH
            );
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn run_cargo_drains_large_stderr_without_deadlock_and_extracts_cdylib() {
        let mut cmd = std::process::Command::new("python3");
        cmd.arg("-c").arg(
            r#"import sys
line = "   Compiling dep v0.1.0\n" * 4096
for _ in range(16):
    sys.stderr.write(line)
    sys.stderr.flush()
    sys.stdout.write('{"reason":"build-script-executed"}\n')
    sys.stdout.flush()
sys.stdout.write('{"reason":"compiler-artifact","target":{"name":"nsh_platform_harmonyos","kind":["cdylib"]},"filenames":["/tmp/out/libnsh_platform_harmonyos.so"]}\n')
sys.stdout.flush()
"#,
        );
        let so = run_cargo(&mut cmd, "arm64-v8a", "aarch64-unknown-linux-ohos", false).unwrap();
        assert_eq!(so, PathBuf::from("/tmp/out/libnsh_platform_harmonyos.so"));
    }

    #[cfg(unix)]
    #[test]
    fn run_cargo_preserves_failure_diagnostics_with_bounded_stderr() {
        let mut cmd = std::process::Command::new("python3");
        cmd.arg("-c").arg(
            r#"import sys
sys.stderr.write("   Compiling big v0.1.0\n" * 8000)
sys.stderr.write("error[E0425]: cannot find value `missing_sym` in this scope\n --> src/lib.rs:42:9\n")
sys.stderr.flush()
sys.exit(101)
"#,
        );
        let err = run_cargo(&mut cmd, "arm64-v8a", "aarch64-unknown-linux-ohos", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("error[E0425]"), "{err}");
        assert!(err.contains("src/lib.rs:42:9"), "{err}");
        assert!(err.len() <= STDERR_TAIL_LIMIT + 512, "len={}", err.len());
    }

    #[test]
    fn preserves_backend_nsh_pins_when_workspace_only_patches_third_party_crates() {
        let dir = std::env::temp_dir().join(format!("nsh-hm-patch-merge-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let ws = dir.join("ws");
        fs::create_dir_all(&ws).unwrap();
        fs::write(
            ws.join("Cargo.toml"),
            "[workspace]\n[patch.\"https://github.com/acme/kit\"]\nacme-kit = { path = \"/tmp/kit\" }\n",
        )
        .unwrap();
        let merged = patches_of(&ws);
        assert!(
            merged.contains("[patch.\"https://github.com/acme/kit\"]"),
            "{merged}"
        );
        assert!(
            merged.contains("acme-kit = { path = \"/tmp/kit\" }"),
            "{merged}"
        );
        if crate::embedded::NSH_PATCH.contains("nsh-core =") {
            assert!(merged.contains("nsh-core ="), "{merged}");
        }
        assert_eq!(toml_string("/tmp/项目/a\"b"), r#""/tmp/项目/a\"b""#);
        fs::remove_dir_all(&dir).unwrap();
    }
}
