//! One test connection's model and isolated filesystem. Input runs on the
//! computer through hdc; the UI thread only answers short scene queries.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use nsh_testing::agent::Model;

pub(crate) struct Session {
    pub(crate) model: Arc<Model>,
    pub(crate) directory: PathBuf,
}

impl Session {
    pub(crate) fn new(cache: &Path) -> napi_ohos::Result<Arc<Self>> {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).map_err(|e| napi_ohos::Error::from_reason(e.to_string()))?;
        let id = u128::from_le_bytes(bytes);
        let directory = cache.join("nsh-tests").join(format!("{id:032x}"));
        std::fs::create_dir_all(&directory)?;
        Ok(Arc::new(Self {
            model: Arc::default(),
            directory,
        }))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.directory)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!(
                "cannot remove test data {}: {error}",
                self.directory.display()
            );
        }
    }
}
