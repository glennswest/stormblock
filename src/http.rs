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

/// Pick the process crypto provider once. `rustls` refuses to guess when
/// both `aws-lc-rs` and `ring` are compiled in — which a test build does,
/// through `reqwest` in dev-dependencies — so it is said here, explicitly,
/// before any TLS config is built (client or server).
pub fn ensure_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}

/// Builds a [`Client`].
pub struct ClientBuilder {
    timeout: Duration,
    root_pem: Vec<Vec<u8>>,
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
        Ok(Client { inner, timeout: self.timeout })
    }
}

/// A pooled HTTP(S) client. Cheap to clone; clones share the pool.
#[derive(Clone)]
pub struct Client {
    inner: Inner,
    timeout: Duration,
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
        ClientBuilder { timeout: Duration::from_secs(30), root_pem: Vec::new() }
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

    /// A deadline for this request alone.
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = Some(d);
        self
    }

    pub async fn send(self) -> Result<Response, Error> {
        let body = self.body?;
        let uri: hyper::Uri = self.url.parse().map_err(|e| Error::Url(format!("{}: {e}", self.url)))?;
        let mut req = hyper::Request::builder().method(self.method).uri(uri);
        if let Some(ct) = self.content_type {
            req = req.header(hyper::header::CONTENT_TYPE, ct);
        }
        req = req.header(hyper::header::USER_AGENT, concat!("stormblock/", env!("CARGO_PKG_VERSION")));
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
