//! Per-request cancellation tracking for ArkWeb custom URL scheme handlers.

use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

use nsh_core::web_resource::WebResourceCancel;
use rustc_hash::FxHashMap;

struct ActiveState {
    by_id: FxHashMap<usize, (usize, WebResourceCancel)>,
    by_native: FxHashMap<usize, usize>,
}

/// Tracks live custom-scheme requests individually so stopping one native
/// request never cancels another concurrent request for the same URL.
pub(crate) struct SchemeCancels {
    next_id: AtomicUsize,
    state: Mutex<ActiveState>,
}

impl Default for SchemeCancels {
    fn default() -> Self {
        Self {
            next_id: AtomicUsize::new(1),
            state: Mutex::new(ActiveState {
                by_id: FxHashMap::default(),
                by_native: FxHashMap::default(),
            }),
        }
    }
}

impl SchemeCancels {
    /// Allocates a unique non-zero request identifier.
    pub(crate) fn allocate_id(&self) -> usize {
        loop {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            if id != 0 {
                return id;
            }
        }
    }

    /// Registers `cancel` under `req_id` and its native request address `native_key`.
    pub(crate) fn register(&self, req_id: usize, native_key: usize, cancel: WebResourceCancel) {
        if req_id == 0 {
            return;
        }
        let mut guard = self.state.lock().expect("scheme cancels");
        if native_key != 0
            && let Some(stale_id) = guard.by_native.insert(native_key, req_id)
            && stale_id != req_id
        {
            guard.by_id.remove(&stale_id);
        }
        guard.by_id.insert(req_id, (native_key, cancel));
    }

    /// Removes `req_id` upon completion without cancelling it.
    pub(crate) fn finish(&self, req_id: usize) {
        if req_id == 0 {
            return;
        }
        let mut guard = self.state.lock().expect("scheme cancels");
        if let Some((native_key, _)) = guard.by_id.remove(&req_id)
            && native_key != 0
            && guard.by_native.get(&native_key).copied() == Some(req_id)
        {
            guard.by_native.remove(&native_key);
        }
    }

    /// Removes and cancels only the individual request matching `user_data_id`
    /// (from `OH_ArkWebResourceRequest_GetUserData`), falling back to `native_key`
    /// (`ArkWeb_ResourceRequest*` address) when `user_data_id` is unset.
    pub(crate) fn stop(&self, user_data_id: usize, native_key: usize) -> bool {
        let cancel = {
            let mut guard = self.state.lock().expect("scheme cancels");
            let resolved_id = if user_data_id != 0 {
                Some(user_data_id)
            } else if native_key != 0 {
                guard.by_native.remove(&native_key)
            } else {
                None
            };
            resolved_id
                .and_then(|id| guard.by_id.remove(&id).map(|(nk, c)| (id, nk, c)))
                .map(|(id, nk, cancel)| {
                    if nk != 0 && guard.by_native.get(&nk).copied() == Some(id) {
                        guard.by_native.remove(&nk);
                    }
                    cancel
                })
        };
        if let Some(cancel) = cancel {
            cancel.cancel();
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use nsh_core::web_resource::{WebResource, WebResourceRequest};

    use super::SchemeCancels;

    #[test]
    fn stopping_one_request_keeps_concurrent_same_url_request_live() {
        let cancels = Arc::new(SchemeCancels::default());
        let delivered_1: Arc<Mutex<Option<Option<WebResource>>>> = Arc::default();
        let delivered_2: Arc<Mutex<Option<Option<WebResource>>>> = Arc::default();

        let id_1 = cancels.allocate_id();
        let c1 = cancels.clone();
        let out_1 = delivered_1.clone();
        let (req_1, cancel_1) = WebResourceRequest::new("app-image:1", "GET", move |res| {
            c1.finish(id_1);
            *out_1.lock().unwrap() = Some(res);
        });
        cancels.register(id_1, 0x1000, cancel_1);

        let id_2 = cancels.allocate_id();
        let c2 = cancels.clone();
        let out_2 = delivered_2.clone();
        let (req_2, cancel_2) = WebResourceRequest::new("app-image:1", "GET", move |res| {
            c2.finish(id_2);
            *out_2.lock().unwrap() = Some(res);
        });
        cancels.register(id_2, 0x2000, cancel_2);

        // Stop only the first request (`id_1` / `0x1000`).
        assert!(cancels.stop(id_1, 0x1000));
        assert!(req_1.is_cancelled());
        assert!(!req_2.is_cancelled());

        // Answering the cancelled request drops its response; answering the second succeeds.
        req_1.respond(WebResource::new("image/png", &b"first"[..]));
        assert_eq!(*delivered_1.lock().unwrap(), None);

        req_2.respond(WebResource::new("image/png", &b"second"[..]));
        assert_eq!(
            *delivered_2.lock().unwrap(),
            Some(Some(WebResource::new("image/png", &b"second"[..])))
        );

        // ArkWeb's post-completion `on_request_stop` for `req_2` is a no-op.
        assert!(!cancels.stop(id_2, 0x2000));
    }

    #[test]
    fn native_pointer_fallback_and_address_reuse_do_not_cancel_newer_request() {
        let cancels = Arc::new(SchemeCancels::default());
        let delivered_2: Arc<Mutex<Option<Option<WebResource>>>> = Arc::default();

        let id_1 = cancels.allocate_id();
        let c1 = cancels.clone();
        let (req_1, cancel_1) = WebResourceRequest::new("app-image:1", "GET", move |_| {
            c1.finish(id_1);
        });
        cancels.register(id_1, 0x4000, cancel_1);

        let id_2 = cancels.allocate_id();
        let c2 = cancels.clone();
        let out_2 = delivered_2.clone();
        let (req_2, cancel_2) = WebResourceRequest::new("app-image:1", "GET", move |res| {
            c2.finish(id_2);
            *out_2.lock().unwrap() = Some(res);
        });
        cancels.register(id_2, 0x5000, cancel_2);

        // Fallback stop when user_data_id == 0 cancels only native_key 0x4000.
        assert!(cancels.stop(0, 0x4000));
        assert!(req_1.is_cancelled());
        assert!(!req_2.is_cancelled());

        // Even if a third request reuses freed address 0x4000 before `req_1` drops,
        // finishing `id_1` must not evict `id_3`.
        let id_3 = cancels.allocate_id();
        let (req_3, cancel_3) = WebResourceRequest::new("app-image:1", "GET", |_| {});
        cancels.register(id_3, 0x4000, cancel_3);
        drop(req_1);
        cancels.finish(id_1);
        assert!(!req_3.is_cancelled());
        assert!(cancels.stop(0, 0x4000));
        assert!(req_3.is_cancelled());

        req_2.respond(WebResource::new("text/plain", &b"ok"[..]));
        assert_eq!(
            *delivered_2.lock().unwrap(),
            Some(Some(WebResource::new("text/plain", &b"ok"[..])))
        );
    }
}
