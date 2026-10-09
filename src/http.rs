//! A small HTTP client — the subset of `reqwest` this engine ever used, on
//! the `hyper` + `rustls` stack the management API already carries.
//!
//! `reqwest` was the single largest thing in the dependency graph (#79):
//! ~140 crates for what amounts to "POST some JSON, read the status and the
//! body". Everything that talks HTTP *out* of this process — cluster RPCs,
//! heartbeats, replication, migration, the StormFS announce — goes through
//! here, and the shape is kept close to `reqwest`'s so the call sites read
//! the same: `client.post(url).json(&req).send().await?`, then `status()`,
//! `json()` or `text()`.
//!
//! Connections are pooled by `hyper-util`'s legacy client, so a heartbeat or
//! a Raft append does not pay a handshake per call. TLS is `rustls` with the
//! WebPKI roots, plus whatever CA the cluster config names.

use std::fmt;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

type Connector = hyper_rustls::HttpsConnector<HttpConnector>;
type Inner = hyper_util::client::legacy::Client<Connector, Full<Bytes>>;

/// What went wrong with a request.
#[derive(Debug)]
pub enum Error {
    /// The URL could not be parsed.
    Url(String),
    /// The request could not be sent or the connection failed.
    Request(String),
    /// The request took longer than the client's timeout.
    Timeout(Duration),
    /// The body could not be read.
    Body(String),
    /// The body was not the JSON the caller expected.
    Json(String),
    /// A request body could not be serialised.
    Serialize(String),
    /// TLS could not be set up (a CA that does not parse, say).
    Tls(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Url(m) => write!(f, "bad url: {m}"),
            Error::Request(m) => write!(f, "request failed: {m}"),
            Error::Timeout(d) => write!(f, "request timed out after {d:?}"),
            Error::Body(m) => write!(f, "reading body: {m}"),
            Error::Json(m) => write!(f, "decoding json: {m}"),
            Error::Serialize(m) => write!(f, "encoding json: {m}"),
            Error::Tls(m) => write!(f, "tls: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl Error {
    /// Whether this is the network and not an answer (#359): a connection
    /// that failed, a timeout, a body cut off. A bad URL, JSON that does not
    /// decode or TLS that does not set up are the caller's.
    pub fn class(&self) -> crate::retry::Class {
        match self {
            Error::Request(_) | Error::Timeout(_) | Error::Body(_) => crate::retry::Class::Transient,
            Error::Url(_) | Error::Json(_) | Error::Serialize(_) | Error::Tls(_) => crate::retry::Class::Permanent,
        }
    }
}

/// An HTTP status, with the two questions callers ask of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusCode(pub u16);

impl StatusCode {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.0)
    }
    pub fn as_u16(&self) -> u16 {
        self.0
    }
}

impl fmt::Display for StatusCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Pick the process crypto provider once, before any TLS config is built
/// (client or server). ring is the only backend compiled in (#209); naming it
/// here keeps that true if a dependency ever brings a second one, which rustls
/// would otherwise refuse to choose between.
pub fn ensure_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

/// Builds a [`Client`].
pub struct ClientBuilder {
    timeout: Duration,
    root_pem: Vec<Vec<u8>>,
    bearer: Option<String>,
}

impl ClientBuilder {
    /// Whole-request deadline: connect, send, headers and body.
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    /// Trust this CA (PEM) in addition to the WebPKI roots.
    pub fn add_root_certificate_pem(mut self, pem: Vec<u8>) -> Self {
        self.root_pem.push(pem);
        self
    }

    /// Present this bearer token on every request unless one overrides it.
    ///
    /// For a client that only ever talks to one place — a peer node, an
    /// appliance — where forgetting it on one call out of five is a failure
    /// that only shows up under load.
    pub fn bearer(mut self, token: Option<String>) -> Self {
        self.bearer = token;
        self
    }

    pub fn build(self) -> Result<Client, Error> {
        ensure_crypto_provider();
        let mut roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        for pem in &self.root_pem {
            let mut rd = std::io::Cursor::new(pem);
            let certs: Vec<_> = rustls_pemfile::certs(&mut rd)
                .collect::<Result<_, _>>()
                .map_err(|e| Error::Tls(format!("reading CA pem: {e}")))?;
            if certs.is_empty() {
                return Err(Error::Tls("CA pem holds no certificate".into()));
            }
            for c in certs {
                roots.add(c).map_err(|e| Error::Tls(format!("adding CA: {e}")))?;
            }
        }
        let tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_or_http()
            .enable_http1()
            .build();
        let inner = hyper_util::client::legacy::Client::builder(TokioExecutor::new()).build(https);
        Ok(Client { inner, timeout: self.timeout, bearer: self.bearer })
    }
}

/// A pooled HTTP(S) client. Cheap to clone; clones share the pool.
#[derive(Clone)]
pub struct Client {
    inner: Inner,
    timeout: Duration,
    bearer: Option<String>,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    /// A client with a 30 s timeout and the WebPKI roots.
    pub fn new() -> Self {
        Self::builder().build().expect("default http client")
    }

    pub fn builder() -> ClientBuilder {
        ClientBuilder { timeout: Duration::from_secs(30), root_pem: Vec::new(), bearer: None }
    }

    pub fn post(&self, url: impl AsRef<str>) -> RequestBuilder {
        self.request(hyper::Method::POST, url.as_ref())
    }

    pub fn get(&self, url: impl AsRef<str>) -> RequestBuilder {
        self.request(hyper::Method::GET, url.as_ref())
    }

    pub fn delete(&self, url: impl AsRef<str>) -> RequestBuilder {
        self.request(hyper::Method::DELETE, url.as_ref())
    }

    pub fn put(&self, url: impl AsRef<str>) -> RequestBuilder {
        self.request(hyper::Method::PUT, url.as_ref())
    }

    fn request(&self, method: hyper::Method, url: &str) -> RequestBuilder {
        RequestBuilder {
            client: self.clone(),
            method,
            url: url.to_string(),
            body: Ok(Bytes::new()),
            content_type: None,
            timeout: None,
            bearer: self.bearer.clone(),
        }
    }
}

/// One request being put together.
pub struct RequestBuilder {
    client: Client,
    method: hyper::Method,
    url: String,
    body: Result<Bytes, Error>,
    content_type: Option<&'static str>,
    timeout: Option<Duration>,
    bearer: Option<String>,
}

impl RequestBuilder {
    /// Send `value` as the JSON body.
    pub fn json<T: serde::Serialize + ?Sized>(mut self, value: &T) -> Self {
        self.body = serde_json::to_vec(value)
            .map(Bytes::from)
            .map_err(|e| Error::Serialize(e.to_string()));
        self.content_type = Some("application/json");
        self
    }

    /// Send raw bytes as the body.
    pub fn body(mut self, bytes: impl Into<Bytes>) -> Self {
        self.body = Ok(bytes.into());
        self.content_type = Some("application/octet-stream");
        self
    }

    /// Present a bearer token, when there is one to present. `None` clears
    /// whatever the client was built with.
    ///
    /// Takes an `Option` because every caller of this has the same shape — a
    /// node that may or may not have been given a credential — and unwrapping
    /// it at each call site is how one of them ends up sending the literal
    /// string "None".
    pub fn bearer(mut self, token: Option<&str>) -> Self {
        self.bearer = token.map(|t| t.to_string());
        self
    }

    /// A deadline for this request alone.
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = Some(d);
        self
    }

    /// Send it. A GET (or HEAD) is retried under [`crate::retry::Policy::NETWORK`]
    /// (#359): a failed connection, a timeout, a 5xx, 408 or 429 is tried
    /// again, bounded, and the last answer (or error) is returned. Anything
    /// else is sent once: a POST, a PUT or a DELETE says what it changes, and
    /// a caller that knows it is safe to repeat uses [`send_retried`].
    ///
    /// [`send_retried`]: RequestBuilder::send_retried
    pub async fn send(self) -> Result<Response, Error> {
        if matches!(self.method, hyper::Method::GET | hyper::Method::HEAD) {
            self.send_retried(crate::retry::Policy::NETWORK).await
        } else {
            let body = match &self.body {
                Ok(b) => b.clone(),
                Err(e) => return Err(Error::Request(e.to_string())),
            };
            self.send_once(body).await
        }
    }

    /// Send it under `policy`, for a request the caller knows is safe to
    /// repeat (idempotent, or checked before it acts).
    pub async fn send_retried(self, policy: crate::retry::Policy) -> Result<Response, Error> {
        let body = match &self.body {
            Ok(b) => b.clone(),
            Err(e) => return Err(Error::Request(e.to_string())),
        };
        enum Why {
            Error(Error),
            Status(Response),
        }
        impl fmt::Display for Why {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self {
                    Why::Error(e) => write!(f, "{e}"),
                    Why::Status(r) => write!(f, "HTTP {}", r.status()),
                }
            }
        }
        let what = format!("{} {}", self.method, self.url);
        let r = crate::retry::with_backoff(
            &what,
            policy,
            |w: &Why| match w {
                Why::Error(e) => e.class(),
                Why::Status(r) => crate::retry::classify_status(r.status().as_u16()),
            },
            |_| {
                let body = body.clone();
                let me = &self;
                async move {
                    match me.send_once(body).await {
                        Ok(r) if crate::retry::classify_status(r.status().as_u16()) == crate::retry::Class::Transient => {
                            Err(Why::Status(r))
                        }
                        Ok(r) => Ok(r),
                        Err(e) => Err(Why::Error(e)),
                    }
                }
            },
        )
        .await;
        match r {
            Ok(resp) => Ok(resp),
            // The last answer, given up on: the caller sees the status.
            Err(f) => match f.error {
                Why::Status(resp) => Ok(resp),
                Why::Error(e) => Err(e),
            },
        }
    }

    async fn send_once(&self, body: Bytes) -> Result<Response, Error> {
        let uri: hyper::Uri = self.url.parse().map_err(|e| Error::Url(format!("{}: {e}", self.url)))?;
        let mut req = hyper::Request::builder().method(self.method.clone()).uri(uri);
        if let Some(ct) = self.content_type {
            req = req.header(hyper::header::CONTENT_TYPE, ct);
        }
        req = req.header(hyper::header::USER_AGENT, concat!("stormblock/", env!("CARGO_PKG_VERSION")));
        if let Some(t) = &self.bearer {
            req = req.header(hyper::header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let req = req.body(Full::new(body)).map_err(|e| Error::Request(e.to_string()))?;
        let deadline = self.timeout.unwrap_or(self.client.timeout);
        let fut = async {
            let resp = self.client.inner.request(req).await.map_err(|e| Error::Request(e.to_string()))?;
            let status = StatusCode(resp.status().as_u16());
            let body = resp
                .into_body()
                .collect()
                .await
                .map_err(|e| Error::Body(e.to_string()))?
                .to_bytes();
            Ok(Response { status, body })
        };
        match tokio::time::timeout(deadline, fut).await {
            Ok(r) => r,
            Err(_) => Err(Error::Timeout(deadline)),
        }
    }
}

impl Client {
    /// How big the thing at this URL says it is, if it says.
    ///
    /// A HEAD, because a streaming import has to create the volume before it
    /// has seen the last byte, and a volume needs a size.
    pub async fn content_length(&self, url: &str) -> Result<Option<u64>, Error> {
        let what = format!("HEAD {url}");
        crate::retry::with_backoff(&what, crate::retry::Policy::NETWORK, Error::class, |_| async {
            let uri: hyper::Uri = url.parse().map_err(|e| Error::Url(format!("{url}: {e}")))?;
            let req = hyper::Request::builder()
                .method(hyper::Method::HEAD)
                .uri(uri)
                .header(hyper::header::USER_AGENT, concat!("stormblock/", env!("CARGO_PKG_VERSION")))
                .body(Full::new(Bytes::new()))
                .map_err(|e| Error::Request(e.to_string()))?;
            // Bounded (#359): a HEAD that never answers held an import for ever.
            let resp = tokio::time::timeout(self.timeout, self.inner.request(req))
                .await
                .map_err(|_| Error::Timeout(self.timeout))?
                .map_err(|e| Error::Request(e.to_string()))?;
            let status = resp.status().as_u16();
            if crate::retry::classify_status(status) == crate::retry::Class::Transient {
                return Err(Error::Request(format!("{url}: HTTP {status}")));
            }
            if !(200..300).contains(&status) {
                return Ok(None);
            }
            Ok(resp
                .headers()
                .get(hyper::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok()))
        })
        .await
        .map_err(|f| f.error)
    }

    /// A GET from byte `from` on (a `Range` request when `from > 0`), bounded
    /// to the headers by the client's timeout. Answers whether the server
    /// resumed (206) or started over (200), and the body.
    async fn get_from(&self, url: &str, from: u64) -> Result<(bool, hyper::body::Incoming), Error> {
        let uri: hyper::Uri = url.parse().map_err(|e| Error::Url(format!("{url}: {e}")))?;
        let mut req = hyper::Request::builder()
            .method(hyper::Method::GET)
            .uri(uri)
            .header(hyper::header::USER_AGENT, concat!("stormblock/", env!("CARGO_PKG_VERSION")));
        if from > 0 {
            req = req.header(hyper::header::RANGE, format!("bytes={from}-"));
        }
        let req = req.body(Full::new(Bytes::new())).map_err(|e| Error::Request(e.to_string()))?;
        let resp = tokio::time::timeout(self.timeout, self.inner.request(req))
            .await
            .map_err(|_| Error::Timeout(self.timeout))?
            .map_err(|e| Error::Request(e.to_string()))?;
        let status = resp.status().as_u16();
        if crate::retry::classify_status(status) == crate::retry::Class::Transient {
            return Err(Error::Request(format!("{url}: HTTP {status}")));
        }
        if !(200..300).contains(&status) {
            return Err(Error::Url(format!("{url}: HTTP {status}")));
        }
        Ok((status == 206, resp.into_body()))
    }

    /// The next frame of a body, bounded by the client's timeout: an idle
    /// bound, not a total one (an image is gigabytes), so a server that
    /// stops sending is a timeout, never a wait for ever (#359).
    async fn next_frame(&self, body: &mut hyper::body::Incoming) -> Result<Option<Bytes>, Error> {
        use http_body_util::BodyExt as _;
        loop {
            match tokio::time::timeout(self.timeout, body.frame()).await {
                Err(_) => return Err(Error::Timeout(self.timeout)),
                Ok(None) => return Ok(None),
                Ok(Some(Err(e))) => return Err(Error::Body(e.to_string())),
                Ok(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        return Ok(Some(data));
                    }
                }
            }
        }
    }

    /// GET a URL and send the body onward in frames, as they arrive.
    ///
    /// **Nothing is staged.** A raw disk image is sequential — it can be
    /// decoded straight off the wire — so requiring a spool file means
    /// requiring room for the image's *whole virtual size* on a node about to
    /// store only the parts that are used. A 32 GB image with 9 GB in it
    /// failed with ENOSPC while the volume it was headed for had room three
    /// times over.
    ///
    /// A bounded channel rather than a callback: the consumer writes to a
    /// volume, which is async, and a synchronous sink would have to block the
    /// runtime to do it. The bound is the backpressure — a slow disk slows the
    /// download rather than growing a queue.
    ///
    /// A failure on the way (#359) is retried under
    /// [`crate::retry::Policy::TRANSFER`], **resuming** with a `Range` from
    /// the byte reached, so the consumer sees one continuous stream. A server
    /// that will not resume (answers 200 to the range) cannot be continued
    /// without sending the consumer bytes twice: that fails, said so.
    pub async fn get_to_channel(
        &self,
        url: &str,
        tx: tokio::sync::mpsc::Sender<Bytes>,
    ) -> Result<u64, Error> {
        let seen = std::sync::atomic::AtomicU64::new(0);
        let what = format!("GET {url} (streamed)");
        let classify = |e: &Error| match e {
            Error::Url(m) if m.starts_with("stopped:") => crate::retry::Class::Permanent,
            e => e.class(),
        };
        crate::retry::with_backoff(&what, crate::retry::Policy::TRANSFER, classify, |_| async {
            let from = seen.load(std::sync::atomic::Ordering::SeqCst);
            let (resumed, mut body) = self.get_from(url, from).await?;
            if from > 0 && !resumed {
                return Err(Error::Url(format!(
                    "stopped: {url} failed after {from} bytes and the server does not resume (no 206 to a Range)"
                )));
            }
            while let Some(data) = self.next_frame(&mut body).await? {
                seen.fetch_add(data.len() as u64, std::sync::atomic::Ordering::SeqCst);
                if tx.send(data).await.is_err() {
                    // The consumer gave up — say so rather than reading the
                    // rest of a body nobody wants.
                    return Err(Error::Url("stopped: the import stopped reading".into()));
                }
            }
            Ok(seen.load(std::sync::atomic::Ordering::SeqCst))
        })
        .await
        .map_err(|f| f.error)
    }

    /// Stream a GET into a file — a cloud image is hundreds of MB and a
    /// 256 MB node must never hold one in memory. Returns the bytes written.
    ///
    /// Retried under [`crate::retry::Policy::TRANSFER`] (#359), resuming with
    /// a `Range` from what the file holds (a server that answers 200 instead
    /// starts the file over). A download that gives up removes its partial
    /// file: an import whose download failed left it in `imports/`.
    pub async fn get_to_file(&self, url: &str, path: &std::path::Path) -> Result<u64, Error> {
        use tokio::io::AsyncWriteExt as _;
        let what = format!("GET {url} -> {}", path.display());
        let r = crate::retry::with_backoff(&what, crate::retry::Policy::TRANSFER, Error::class, |_| async {
            let have = tokio::fs::metadata(path).await.map(|m| m.len()).unwrap_or(0);
            let (resumed, mut body) = self.get_from(url, have).await?;
            let mut file = if resumed && have > 0 {
                tokio::fs::OpenOptions::new().append(true).open(path).await
            } else {
                tokio::fs::File::create(path).await
            }
            .map_err(|e| Error::Url(format!("{}: {e}", path.display())))?;
            let mut written = if resumed { have } else { 0 };
            while let Some(data) = self.next_frame(&mut body).await? {
                file.write_all(&data).await.map_err(|e| Error::Url(format!("{}: {e}", path.display())))?;
                written += data.len() as u64;
            }
            file.flush().await.map_err(|e| Error::Url(format!("{}: {e}", path.display())))?;
            Ok(written)
        })
        .await;
        match r {
            Ok(n) => Ok(n),
            Err(f) => {
                let _ = tokio::fs::remove_file(path).await;
                Err(f.error)
            }
        }
    }
}

/// A response, body already read.
#[derive(Debug, Clone)]
pub struct Response {
    status: StatusCode,
    body: Bytes,
}

impl Response {
    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub async fn json<T: serde::de::DeserializeOwned>(self) -> Result<T, Error> {
        serde_json::from_slice(&self.body).map_err(|e| Error::Json(e.to_string()))
    }

    pub async fn text(self) -> Result<String, Error> {
        Ok(String::from_utf8_lossy(&self.body).into_owned())
    }

    pub fn bytes(self) -> Bytes {
        self.body
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Json, Router};

    /// #359: a download cut off midway resumes with a `Range` and the file
    /// ends up whole; a GET answered 503 twice is tried again and succeeds.
    #[tokio::test]
    async fn a_download_cut_off_resumes_and_a_503_is_tried_again() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let body: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let served = body.clone();
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let seen = requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else { return };
                let n = seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body = served.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let got = s.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..got]).to_ascii_lowercase();
                    if req.starts_with("get /flaky") {
                        let (code, text) = if n < 2 { (503, "busy") } else { (200, "fine") };
                        let _ = s
                            .write_all(format!("HTTP/1.1 {code} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}", text.len()).as_bytes())
                            .await;
                        return;
                    }
                    let from = req
                        .lines()
                        .find_map(|l| l.strip_prefix("range: bytes="))
                        .and_then(|r| r.trim_end_matches('-').trim().trim_end_matches('-').parse::<usize>().ok());
                    match from {
                        None => {
                            // Promise the whole body, send a third, hang up.
                            let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len());
                            let _ = s.write_all(head.as_bytes()).await;
                            let _ = s.write_all(&body[..body.len() / 3]).await;
                        }
                        Some(f) => {
                            let rest = &body[f..];
                            let head = format!(
                                "HTTP/1.1 206 Partial\r\ncontent-length: {}\r\ncontent-range: bytes {f}-{}/{}\r\nconnection: close\r\n\r\n",
                                rest.len(),
                                body.len() - 1,
                                body.len()
                            );
                            let _ = s.write_all(head.as_bytes()).await;
                            let _ = s.write_all(rest).await;
                        }
                    }
                });
            }
        });
        let client = Client::builder().timeout(Duration::from_secs(5)).build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.raw");
        let n = client.get_to_file(&format!("http://{addr}/image"), &path).await.expect("resumed");
        assert_eq!(n, body.len() as u64);
        assert_eq!(std::fs::read(&path).unwrap(), body, "the file is whole after the resume");

        requests.store(0, std::sync::atomic::Ordering::SeqCst);
        let r = client.get(format!("http://{addr}/flaky")).send().await.unwrap();
        assert_eq!(r.status().as_u16(), 200, "a 503 is tried again");
        assert_eq!(r.text().await.unwrap(), "fine");
    }

    /// The client speaks to the server this binary runs, round trip.
    #[tokio::test]
    async fn posts_json_and_reads_it_back() {
        let app = Router::new().route(
            "/echo",
            post(|Json(v): Json<serde_json::Value>| async move { Json(serde_json::json!({ "got": v })) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = Client::builder().timeout(Duration::from_secs(5)).build().unwrap();
        let resp = client
            .post(format!("http://{addr}/echo"))
            .json(&serde_json::json!({ "a": 1 }))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
        let v: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(v["got"]["a"], 1);

        let resp = client.post(format!("http://{addr}/missing")).json(&1).send().await.unwrap();
        assert_eq!(resp.status().as_u16(), 404);
        assert!(!resp.status().is_success());
    }

    #[tokio::test]
    async fn a_dead_port_is_a_request_error_and_a_deadline_is_a_timeout() {
        let client = Client::builder().timeout(Duration::from_millis(200)).build().unwrap();
        let err = client.post("http://127.0.0.1:9/x").json(&1).send().await.unwrap_err();
        assert!(matches!(err, Error::Request(_) | Error::Timeout(_)), "{err}");
        let err = client.post("not a url").json(&1).send().await.unwrap_err();
        assert!(matches!(err, Error::Url(_)), "{err}");
    }
}
