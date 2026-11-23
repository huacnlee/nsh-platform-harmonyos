//! Split modules and companion policy are scoped to the accepted HAP entry's directory.
//! Preloading is bounded and shared by source and trusted bytecode entries.
use std::sync::{Arc, Weak};

use nsh_engine::Bundle;

use crate::{
    app::{
        chunks_spec::{preload, valid_chunk_name},
        runtime::{Dispatcher, Message},
        session::Services,
    },
    services::executor::Executor,
};

pub(crate) struct ResourceChunks {
    host: Arc<Services>,
    prefix: String,
}

impl ResourceChunks {
    pub(crate) fn new(host: Arc<Services>, entry: &str) -> Arc<Self> {
        let prefix = entry
            .rsplit_once('/')
            .map_or(String::new(), |(p, _)| format!("{p}/"));
        Arc::new(Self { host, prefix })
    }
    pub(crate) fn read_chunk(&self, id: &str) -> Result<nsh_engine::Chunk, String> {
        if !valid_chunk_name(id) {
            return Err("expected a chunk file name".into());
        }
        let name = format!("{}{id}", self.prefix);
        let bytes = self.host.resource(&name).map_err(|e| e.to_string())?;
        let code = String::from_utf8(bytes).map_err(|_| "chunk source must be UTF-8")?;
        let mut chunk = nsh_engine::Chunk::source(id.to_owned(), code);
        if let Ok(bytes) = self.host.resource(&format!("{name}.map"))
            && let Ok(map) = String::from_utf8(bytes)
            && !map.is_empty()
        {
            chunk.source_map = Some(map.into());
        }
        Ok(chunk)
    }
    pub(crate) fn preload(&self, bundle: &mut Bundle) -> Result<(), String> {
        preload(bundle, |name| {
            self.host
                .resource(&format!("{}{name}", self.prefix))
                .map_err(|e| e.to_string())
        })
    }
    pub(crate) fn read_optional(&self, name: &str) -> Option<Vec<u8>> {
        self.host.resource(&format!("{}{name}", self.prefix)).ok()
    }
    pub(crate) fn load(self: &Arc<Self>, id: &str, route: Weak<Dispatcher>) -> bool {
        let id = id.to_owned();
        let loader = self.clone();
        let permit = match self.host.reserve_load() {
            Ok(p) => p,
            Err(e) => {
                if let Some(route) = route.upgrade() {
                    route.send(Message::Chunk(Err((id, e.to_string()))));
                }
                return true;
            }
        };
        let failed_route = route.clone();
        let failed_id = id.clone();
        let result = Executor::assets().and_then(|pool| {
            pool.submit(move || {
                let _permit = permit;
                let result = loader.read_chunk(&id);
                if let Some(route) = route.upgrade() {
                    route.send(Message::Chunk(result.map_err(|e| (id, e))));
                }
            })
        });
        if let Err(error) = result
            && let Some(route) = failed_route.upgrade()
        {
            route.send(Message::Chunk(Err((failed_id, error.to_string()))));
        }
        true
    }
}
