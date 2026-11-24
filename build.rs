//! For OHOS, sets up the backend's N-API link. For the computer, embeds the
//! DevEco project template (`template/`) and the ArkTS package
//! (`arkts/`) in the program, so an installed
//! `nsh-platform-harmonyos` writes projects without this repository.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

fn files(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if matches!(
            name,
            "oh_modules" | "node_modules" | "build" | ".hvigor" | ".DS_Store"
        ) {
            continue;
        }
        if path.is_dir() {
            files(root, &path, out);
        } else {
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            out.push((relative, path));
        }
    }
}

fn join_relative(base: &Path, relative: &str) -> PathBuf {
    let candidate = Path::new(relative);
    if candidate.is_absolute() {
        return candidate.to_path_buf();
    }
    relative
        .split(['/', '\\'])
        .filter(|part| !part.is_empty() && *part != ".")
        .fold(base.to_path_buf(), |path, part| path.join(part))
}

fn locked_git_rev(lockfile: &str, url: &str, branch: &str) -> Option<String> {
    let prefix = format!("source = \"git+{url}?branch={branch}#");
    for line in lockfile.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(&prefix)
            && let Some(rev) = rest.strip_suffix('"')
            && !rev.is_empty()
        {
            return Some(rev.to_owned());
        }
    }
    None
}

fn pin_git_branch(line: &str, lockfile: &str) -> String {
    if line.contains("rev = \"") {
        return line.to_owned();
    }
    let Some((_, git_rest)) = line.split_once("git = \"") else {
        return line.to_owned();
    };
    let Some((url, _)) = git_rest.split_once('"') else {
        return line.to_owned();
    };
    let Some((_, branch_rest)) = line.split_once("branch = \"") else {
        return line.to_owned();
    };
    let Some((branch, _)) = branch_rest.split_once('"') else {
        return line.to_owned();
    };
    let Some(rev) = locked_git_rev(lockfile, url, branch) else {
        return line.to_owned();
    };
    line.replacen(
        &format!("branch = \"{branch}\""),
        &format!("rev = \"{rev}\""),
        1,
    )
}

fn main() {
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("ohos") {
        // The backend: the OHOS N-API linker setup. khronos-egl with
        // no-pkg-config leaves the platform library to its host.
        napi_build_ohos::setup();
        println!("cargo:rustc-link-lib=EGL");
        return;
    }
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let repo = manifest.as_path();
    let mut code = String::new();
    for (name, dir) in [
        ("TEMPLATE", repo.join("template")),
        ("ARKTS", repo.join("arkts")),
    ] {
        println!("cargo:rerun-if-changed={}", dir.display());
        let mut list = Vec::new();
        files(&dir, &dir, &mut list);
        code.push_str(&format!("pub(crate) const {name}: &[(&str, &[u8])] = &[\n"));
        for (relative, path) in list {
            println!("cargo:rerun-if-changed={}", path.display());
            code.push_str(&format!(
                "    ({relative:?}, include_bytes!({:?})),\n",
                path.display().to_string()
            ));
        }
        code.push_str("];\n");
    }
    // NSH's crates as this repository pins them: the runner of a TSX app,
    // which has no generated workspace of NSH's, takes them from here.
    let workspace = fs::read_to_string(repo.join("Cargo.toml")).unwrap();
    let lockfile = fs::read_to_string(repo.join("Cargo.lock")).unwrap_or_default();
    println!(
        "cargo:rerun-if-changed={}",
        repo.join("Cargo.toml").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        repo.join("Cargo.lock").display()
    );
    let mut patch = String::new();
    let mut inside = false;
    for line in workspace.lines() {
        if line.starts_with('[') {
            inside = line.starts_with("[patch");
        }
        if inside {
            // A path is relative to this repository: make it absolute.
            let line = match line.split_once("path = \"") {
                Some((head, rest)) => {
                    let (path, tail) = rest.split_once('"').unwrap();
                    let path = join_relative(repo, path);
                    let path = path.canonicalize().unwrap_or(path);
                    format!("{head}path = {:?}{tail}", path.display().to_string())
                }
                None => pin_git_branch(line, &lockfile),
            };
            patch.push_str(&line);
            patch.push('\n');
        }
    }
    code.push_str(&format!("pub(crate) const NSH_PATCH: &str = {patch:?};\n"));
    fs::write(
        PathBuf::from(env::var("OUT_DIR").unwrap()).join("embedded.rs"),
        code,
    )
    .unwrap();
}
