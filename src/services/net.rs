//! Streaming HTTP transport behind `fetch`, on ureq (rustls with the webpki roots):
//! per-surface admission, per-request socket cancellation, and dedicated readers
//! for open streams.
//!
//! Matches the Linux transport (`nsh-platform-linux/src/net.rs`) and
//! `nsh-engine`'s cancellable `ureq` connector (`Cancellable` / `Tracked`).
//! Each request reads the system proxy through the network connection binding,
//! including bypass rules. Requests stream in both directions; cancellation
//! suppresses subsequent events and shuts down the request's active TCP socket
//! so blocking head, handshake, upload, or response-body reads exit promptly
//! and release their `RequestPermit`. Only connection and response-head waits
//! have deadlines, preserving long-lived idle event streams.

use std::{
    cell::RefCell,
    io::Read,
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicUsize, Ordering},
        mpsc::{Receiver, Sender, channel},
    },
    time::Duration,
};

use nsh_engine::{
    FetchEvent, FetchEvents, FetchTask, HttpBody, HttpCache, HttpHead, HttpRequest, Network,
};
use ureq::{
    Agent, RequestExt, ResponseExt, SendBody, http,
    unversioned::{
        resolver::DefaultResolver,
        transport::{
            Buffers, ConnectionDetails, Connector, DefaultConnector, NextTimeout, ShutdownHandle,
            Transport,
        },
    },
};

/// Bytes read from the response per chunk.
const CHUNK: usize = 64 << 10;

/// Redirects followed before the last response is handed on as it is.
const MAX_REDIRECTS: u32 = 20;

/// Maximum concurrent in-flight requests per surface.
const MAX_CONCURRENT_REQUESTS: usize = 32;

/// Worker-side snapshot of the host's current native proxy policy.
pub(crate) struct SystemProxy {
    proxy: Option<ureq::Proxy>,
}

impl SystemProxy {
    #[cfg(target_env = "ohos")]
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

    #[cfg(not(target_env = "ohos"))]
    pub(crate) fn read() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self { proxy: None })
    }

    pub(crate) fn value(&self) -> Option<ureq::Proxy> {
        self.proxy.clone()
    }
}

fn spawn_fetch(task: impl FnOnce() + Send + 'static) -> Result<(), String> {
    #[cfg(target_env = "ohos")]
    {
        crate::services::executor::Executor::fetch()?.submit(task)
    }
    #[cfg(not(target_env = "ohos"))]
    {
        std::thread::Builder::new()
            .name("nsh-fetch-test".into())
            .spawn(task)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

fn spawn_stream(task: impl FnOnce() + Send + 'static) -> Result<(), String> {
    #[cfg(target_env = "ohos")]
    {
        crate::services::executor::Executor::stream(task)
    }
    #[cfg(not(target_env = "ohos"))]
    {
        std::thread::Builder::new()
            .name("nsh-fetch-stream-test".into())
            .spawn(task)
            .map(|_| ())
            .map_err(|e| e.to_string())
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
            let config = Agent::config_builder()
                .http_status_as_error(false)
                .max_redirects(redirects)
                .max_redirects_will_error(false)
                .timeout_connect(Some(Duration::from_secs(30)))
                .timeout_recv_response(Some(Duration::from_secs(60)))
                .build();
            Agent::with_parts(config, Cancellable::default(), DefaultResolver::default())
        };
        Self {
            follow: agent(MAX_REDIRECTS),
            manual: agent(0),
            requests: Arc::default(),
        }
    }
}

impl Network for UreqNetwork {
    fn fetch(&self, request: HttpRequest, mut events: FetchEvents) -> Box<dyn FetchTask> {
        if request.cache == HttpCache::OnlyIfCached {
            events(FetchEvent::Failed(
                "no cached response (`cache: \"only-if-cached\"`)".into(),
            ));
            return Box::new(Task {
                control: Arc::default(),
                body: None,
            });
        }
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
                (count < MAX_CONCURRENT_REQUESTS).then_some(count + 1)
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
                    spawn_stream(run)
                } else {
                    spawn_fetch(run)
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

/// Shared between the engine's handle and the request's thread(s).
#[derive(Default)]
struct Control {
    state: Mutex<ControlState>,
    changed: Condvar,
}

#[derive(Default)]
struct ControlState {
    paused: bool,
    cancelled: bool,
    /// The socket the request is currently using, shut down by `cancel`.
    socket: Option<Weak<dyn ShutdownHandle>>,
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

    /// Registers `socket` with this request; if already cancelled, shuts it down
    /// immediately so in-flight handshake/CONNECT/read exits at once.
    fn attach(&self, socket: &Weak<dyn ShutdownHandle>) {
        let shutdown = {
            let mut s = self.state.lock().expect("control");
            if s.cancelled {
                socket.upgrade()
            } else {
                s.socket = Some(socket.clone());
                None
            }
        };
        if let Some(socket) = shutdown {
            socket.shutdown();
        }
    }

    /// Detaches `socket` (`None`: any) once this request no longer owns it.
    fn detach(&self, socket: Option<&Weak<dyn ShutdownHandle>>) {
        let mut s = self.state.lock().expect("control");
        if socket.is_none_or(|socket| s.socket.as_ref().is_some_and(|s| Weak::ptr_eq(s, socket))) {
            s.socket = None;
        }
    }
}

impl std::fmt::Debug for Control {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Control")
    }
}

thread_local! {
    /// The request running on this worker thread right now.
    #[allow(clippy::missing_const_for_thread_local)]
    static REQUEST: RefCell<Option<Arc<Control>>> = const { RefCell::new(None) };
}

/// Binds `control` into `REQUEST` for the current scope on a worker thread.
///
/// Clears `REQUEST` unconditionally on drop (including panic), and detaches
/// `control.socket` unless `handoff()` transferred ownership of the live
/// response socket to the dedicated stream reader thread.
struct RequestScope {
    control: Arc<Control>,
    detach_on_drop: bool,
}

impl RequestScope {
    fn enter(control: Arc<Control>) -> Self {
        REQUEST.with(|slot| *slot.borrow_mut() = Some(control.clone()));
        Self {
            control,
            detach_on_drop: true,
        }
    }

    fn handoff(&mut self) {
        self.detach_on_drop = false;
    }
}

impl Drop for RequestScope {
    fn drop(&mut self) {
        REQUEST.with(|slot| *slot.borrow_mut() = None);
        if self.detach_on_drop {
            self.control.detach(None);
        }
    }
}

/// ureq's default connector, keeping each connection's socket handle so that
/// cancelling a request interrupts its active blocking I/O immediately.
#[derive(Debug, Default)]
struct Cancellable(DefaultConnector);

impl Connector for Cancellable {
    type Out = Tracked;

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<()>,
    ) -> Result<Option<Tracked>, ureq::Error> {
        let socket: Arc<Mutex<Option<Weak<dyn ShutdownHandle>>>> = Arc::default();
        let register = details.register_shutdown.clone();
        let kept = socket.clone();
        let details = ConnectionDetails {
            uri: details.uri,
            addrs: details.addrs.clone(),
            config: details.config,
            request_level: details.request_level,
            resolver: details.resolver,
            now: details.now,
            timeout: details.timeout,
            current_time: details.current_time.clone(),
            run_connector: details.run_connector.clone(),
            // Called synchronously on the request thread as soon as the TCP
            // socket is created, interrupting TLS handshakes and proxy CONNECT too.
            register_shutdown: Arc::new(move |handle: Weak<dyn ShutdownHandle>| {
                REQUEST.with(|r| {
                    if let Some(control) = &*r.borrow() {
                        control.attach(&handle);
                    }
                });
                *kept.lock().expect("socket") = Some(handle.clone());
                register(handle)
            }),
        };
        let inner = self.0.connect(&details, chained)?;
        let socket = socket.lock().expect("socket").take();
        Ok(inner.map(|inner| Tracked {
            inner,
            socket,
            owner: None,
        }))
    }
}

/// A transport wrapper that associates the underlying socket with whichever
/// request is actively driving it, detaching before pooling or reuse.
#[derive(Debug)]
struct Tracked {
    inner: Box<dyn Transport>,
    socket: Option<Weak<dyn ShutdownHandle>>,
    owner: Option<Arc<Control>>,
}

impl Tracked {
    fn attach(&mut self) {
        if let Some(socket) = &self.socket {
            let control = REQUEST.with(|r| r.borrow().clone());
            if let Some(control) = control {
                control.attach(socket);
                self.owner = Some(control);
            }
        }
    }
}

impl Transport for Tracked {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.inner.buffers()
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.attach();
        self.inner.transmit_output(amount, timeout)
    }

    fn maybe_await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.attach();
        self.inner.maybe_await_input(timeout)
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.attach();
        self.inner.await_input(timeout)
    }

    fn is_open(&mut self) -> bool {
        // ureq queries is_open() before returning a connection to the pool and
        // before reusing it: detach so a late cancel never kills another request.
        if let Some(owner) = self.owner.take() {
            owner.detach(self.socket.as_ref());
        }
        self.inner.is_open()
    }

    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}

/// The engine's handle on a request.
struct Task {
    control: Arc<Control>,
    /// Chunks of a streamed body (`None`: the end).
    body: Option<Sender<Option<Vec<u8>>>>,
}

impl Task {
    fn cancel_inner(&mut self) {
        let socket = {
            let mut s = self.control.state.lock().expect("control");
            s.cancelled = true;
            s.socket.take().and_then(|w| w.upgrade())
        };
        if let Some(socket) = socket {
            socket.shutdown();
        }
        self.control.changed.notify_all();
        // Wakes a body reader waiting for its next chunk.
        if let Some(body) = self.body.take() {
            let _ = body.send(None);
        }
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        self.cancel_inner();
    }
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

    fn cancel(mut self: Box<Self>) {
        self.cancel_inner();
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
    let mut scope = RequestScope::enter(events.control.clone());
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
    if events.control.cancelled() {
        return Ok(());
    }
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
    if events.control.cancelled() {
        return Ok(());
    }
    let events = events.clone();
    spawn_stream(move || {
        // Hold `permit` until the reader finishes and drops `response`, so the
        // admission count strictly tracks live I/O threads.
        let _permit = permit;
        let _scope = RequestScope::enter(events.control.clone());
        if let Err(error) = read_response(response, &events) {
            events.send(FetchEvent::Failed(error.to_string()));
        }
    })
    .map_err(std::io::Error::other)?;
    // Socket ownership transferred cleanly to the stream reader thread; clear
    // TLS on this pool thread without detaching `control.socket`.
    scope.handoff();
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

#[cfg(test)]
mod tests {
    use std::{io::Write as _, net::TcpListener, sync::mpsc, time::Instant};

    use super::*;

    fn wait_zero_permits(requests: &Arc<AtomicUsize>, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while requests.load(Ordering::Acquire) != 0 {
            assert!(
                Instant::now() < deadline,
                "request permit leaked: still {}",
                requests.load(Ordering::Acquire)
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn get_request(url: String) -> HttpRequest {
        HttpRequest {
            method: "GET".into(),
            url,
            headers: Vec::new(),
            body: HttpBody::Empty,
            follow_redirects: true,
            cache: HttpCache::Default,
        }
    }

    #[test]
    fn cancel_interrupts_blocked_response_head_and_releases_permit() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut req = [0u8; 512];
            let _ = stream.read(&mut req);
            accepted_tx.send(()).unwrap();
            // Hold the connection open without writing any response headers.
            let _ = done_rx.recv_timeout(Duration::from_secs(5));
        });

        let net = UreqNetwork::new();
        let (ev_tx, ev_rx) = mpsc::channel();
        let task = net.fetch(
            get_request(format!("http://{addr}/slow-head")),
            Box::new(move |ev| {
                let _ = ev_tx.send(ev);
            }),
        );
        accepted_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(net.requests.load(Ordering::Acquire), 1);

        let started = Instant::now();
        task.cancel();
        wait_zero_permits(&net.requests, Duration::from_secs(2));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "head cancel took too long: {:?}",
            started.elapsed()
        );
        assert!(ev_rx.try_recv().is_err());

        let _ = done_tx.send(());
        server.join().unwrap();
    }

    #[test]
    fn cancel_interrupts_blocked_response_body_and_frees_permits_for_next_request() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (release_tx, release_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            // First request: sends headers + 1 chunk, then stalls forever.
            let (mut s1, _) = listener.accept().unwrap();
            let mut buf = [0u8; 512];
            let _ = s1.read(&mut buf);
            s1.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n")
                .unwrap();
            s1.flush().unwrap();

            // Second request: completes normally after first is cancelled.
            let (mut s2, _) = listener.accept().unwrap();
            let _ = s2.read(&mut buf);
            s2.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
            s2.flush().unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
        });

        let net = UreqNetwork::new();
        let (ev_tx, ev_rx) = mpsc::channel();
        let task = net.fetch(
            get_request(format!("http://{addr}/stream")),
            Box::new(move |ev| {
                let _ = ev_tx.send(ev);
            }),
        );
        let mut saw_chunk = false;
        while !saw_chunk {
            match ev_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                FetchEvent::Head(_) => {}
                FetchEvent::Data(bytes) => {
                    assert_eq!(bytes, b"hello");
                    saw_chunk = true;
                }
                other => panic!("unexpected event before chunk: {other:?}"),
            }
        }
        assert_eq!(net.requests.load(Ordering::Acquire), 1);

        // Now stream reader is blocked in `reader.read(&mut buf)`. Cancel must unblock it!
        task.cancel();
        wait_zero_permits(&net.requests, Duration::from_secs(2));

        // Subsequent request succeeds and releases its permit cleanly.
        let (ev2_tx, ev2_rx) = mpsc::channel();
        let _task2 = net.fetch(
            get_request(format!("http://{addr}/next")),
            Box::new(move |ev| {
                let _ = ev2_tx.send(ev);
            }),
        );
        let mut body = Vec::new();
        loop {
            match ev2_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                FetchEvent::Head(_) => {}
                FetchEvent::Data(chunk) => body.extend_from_slice(&chunk),
                FetchEvent::End => break,
                other => panic!("unexpected second event: {other:?}"),
            }
        }
        assert_eq!(body, b"ok");
        wait_zero_permits(&net.requests, Duration::from_secs(2));

        let _ = release_tx.send(());
        server.join().unwrap();
    }

    #[test]
    fn cancel_while_paused_and_streaming_upload_release_permits_and_keep_idle_stream_alive() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (release_tx, release_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            // 1. Paused stream cancel
            let (mut s1, _) = listener.accept().unwrap();
            let mut buf = [0u8; 512];
            let _ = s1.read(&mut buf);
            s1.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\n")
                .unwrap();
            s1.flush().unwrap();

            // 2. Streaming upload waiting for body chunks
            let (mut s2, _) = listener.accept().unwrap();
            let _ = s2.read(&mut buf);

            // 3. Long-lived idle uncancelled event stream (waits 150ms then sends chunk + EOF)
            let (mut s3, _) = listener.accept().unwrap();
            let _ = s3.read(&mut buf);
            s3.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .unwrap();
            s3.flush().unwrap();
            std::thread::sleep(Duration::from_millis(150));
            s3.write_all(b"4\r\nping\r\n0\r\n\r\n").unwrap();
            s3.flush().unwrap();

            let _ = release_rx.recv_timeout(Duration::from_secs(5));
        });

        let net = UreqNetwork::new();

        // 1. Pause then cancel
        let (tx1, rx1) = mpsc::channel();
        let mut t1 = net.fetch(
            get_request(format!("http://{addr}/paused")),
            Box::new(move |ev| {
                let _ = tx1.send(ev);
            }),
        );
        t1.pause(true);
        loop {
            if matches!(
                rx1.recv_timeout(Duration::from_secs(2)).unwrap(),
                FetchEvent::Head(_)
            ) {
                break;
            }
        }
        t1.cancel();
        wait_zero_permits(&net.requests, Duration::from_secs(2));

        // 2. Streaming upload waiting in BodyReader::read -> cancel
        let (tx2, rx2) = mpsc::channel();
        let t2 = net.fetch(
            HttpRequest {
                method: "POST".into(),
                url: format!("http://{addr}/upload"),
                headers: Vec::new(),
                body: HttpBody::Stream,
                follow_redirects: true,
                cache: HttpCache::Default,
            },
            Box::new(move |ev| {
                let _ = tx2.send(ev);
            }),
        );
        assert!(matches!(
            rx2.recv_timeout(Duration::from_secs(2)).unwrap(),
            FetchEvent::BodyWanted
        ));
        t2.cancel();
        wait_zero_permits(&net.requests, Duration::from_secs(2));

        // 3. Idle uncancelled stream stays open and receives delayed event + EOF
        let (tx3, rx3) = mpsc::channel();
        let _t3 = net.fetch(
            get_request(format!("http://{addr}/sse")),
            Box::new(move |ev| {
                let _ = tx3.send(ev);
            }),
        );
        let mut received = Vec::new();
        loop {
            match rx3.recv_timeout(Duration::from_secs(2)).unwrap() {
                FetchEvent::Head(_) => {}
                FetchEvent::Data(b) => received.extend_from_slice(&b),
                FetchEvent::End => break,
                other => panic!("unexpected sse event: {other:?}"),
            }
        }
        assert_eq!(received, b"ping");
        wait_zero_permits(&net.requests, Duration::from_secs(2));

        let _ = release_tx.send(());
        server.join().unwrap();
    }

    #[test]
    fn cancelling_finished_pooled_request_does_not_kill_reused_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nConnection: keep-alive\r\n\r\n")
                .unwrap();
            stream.flush().unwrap();
            // Second request should reuse the same keep-alive socket cleanly.
            let n = stream.read(&mut buf).unwrap();
            assert!(n > 0);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nreuse",
                )
                .unwrap();
            stream.flush().unwrap();
        });

        let net = UreqNetwork::new();
        let (tx1, rx1) = mpsc::channel();
        let t1 = net.fetch(
            get_request(format!("http://{addr}/first")),
            Box::new(move |ev| {
                let _ = tx1.send(ev);
            }),
        );
        loop {
            if matches!(
                rx1.recv_timeout(Duration::from_secs(2)).unwrap(),
                FetchEvent::End
            ) {
                break;
            }
        }
        wait_zero_permits(&net.requests, Duration::from_secs(2));
        // Late cancel on an already-finished request whose connection returned to the pool.
        t1.cancel();

        let (tx2, rx2) = mpsc::channel();
        let _t2 = net.fetch(
            get_request(format!("http://{addr}/second")),
            Box::new(move |ev| {
                let _ = tx2.send(ev);
            }),
        );
        let mut body = Vec::new();
        loop {
            match rx2.recv_timeout(Duration::from_secs(2)).unwrap() {
                FetchEvent::Head(_) => {}
                FetchEvent::Data(b) => body.extend_from_slice(&b),
                FetchEvent::End => break,
                other => panic!("unexpected reuse event: {other:?}"),
            }
        }
        assert_eq!(body, b"reuse");
        wait_zero_permits(&net.requests, Duration::from_secs(2));
        server.join().unwrap();
    }
}
