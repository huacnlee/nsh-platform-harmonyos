//! A host-selected asset directory; untrusted module URIs stay within it.

use std::{
    io::Read,
    path::{Component, Path, PathBuf},
};

const MAX_ASSET: u64 = 64 << 20;

/// The engine's assets, with the streaming reads the video player and web
/// views take large files with.
pub(crate) trait StreamAssets: nsh_engine::Assets {
    /// The file behind `uri` when it is one on disk, opened.
    fn open_file(&self, _uri: &str) -> Option<Result<std::fs::File, String>> {
        None
    }

    /// A reader of `uri`'s bytes.
    fn reader(&self, uri: &str) -> Result<Box<dyn Read>, String> {
        self.read(uri)
            .map(|bytes| Box::new(std::io::Cursor::new(bytes)) as Box<dyn Read>)
    }
}

/// Whether `bytes` are QuickJS bytecode (NSH's compiled bundles) rather than
/// source: bytecode starts with its format's version byte, which
/// `nsh_engine::BYTECODE_VERSION` names (`…/bc<N>/…`); source text never
/// starts with a control character.
pub(crate) fn is_bytecode(bytes: &[u8]) -> bool {
    let version = nsh_engine::BYTECODE_VERSION
        .split('/')
        .find_map(|part| part.strip_prefix("bc")?.parse::<u8>().ok());
    version.is_some_and(|version| bytes.first() == Some(&version))
}

pub(crate) struct HostAssets {
    host: std::sync::Arc<crate::app::session::Services>,
    directory: Option<AssetDirectory>,
}

impl HostAssets {
    pub(crate) fn new(
        host: std::sync::Arc<crate::app::session::Services>,
        directory: Option<AssetDirectory>,
    ) -> Self {
        Self { host, directory }
    }
}

impl StreamAssets for HostAssets {
    fn open_file(&self, uri: &str) -> Option<Result<std::fs::File, String>> {
        self.directory.as_ref().map(|directory| {
            self.host.check().map_err(|e| e.to_string())?;
            std::fs::File::open(directory.resolve(uri)?).map_err(|e| e.to_string())
        })
    }
    fn reader(&self, uri: &str) -> Result<Box<dyn Read>, String> {
        if let Some(file) = self.open_file(uri) {
            return file.map(|file| Box::new(file) as Box<dyn Read>);
        }
        let name = uri
            .strip_prefix("asset:/")
            .ok_or("expected an asset:/ URI")?;
        self.host
            .resource_reader(&format!("assets/{name}"))
            .map_err(|e| e.to_string())
    }
}

impl nsh_engine::Assets for HostAssets {
    fn read(&self, uri: &str) -> Result<Vec<u8>, String> {
        self.host.check().map_err(|error| error.to_string())?;
        if let Some(directory) = &self.directory {
            return directory.read(uri);
        }
        let name = uri
            .strip_prefix("asset:/")
            .ok_or("expected an asset:/ URI")?;
        crate::app::session::resource_name(name).map_err(|error| error.to_string())?;
        self.host
            .resource(&format!("assets/{name}"))
            .map_err(|error| error.to_string())
    }
}

#[derive(Clone)]
pub(crate) struct AssetDirectory(PathBuf);

impl AssetDirectory {
    pub(crate) fn new(path: &str) -> Result<Self, String> {
        Self::from_path(Path::new(path))
    }
    pub(crate) fn from_path(path: &Path) -> Result<Self, String> {
        let path = path.canonicalize().map_err(|e| e.to_string())?;
        if !path.is_dir() {
            return Err("assetDirectory must name a directory".into());
        }
        Ok(Self(path))
    }
    pub(crate) fn resolve(&self, uri: &str) -> Result<PathBuf, String> {
        let relative = Path::new(
            uri.strip_prefix("asset:/")
                .ok_or("expected an asset:/ URI")?,
        );
        if relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
            || relative.as_os_str().is_empty()
        {
            return Err("invalid asset path".into());
        }
        let path = self
            .0
            .join(relative)
            .canonicalize()
            .map_err(|e| e.to_string())?;
        if !path.starts_with(&self.0) {
            return Err("asset escapes its directory".into());
        }
        Ok(path)
    }
}

impl StreamAssets for AssetDirectory {
    fn open_file(&self, uri: &str) -> Option<Result<std::fs::File, String>> {
        Some(
            self.resolve(uri)
                .and_then(|path| std::fs::File::open(path).map_err(|e| e.to_string())),
        )
    }
}

impl nsh_engine::Assets for AssetDirectory {
    fn read(&self, uri: &str) -> Result<Vec<u8>, String> {
        let file = std::fs::File::open(self.resolve(uri)?).map_err(|e| e.to_string())?;
        let mut bytes = Vec::new();
        file.take(MAX_ASSET + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_ASSET {
            return Err("asset exceeds 64 MiB".into());
        }
        Ok(bytes)
    }
}
