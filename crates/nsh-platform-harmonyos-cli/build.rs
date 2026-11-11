//! Embeds the DevEco project template (`template/`) and the ArkTS package
//! (`crates/nsh-platform-harmonyos/arkts`) in the program, so an installed
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

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let repo = manifest.parent().unwrap().parent().unwrap();
    let mut code = String::new();
    for (name, dir) in [
        ("TEMPLATE", repo.join("template")),
        (
            "ARKTS",
            repo.join("crates")
                .join("nsh-platform-harmonyos")
                .join("arkts"),
        ),
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
    println!(
        "cargo:rerun-if-changed={}",
        repo.join("Cargo.toml").display()
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
                None => line.to_owned(),
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
