//! Finite synchronous transport peer. No redirect, proxy, cache or automatic
//! retry can turn a retained intent into a second hidden network attempt.
use super::*;
use aq_observation_admission::{self as admission, Reply, Request, Status};
use reqwest::{
    Url,
    blocking::{Client, Response},
    header::{self, HeaderValue},
};
use std::{
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    time::Duration,
};
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpOwnerConfig {
    pub version: u32,
    pub origin: String,
    pub node_id: Uuid,
    pub boot_id: Uuid,
    pub credential_file: PathBuf,
    #[serde(default)]
    pub allow_loopback_http: bool,
}
impl HttpOwnerConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = read_private(path, admission::MAX_BYTES)?;
        let cfg: Self = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid private owner configuration"))?;
        cfg.endpoint()?;
        Ok(cfg)
    }
    fn endpoint(&self) -> Result<Url> {
        ensure!(
            self.version == 1 && !self.node_id.is_nil() && !self.boot_id.is_nil(),
            "owner configuration version/identity"
        );
        ensure!(self.origin.len() <= 2048, "owner endpoint bound");
        let u = Url::parse(&self.origin).map_err(|_| anyhow::anyhow!("invalid owner endpoint"))?;
        ensure!(
            u.username().is_empty()
                && u.password().is_none()
                && u.query().is_none()
                && u.fragment().is_none()
                && u.path() == "/"
                && u.port() != Some(0),
            "owner endpoint must be a bare origin"
        );
        let canonical = u.origin().ascii_serialization();
        ensure!(
            self.origin == canonical || self.origin == format!("{canonical}/"),
            "owner origin must be canonical"
        );
        let loopback = u
            .host_str()
            .and_then(|h| h.trim_matches(['[', ']']).parse::<std::net::IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback());
        ensure!(
            u.scheme() == "https" || (u.scheme() == "http" && self.allow_loopback_http && loopback),
            "owner HTTPS required; explicit loopback HTTP is fixture-only"
        );
        u.join("/v1/worker/observations/transport")
            .map_err(|_| anyhow::anyhow!("invalid owner endpoint"))
    }
}
/// Node-owned peer only. Credential values have no Debug/Serialize surface and
/// never enter LocalIntent, request bodies, error text or command arguments.
pub struct HttpOwnerPeer {
    client: Client,
    url: Url,
    node_id: Uuid,
    authorization: HeaderValue,
    /// Edge headers of the node's link (Cloudflare Access), sent on every request.
    headers: Vec<(header::HeaderName, HeaderValue)>,
}
fn client(timeout: Duration) -> Result<Client> {
    Client::builder()
        .http1_only()
        .pool_max_idle_per_host(0)
        .referer(false)
        .no_gzip()
        .no_brotli()
        .no_zstd()
        .no_deflate()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .no_proxy()
        .timeout(timeout)
        .connect_timeout(timeout.min(Duration::from_secs(3)))
        .build()
        .map_err(|_| anyhow::anyhow!("owner HTTP client construction failed"))
}
impl HttpOwnerPeer {
    pub fn from_config(config: &HttpOwnerConfig) -> Result<Self> {
        let url = config.endpoint()?;
        let bytes = read_private(&config.credential_file, 512)?;
        let bytes = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
        ensure!(
            bytes.starts_with(b"aqn_")
                && bytes.len() >= 16
                && bytes
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')),
            "invalid private node credential"
        );
        let mut authorization = HeaderValue::from_bytes(&[b"Bearer ".as_slice(), bytes].concat())
            .map_err(|_| anyhow::anyhow!("invalid node credential header"))?;
        authorization.set_sensitive(true);
        Ok(Self {
            client: client(Duration::from_secs(10))?,
            url,
            node_id: config.node_id,
            authorization,
            headers: vec![],
        })
    }
    /// The node service's peer: its enrolled server (HTTPS, or HTTP on loopback
    /// as the node's link allows), its bearer and its link's edge headers.
    pub fn for_node(
        server_url: &str,
        node_id: Uuid,
        bearer: &str,
        edge_headers: &[(&str, &str)],
        timeout: Duration,
    ) -> Result<Self> {
        let parsed = Url::parse(server_url).map_err(|_| anyhow::anyhow!("invalid server URL"))?;
        let url = HttpOwnerConfig {
            version: 1,
            origin: parsed.origin().ascii_serialization(),
            node_id,
            boot_id: node_id,
            credential_file: PathBuf::new(),
            allow_loopback_http: true,
        }
        .endpoint()?;
        ensure!(
            parsed.path() == "/" && parsed.username().is_empty() && parsed.query().is_none(),
            "server URL must be a bare origin"
        );
        let secret = |value: &str| -> Result<HeaderValue> {
            let mut value = HeaderValue::from_str(value)
                .map_err(|_| anyhow::anyhow!("invalid node link header"))?;
            value.set_sensitive(true);
            Ok(value)
        };
        let headers = edge_headers
            .iter()
            .map(|(name, value)| Ok((name.parse::<header::HeaderName>()?, secret(value)?)))
            .collect::<Result<_>>()?;
        Ok(Self {
            client: client(timeout)?,
            url,
            node_id,
            authorization: secret(&format!("Bearer {bearer}"))?,
            headers,
        })
    }
    /// One batch upload. Any error leaves the sealed packet for an identical retry.
    pub fn batch(&self, packet: &Packet) -> Result<admission::BatchReply> {
        let url = self.url.join("/v1/worker/observations/batch")?;
        let bytes = self.send(
            reqwest::Method::POST,
            url,
            Some(encode(packet)?),
            MAX_BATCH_BYTES,
        )?;
        let reply: admission::BatchReply = decode(&bytes)?;
        reply.validate_against(packet)?;
        Ok(reply)
    }
    fn request(&self, method: reqwest::Method, body: Option<Vec<u8>>) -> Result<Vec<u8>> {
        self.send(method, self.url.clone(), body, admission::MAX_BYTES)
    }
    fn send(
        &self,
        method: reqwest::Method,
        url: Url,
        body: Option<Vec<u8>>,
        bound: usize,
    ) -> Result<Vec<u8>> {
        let mut request = self
            .client
            .request(method, url)
            .header(header::AUTHORIZATION, self.authorization.clone())
            .header(header::ACCEPT, "application/json")
            .header(header::ACCEPT_ENCODING, "identity")
            .header(header::CACHE_CONTROL, "no-store");
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        if let Some(body) = body {
            ensure!(body.len() <= bound, "owner request bound");
            request = request
                .header(header::CONTENT_TYPE, "application/json")
                .body(body);
        }
        let response = request.send().map_err(|_| {
            anyhow::anyhow!("owner response unknown; retained intent requires explicit recovery")
        })?;
        bounded_response(response)
    }
}
impl AuthenticatedOwnerPeer for HttpOwnerPeer {
    fn mutate(&mut self, request: &Request) -> Result<Reply> {
        ensure!(
            request.node_id == self.node_id,
            "owner request node mismatch"
        );
        request.validate()?;
        let bytes = self.request(reqwest::Method::POST, Some(encode(request)?))?;
        admission::decode_reply(&bytes, request).map_err(|_| {
            anyhow::anyhow!("invalid owner reply; retained intent requires explicit recovery")
        })
    }
    fn status(&mut self, node_id: Uuid) -> Result<Status> {
        ensure!(node_id == self.node_id, "owner status node mismatch");
        let bytes = self.request(reqwest::Method::GET, None)?;
        let status = admission::decode_status(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid owner status"))?;
        ensure!(status.node_id == self.node_id, "owner status node mismatch");
        Ok(status)
    }
}
fn bounded_response(mut response: Response) -> Result<Vec<u8>> {
    // An HTTP error is not a claim that a mutation did not commit. LocalIntent
    // retains its exact operation and requires explicit caller recovery.
    ensure!(
        response.status() == reqwest::StatusCode::OK,
        "owner HTTP status {}; retained intent unchanged",
        response.status().as_u16()
    );
    ensure!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v
                .split(';')
                .next()
                .is_some_and(|x| x.trim().eq_ignore_ascii_case("application/json"))),
        "owner response content type"
    );
    ensure!(
        response
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v
                .split(',')
                .any(|p| p.trim().eq_ignore_ascii_case("no-store"))),
        "owner response must prohibit caching"
    );
    ensure!(
        response
            .headers()
            .get(header::CONTENT_ENCODING)
            .is_none_or(|v| v.as_bytes().eq_ignore_ascii_case(b"identity")),
        "encoded owner response refused"
    );
    ensure!(
        response
            .content_length()
            .is_none_or(|n| n <= admission::MAX_BYTES as u64),
        "owner response bound"
    );
    let mut bytes = vec![];
    response
        .by_ref()
        .take(admission::MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            anyhow::anyhow!("owner body unknown; retained intent requires explicit recovery")
        })?;
    ensure!(bytes.len() <= admission::MAX_BYTES, "owner response bound");
    Ok(bytes)
}
fn read_private(path: &Path, bound: usize) -> Result<Vec<u8>> {
    ensure!(path.is_absolute(), "absolute private owner input required");
    let parent = path.parent().context("private input parent")?;
    ensure!(
        std::fs::canonicalize(parent)? == parent,
        "canonical private input parent required"
    );
    let dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(parent)?;
    let m = dir.metadata()?;
    ensure!(
        m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
        "private owner input directory required"
    );
    let name = std::ffi::CString::new(
        path.file_name()
            .context("private input name")?
            .as_encoded_bytes(),
    )?;
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    ensure!(fd >= 0, "private owner input open failed");
    let mut file = unsafe { File::from_raw_fd(fd) };
    let m = file.metadata()?;
    ensure!(
        m.is_file()
            && m.nlink() == 1
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o077 == 0
            && m.len() <= bound as u64,
        "private owner input refused"
    );
    let mut bytes = vec![];
    file.by_ref()
        .take(bound as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= bound, "private owner input bound");
    let after = std::fs::symlink_metadata(parent)?;
    let held = dir.metadata()?;
    ensure!(
        after.dev() == held.dev()
            && after.ino() == held.ino()
            && std::fs::canonicalize(parent)? == parent,
        "private input parent changed"
    );
    Ok(bytes)
}
