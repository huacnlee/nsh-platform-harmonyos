//! Streaming HTTP transport behind `fetch`, on ureq (rustls with the webpki roots):
//! per-surface admission and dedicated readers for open streams.
//!
//! Matches the Linux transport (`nsh-platform-linux/src/net.rs`).
//! Rust TLS keeps the same bounded redirects and streaming behavior as the
//! desktop transport. Each request reads the system proxy through the network
//! connection binding, including bypass rules. Cookie-store integration is separate.
//! OHOS owns worker admission and per-request native proxy lookup. Requests
//! stream in both directions; cancellation suppresses subsequent events, and
//! only connection and response-head waits have deadlines, preserving streams.
//!
//! HTTP error statuses are responses, not failures. A request thread
//! blocked in a read or write is not interrupted by `cancel` or `pause`: it
//! stops before its next chunk.

use std::{
    io::Read,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::{Receiver, Sender, channel},
    },
    time::Duration,
};

use nsh_engine::{FetchEvent, FetchEvents, FetchTask, HttpBody, HttpHead, HttpRequest, Network};
use ureq::{Agent, RequestExt, ResponseExt, SendBody, http};

/// Bytes read from the response per chunk.
const CHUNK: usize = 64 << 10;

/// Redirects followed before the last response is handed on as it is.
const MAX_REDIRECTS: u32 = 20;

/// Worker-side snapshot of the host's current native proxy policy.
pub(crate) struct SystemProxy {
    proxy: Option<ureq::Proxy>,
}
impl SystemProxy {
    pub(crate) fn read() -> Result<Self, Box<dyn std::error::Error>> {
        let system = ohos_net_connection_binding::get_default_http_proxy()?;
        let proxy = if system.host.is_empty() {
            None
        } else {
            let mut builder = ureq::Proxy::builder(ureq::ProxyProtocol::Http)
                .host(&system.host)
                .port(system.port);
            for exclusion in &system.exclusion_list {
                builder = builder.no_proxy(exclusion);
            }
            Some(builder.build()?)
        };
        Ok(Self { proxy })
    }
    pub(crate) fn value(&self) -> Option<ureq::Proxy> {
        self.proxy.clone()
    }
}

/// Each engine owns its connection reuse and admission budget.
pub(crate) fn network() -> Arc<dyn Network> {
    Arc::new(UreqNetwork::new())
}

/// `fetch` on two ureq agents: one follows redirects, one hands on the 3xx
/// response (`redirect: "manual"`).
struct UreqNetwork {
    follow: Agent,
    manual: Agent,
    requests: Arc<AtomicUsize>,
}

impl UreqNetwork {
    fn new() -> Self {
        let agent = |redirects| {
            Agent::config_builder()
                .http_status_as_error(false)
                .max_redirects(redirects)
                .max_redirects_will_error(false)
                .timeout_connect(Some(Duration::from_secs(30)))
                .timeout_recv_response(Some(Duration::from_secs(60)))
                .build()
                .into()
        };
        Self {
            follow: agent(MAX_REDIRECTS),
            manual: agent(0),
            requests: Arc::default(),
        }
    }
}

impl Network for UreqNetwork {
    fn fetch(&self, request: HttpRequest, events: FetchEvents) -> Box<dyn FetchTask> {
        let agent = if request.follow_redirects {
            self.follow.clone()
        } else {
            self.manual.clone()
        };
        let control = Arc::new(Control::default());
        let (body_tx, body_rx) = match request.body {
            HttpBody::Stream => {
                let (tx, rx) = channel();
                (Some(tx), Some(rx))
            }
            _ => (None, None),
        };
        let events = Events {
            sink: Arc::new(Mutex::new(events)),
            control: control.clone(),
        };
        let spawned = self
            .requests
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < 32).then_some(count + 1)
            })
            .map_err(|_| "too many concurrent requests on this surface".to_owned())
            .and_then(|_| {
                let permit = RequestPermit(self.requests.clone());
                let streaming_upload = matches!(request.body, HttpBody::Stream);
                let run = {
                    let events = events.clone();
                    move || {
                        if let Err(e) = run(&agent, request, body_rx, &events, permit) {
                            events.send(FetchEvent::Failed(e.to_string()));
                        }
                    }
                };
                if streaming_upload {
                    crate::services::executor::Executor::stream(run)
                } else {
                    crate::services::executor::Executor::fetch()?.submit(run)
                }
            });
        if let Err(e) = spawned {
            events.send(FetchEvent::Failed(format!("cannot start the request: {e}")));
        }
        Box::new(Task {
            control,
            body: body_tx,
        })
    }
}

struct RequestPermit(Arc<AtomicUsize>);
impl Drop for RequestPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Shared between the engine's handle and the request's thread.
#[derive(Default)]
struct Control {
    state: Mutex<ControlState>,
    changed: Condvar,
}

#[derive(Default)]
struct ControlState {
    paused: bool,
    cancelled: bool,
}

impl Control {
    /// Waits while paused; false once cancelled.
    fn proceed(&self) -> bool {
        let mut s = self.state.lock().expect("control");
        while s.paused && !s.cancelled {
            s = self.changed.wait(s).expect("control");
        }
        !s.cancelled
    }

    fn cancelled(&self) -> bool {
        self.state.lock().expect("control").cancelled
    }
}

/// The engine's handle on a request.
struct Task {
    control: Arc<Control>,
    /// Chunks of a streamed body (`None`: the end).
    body: Option<Sender<Option<Vec<u8>>>>,
}

impl FetchTask for Task {
    fn send(&mut self, chunk: Vec<u8>) {
        if let Some(body) = &self.body {
            let _ = body.send(Some(chunk));
        }
    }

    fn finish(&mut self) {
        if let Some(body) = self.body.take() {
            let _ = body.send(None);
        }
    }

    fn pause(&mut self, paused: bool) {
        self.control.state.lock().expect("control").paused = paused;
        self.control.changed.notify_all();
    }

    fn cancel(self: Box<Self>) {
        self.control.state.lock().expect("control").cancelled = true;
        self.control.changed.notify_all();
        // Wakes a body reader waiting for its next chunk.
        if let Some(body) = &self.body {
            let _ = body.send(None);
        }
    }
}

/// The request's events, also reported from within ureq (the body reader);
/// silent once the request is cancelled.
#[derive(Clone)]
struct Events {
    sink: Arc<Mutex<FetchEvents>>,
    control: Arc<Control>,
}

impl Events {
    fn send(&self, event: FetchEvent) {
        let mut sink = self.sink.lock().expect("events");
        if !self.control.cancelled() {
            sink(event);
        }
    }
}

/// A streamed body: asks for each chunk, then hands it to ureq (which sends
/// it with chunked transfer encoding, the length being unknown).
struct BodyReader {
    chunks: Receiver<Option<Vec<u8>>>,
    current: Vec<u8>,
    at: usize,
    events: Events,
    done: bool,
}

impl Read for BodyReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.at == self.current.len() {
            if self.done {
                return Ok(0);
            }
            self.events.send(FetchEvent::BodyWanted);
            match self.chunks.recv() {
                Ok(Some(chunk)) => {
                    self.current = chunk;
                    self.at = 0;
                }
                Ok(None) | Err(_) => {
                    self.done = true;
                    // A cancelled upload must not end as a complete body.
                    if self.events.control.cancelled() {
                        return Err(std::io::Error::other("cancelled"));
                    }
                    return Ok(0);
                }
            }
        }
        let n = buf.len().min(self.current.len() - self.at);
        buf[..n].copy_from_slice(&self.current[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// Makes the request and reports its response, on the request's thread.
fn run(
    agent: &Agent,
    request: HttpRequest,
    body: Option<Receiver<Option<Vec<u8>>>>,
    events: &Events,
    permit: RequestPermit,
) -> Result<(), Box<dyn std::error::Error>> {
    if events.control.cancelled() {
        return Ok(());
    }
    let requested: http::Uri = request.url.parse()?;
    // Query on the request worker, so proxy changes apply without rebuilding an
    // engine or blocking its thread. A failed query must not bypass host policy.
    let proxy = SystemProxy::read()?.value();
    let mut builder = http::Request::builder()
        .method(request.method.as_str())
        .uri(requested.clone());
    for (name, value) in &request.headers {
        builder = builder.header(name, value);
    }
    let response = match (request.body, body) {
        (HttpBody::Bytes(bytes), _) => builder
            .body(bytes)?
            .with_agent(agent)
            .configure()
            .proxy(proxy)
            .build()
            .run()?,
        (HttpBody::Stream, Some(chunks)) => {
            let reader = BodyReader {
                chunks,
                current: Vec::new(),
                at: 0,
                events: events.clone(),
                done: false,
            };
            builder
                .body(SendBody::from_owned_reader(reader))?
                .with_agent(agent)
                .configure()
                .proxy(proxy)
                .build()
                .run()?
        }
        _ => builder
            .body(())?
            .with_agent(agent)
            .configure()
            .proxy(proxy)
            .build()
            .run()?,
    };
    let url = response.get_uri();
    let redirected = *url != requested;
    let url = url.to_string();
    let headers = response
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    let status = response.status();
    events.send(FetchEvent::Head(HttpHead {
        status: status.as_u16(),
        status_text: status.canonical_reason().unwrap_or_default().to_owned(),
        url,
        redirected,
        headers,
    }));
    let events = events.clone();
    crate::services::executor::Executor::stream(move || {
        let _permit = permit;
        if let Err(error) = read_response(response, &events) {
            events.send(FetchEvent::Failed(error.to_string()));
        }
    })
    .map_err(std::io::Error::other)?;
    Ok(())
}

fn read_response(mut response: http::Response<ureq::Body>, events: &Events) -> std::io::Result<()> {
    let mut reader = response.body_mut().with_config().limit(u64::MAX).reader();
    let mut buf = vec![0; CHUNK];
    loop {
        if !events.control.proceed() {
            return Ok(());
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        events.send(FetchEvent::Data(buf[..n].to_vec()));
    }
    events.send(FetchEvent::End);
    Ok(())
}
