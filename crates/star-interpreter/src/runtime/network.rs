//! Capability-gated TCP, UDP, and HTTP host APIs.
//!
//! Network access is deny by default. A trusted host must grant `net.connect`
//! or `net.listen` and add exact socket endpoints or DNS names to the relevant
//! allowlist before guest code can use the service. HTTP client DNS results are
//! pinned when a host grants a name, preventing later DNS rebinding. Proxies
//! and redirects are disabled so requests cannot escape the checked endpoint.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{
    IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket,
};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use http::{HeaderName, HeaderValue, Method, Request, StatusCode, Uri};
use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};
use ureq::{Agent, Error as UreqError};

use super::host::Capability;

const MAX_RESOLVED_ADDRESSES: usize = 16;
const MAX_HTTP_HEADER_BYTES: usize = 64 * 1024;
const MAX_HTTP_HEADER_COUNT: usize = 128;
const HTTP_SOCKET_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// A denied operation or failed socket/HTTP operation.
#[derive(Debug)]
pub enum NetworkError {
    /// The operation requires a capability that the host did not grant.
    PermissionDenied(Capability),
    /// The socket endpoint is not present in the corresponding host allowlist.
    EndpointNotAllowed(SocketAddr),
    /// The host name was invalid or did not resolve during trusted setup.
    HostResolution(io::Error),
    /// An endpoint string or URL was invalid.
    InvalidEndpoint(String),
    /// A requested HTTP method or header was malformed.
    InvalidHttpValue(String),
    /// An operation would exceed the caller's byte limit.
    BodyTooLarge { limit: usize },
    /// An HTTP header block exceeded its byte limit.
    HeadersTooLarge { limit: usize },
    /// An HTTP message exceeded its header-count limit.
    TooManyHeaders { limit: usize },
    /// An HTTP message was malformed or used unsupported framing.
    HttpProtocolError(String),
    /// A socket operation failed.
    Io(io::Error),
    /// An HTTP client operation failed.
    Http(UreqError),
    /// The endpoint allowlist lock was poisoned by a host panic.
    PolicyPoisoned,
}

impl fmt::Display for NetworkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PermissionDenied(capability) => {
                write!(formatter, "missing {} capability", capability.name())
            }
            Self::EndpointNotAllowed(address) => {
                write!(formatter, "network endpoint {address} is not allowed")
            }
            Self::HostResolution(error) => write!(formatter, "host resolution failed: {error}"),
            Self::InvalidEndpoint(endpoint) => {
                write!(formatter, "invalid network endpoint: {endpoint}")
            }
            Self::InvalidHttpValue(value) => write!(formatter, "invalid HTTP value: {value}"),
            Self::BodyTooLarge { limit } => {
                write!(formatter, "HTTP body exceeds the {limit}-byte limit")
            }
            Self::HeadersTooLarge { limit } => {
                write!(formatter, "HTTP headers exceed the {limit}-byte limit")
            }
            Self::TooManyHeaders { limit } => {
                write!(formatter, "HTTP headers exceed the {limit}-header limit")
            }
            Self::HttpProtocolError(message) => formatter.write_str(message),
            Self::Io(error) => write!(formatter, "network I/O failed: {error}"),
            Self::Http(error) => write!(formatter, "HTTP request failed: {error}"),
            Self::PolicyPoisoned => formatter.write_str("network policy lock is poisoned"),
        }
    }
}

impl std::error::Error for NetworkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::HostResolution(error) | Self::Io(error) => Some(error),
            Self::Http(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for NetworkError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Default)]
struct NetworkPolicy {
    connect_addresses: BTreeSet<SocketAddr>,
    listen_addresses: BTreeSet<SocketAddr>,
    http_hosts: BTreeMap<(String, u16), Vec<SocketAddr>>,
}

/// Host-configured network APIs for JSON-adjacent protocols and sockets.
///
/// Capabilities alone do not authorize a destination: the host must also add
/// the exact address or resolved host/port to an allowlist. The type is created
/// by [`crate::runtime::host::HostServices`] and shares its immutable grants.
pub struct NetworkServices {
    capabilities: BTreeSet<Capability>,
    policy: Arc<RwLock<NetworkPolicy>>,
    http_agent: Agent,
}

impl NetworkServices {
    pub(crate) fn new(capabilities: BTreeSet<Capability>) -> Self {
        let policy = Arc::new(RwLock::new(NetworkPolicy::default()));
        let resolver = PinnedResolver {
            policy: Arc::clone(&policy),
        };
        let config = Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .allow_non_standard_methods(true)
            .max_response_header_size(MAX_HTTP_HEADER_BYTES)
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(30)))
            .build();
        let http_agent = Agent::with_parts(config, DefaultConnector::default(), resolver);
        Self {
            capabilities,
            policy,
            http_agent,
        }
    }

    /// Adds one exact IPv4 or IPv6 endpoint to the outbound allowlist.
    pub fn allow_connect_address(&self, address: SocketAddr) -> Result<(), NetworkError> {
        self.require(Capability::NetConnect)?;
        self.policy
            .write()
            .map_err(|_| NetworkError::PolicyPoisoned)?
            .connect_addresses
            .insert(address);
        Ok(())
    }

    /// Resolves and pins a host's current addresses for outbound TCP/UDP and
    /// HTTP use. The name is resolved only during trusted host configuration.
    pub fn allow_connect_host(
        &self,
        host: &str,
        port: u16,
    ) -> Result<Vec<SocketAddr>, NetworkError> {
        self.require(Capability::NetConnect)?;
        let addresses = resolve_host(host, port)?;
        if addresses.is_empty() {
            return Err(NetworkError::InvalidEndpoint(format!("{host}:{port}")));
        }
        let mut policy = self
            .policy
            .write()
            .map_err(|_| NetworkError::PolicyPoisoned)?;
        for address in &addresses {
            policy.connect_addresses.insert(*address);
        }
        policy
            .http_hosts
            .insert((normalize_host(host), port), addresses.clone());
        Ok(addresses)
    }

    /// Returns the addresses pinned for an explicitly allowed host. This is a
    /// lookup of the host's saved resolution, not a new DNS query.
    pub fn resolve_allowed_host(
        &self,
        host: &str,
        port: u16,
    ) -> Result<Vec<SocketAddr>, NetworkError> {
        self.require(Capability::NetConnect)?;
        let policy = self
            .policy
            .read()
            .map_err(|_| NetworkError::PolicyPoisoned)?;
        if let Some(addresses) = policy.http_hosts.get(&(normalize_host(host), port)) {
            return Ok(addresses.clone());
        }
        if let Ok(address) = host.parse::<IpAddr>() {
            let endpoint = SocketAddr::new(address, port);
            if policy.connect_addresses.contains(&endpoint) {
                return Ok(vec![endpoint]);
            }
        }
        Err(NetworkError::InvalidEndpoint(format!(
            "{host}:{port} is not in the outbound allowlist"
        )))
    }

    /// Adds one exact endpoint to the inbound TCP/UDP allowlist.
    pub fn allow_listen_address(&self, address: SocketAddr) -> Result<(), NetworkError> {
        self.require(Capability::NetListen)?;
        self.policy
            .write()
            .map_err(|_| NetworkError::PolicyPoisoned)?
            .listen_addresses
            .insert(address);
        Ok(())
    }

    /// Connects a TCP socket to an allowlisted endpoint.
    pub fn tcp_connect(
        &self,
        address: SocketAddr,
        timeout: Duration,
    ) -> Result<TcpStream, NetworkError> {
        self.require(Capability::NetConnect)?;
        self.check_connect(address)?;
        Ok(TcpStream::connect_timeout(&address, timeout)?)
    }

    /// Binds a TCP listener to an allowlisted local endpoint.
    pub fn tcp_listen(&self, address: SocketAddr) -> Result<TcpListener, NetworkError> {
        self.require(Capability::NetListen)?;
        self.check_listen(address)?;
        Ok(TcpListener::bind(address)?)
    }

    /// Creates a connected UDP socket whose peer is allowlisted.
    pub fn udp_connect(&self, peer: SocketAddr) -> Result<UdpSocketHandle, NetworkError> {
        self.require(Capability::NetConnect)?;
        self.check_connect(peer)?;
        let local = unspecified_address(peer.is_ipv6());
        let socket = UdpSocket::bind(local)?;
        socket.connect(peer)?;
        Ok(UdpSocketHandle {
            socket,
            capabilities: self.capabilities.clone(),
            policy: Arc::clone(&self.policy),
        })
    }

    /// Binds a UDP socket to an allowlisted local endpoint.
    pub fn udp_listen(&self, address: SocketAddr) -> Result<UdpSocketHandle, NetworkError> {
        self.require(Capability::NetListen)?;
        self.check_listen(address)?;
        Ok(UdpSocketHandle {
            socket: UdpSocket::bind(address)?,
            capabilities: self.capabilities.clone(),
            policy: Arc::clone(&self.policy),
        })
    }

    /// Sends one bounded HTTP request to an allowlisted, pinned host.
    ///
    /// Redirects and ambient proxies are disabled. Response status codes are
    /// returned as ordinary responses; callers decide how to interpret them.
    pub fn http_request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, Vec<u8>)],
        body: &[u8],
        max_request_body_bytes: usize,
        max_response_body_bytes: usize,
    ) -> Result<HttpResponse, NetworkError> {
        self.require(Capability::NetConnect)?;
        validate_http_headers(headers)?;
        if body.len() > max_request_body_bytes {
            return Err(NetworkError::BodyTooLarge {
                limit: max_request_body_bytes,
            });
        }
        let uri: Uri = url
            .parse()
            .map_err(|_| NetworkError::InvalidEndpoint(url.to_owned()))?;
        let (host, port) = http_host_port(&uri)?;
        self.check_http_host(&host, port)?;

        let method = Method::from_bytes(method.as_bytes())
            .map_err(|_| NetworkError::InvalidHttpValue(method.to_owned()))?;
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| NetworkError::InvalidHttpValue(name.clone()))?;
            let value = HeaderValue::from_bytes(value)
                .map_err(|_| NetworkError::InvalidHttpValue("header value".to_owned()))?;
            builder = builder.header(name, value);
        }
        let request = builder
            .body(body.to_vec())
            .map_err(|error| NetworkError::InvalidHttpValue(error.to_string()))?;
        let mut response = self.http_agent.run(request).map_err(NetworkError::Http)?;
        let status = response.status().as_u16();
        if response.headers().iter().count() > MAX_HTTP_HEADER_COUNT {
            return Err(NetworkError::TooManyHeaders {
                limit: MAX_HTTP_HEADER_COUNT,
            });
        }
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| (name.as_str().to_owned(), value.as_bytes().to_vec()))
            .collect();
        let limit = u64::try_from(max_response_body_bytes).unwrap_or(u64::MAX);
        let body = response
            .body_mut()
            .with_config()
            .limit(limit)
            .read_to_vec()
            .map_err(|error| match error {
                UreqError::BodyExceedsLimit(_) => NetworkError::BodyTooLarge {
                    limit: max_response_body_bytes,
                },
                error => NetworkError::Http(error),
            })?;
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }

    /// Starts a plain HTTP server on an allowlisted address.
    ///
    /// TLS server support belongs to the separate TLS service and is not
    /// implied by `net.listen`.
    pub fn http_listen(&self, address: SocketAddr) -> Result<HttpServer, NetworkError> {
        self.require(Capability::NetListen)?;
        self.check_listen(address)?;
        let listener = TcpListener::bind(address)?;
        let local_address = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        Ok(HttpServer {
            listener,
            local_address,
        })
    }

    fn require(&self, capability: Capability) -> Result<(), NetworkError> {
        if self.capabilities.contains(&capability) {
            Ok(())
        } else {
            Err(NetworkError::PermissionDenied(capability))
        }
    }

    fn check_connect(&self, address: SocketAddr) -> Result<(), NetworkError> {
        if self
            .policy
            .read()
            .map_err(|_| NetworkError::PolicyPoisoned)?
            .connect_addresses
            .contains(&address)
        {
            Ok(())
        } else {
            Err(NetworkError::EndpointNotAllowed(address))
        }
    }

    fn check_listen(&self, address: SocketAddr) -> Result<(), NetworkError> {
        if self
            .policy
            .read()
            .map_err(|_| NetworkError::PolicyPoisoned)?
            .listen_addresses
            .contains(&address)
        {
            Ok(())
        } else {
            Err(NetworkError::EndpointNotAllowed(address))
        }
    }

    fn check_http_host(&self, host: &str, port: u16) -> Result<(), NetworkError> {
        let policy = self
            .policy
            .read()
            .map_err(|_| NetworkError::PolicyPoisoned)?;
        if let Some(addresses) = policy.http_hosts.get(&(normalize_host(host), port)) {
            if addresses.is_empty() {
                return Err(NetworkError::InvalidEndpoint(format!("{host}:{port}")));
            }
            return Ok(());
        }
        if let Ok(address) = host.parse::<IpAddr>() {
            let endpoint = SocketAddr::new(address, port);
            if policy.connect_addresses.contains(&endpoint) {
                return Ok(());
            }
            return Err(NetworkError::EndpointNotAllowed(endpoint));
        }
        Err(NetworkError::InvalidEndpoint(format!(
            "{host}:{port} is not in the HTTP host allowlist"
        )))
    }
}

/// A UDP socket that retains the host's outbound endpoint policy.
pub struct UdpSocketHandle {
    socket: UdpSocket,
    capabilities: BTreeSet<Capability>,
    policy: Arc<RwLock<NetworkPolicy>>,
}

impl UdpSocketHandle {
    /// Sends a datagram to an exact endpoint allowed by `net.connect`.
    pub fn send_to(&self, bytes: &[u8], address: SocketAddr) -> Result<usize, NetworkError> {
        require_capability(&self.capabilities, Capability::NetConnect)?;
        check_policy_address(&self.policy, address, true)?;
        Ok(self.socket.send_to(bytes, address)?)
    }

    /// Sends to the peer selected by [`Self::connect`].
    pub fn send(&self, bytes: &[u8]) -> Result<usize, NetworkError> {
        require_capability(&self.capabilities, Capability::NetConnect)?;
        let peer = self.socket.peer_addr()?;
        check_policy_address(&self.policy, peer, true)?;
        Ok(self.socket.send(bytes)?)
    }

    /// Connects this socket to an exact endpoint allowed by `net.connect`.
    pub fn connect(&self, address: SocketAddr) -> Result<(), NetworkError> {
        require_capability(&self.capabilities, Capability::NetConnect)?;
        check_policy_address(&self.policy, address, true)?;
        self.socket.connect(address)?;
        Ok(())
    }

    /// Receives one datagram and its source endpoint.
    pub fn recv_from(&self, bytes: &mut [u8]) -> Result<(usize, SocketAddr), NetworkError> {
        Ok(self.socket.recv_from(bytes)?)
    }

    /// Receives one datagram from a connected peer.
    pub fn recv(&self, bytes: &mut [u8]) -> Result<usize, NetworkError> {
        Ok(self.socket.recv(bytes)?)
    }

    /// Returns the socket's local endpoint.
    pub fn local_addr(&self) -> Result<SocketAddr, NetworkError> {
        Ok(self.socket.local_addr()?)
    }

    /// Sets a read timeout.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<(), NetworkError> {
        Ok(self.socket.set_read_timeout(timeout)?)
    }

    /// Sets a write timeout.
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> Result<(), NetworkError> {
        Ok(self.socket.set_write_timeout(timeout)?)
    }
}

/// A bounded HTTP response returned by [`NetworkServices::http_request`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpResponse {
    /// Numeric HTTP status code.
    pub status: u16,
    /// Raw header names and values.
    pub headers: Vec<(String, Vec<u8>)>,
    /// Response body, bounded by the caller's limit.
    pub body: Vec<u8>,
}

/// A plain HTTP listener bound through a `net.listen` endpoint grant.
pub struct HttpServer {
    listener: TcpListener,
    local_address: SocketAddr,
}

impl HttpServer {
    /// Returns the actual local endpoint.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_address
    }

    /// Waits up to `timeout` for a client connection (`None` waits indefinitely)
    /// and then reads one request. Accepted connections have a 30-second total
    /// request-read deadline and a 30-second write timeout. Headers are limited
    /// to 64 KiB and 128 fields; the body is limited by
    /// `max_request_body_bytes`. Each response closes its connection.
    pub fn receive_request(
        &self,
        timeout: Option<Duration>,
        max_request_body_bytes: usize,
    ) -> Result<Option<HttpRequest>, NetworkError> {
        let deadline = timeout.and_then(|duration| Instant::now().checked_add(duration));
        let (stream, remote_addr) = loop {
            match self.listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if deadline.is_some_and(|until| Instant::now() >= until) {
                        return Ok(None);
                    }
                    thread::sleep(HTTP_ACCEPT_POLL_INTERVAL);
                }
                Err(error) => return Err(NetworkError::Io(error)),
            }
        };
        stream.set_read_timeout(Some(HTTP_SOCKET_TIMEOUT))?;
        stream.set_write_timeout(Some(HTTP_SOCKET_TIMEOUT))?;
        let request_deadline = Instant::now() + HTTP_SOCKET_TIMEOUT;
        match read_http_request(
            stream,
            remote_addr,
            max_request_body_bytes,
            request_deadline,
        ) {
            Ok(request) => Ok(Some(request)),
            Err(error) => Err(error),
        }
    }
}

/// One bounded HTTP request received by [`HttpServer`].
pub struct HttpRequest {
    stream: TcpStream,
    /// Request method.
    pub method: String,
    /// Request path and query.
    pub url: String,
    /// Client address.
    pub remote_addr: SocketAddr,
    /// Raw request header names and values.
    pub headers: Vec<(String, Vec<u8>)>,
    /// Request body.
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// Sends an HTTP response, bounded by the supplied body limit, and closes
    /// this request's connection.
    pub fn respond(
        self,
        status: u16,
        headers: &[(String, Vec<u8>)],
        body: Vec<u8>,
        max_response_body_bytes: usize,
    ) -> Result<(), NetworkError> {
        if !(200..=599).contains(&status) {
            return Err(NetworkError::InvalidHttpValue(format!(
                "final status code {status} is outside 200..=599"
            )));
        }
        if body.len() > max_response_body_bytes {
            return Err(NetworkError::BodyTooLarge {
                limit: max_response_body_bytes,
            });
        }
        validate_http_headers(headers)?;
        for (name, _) in headers {
            let parsed_name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| NetworkError::InvalidHttpValue(name.clone()))?;
            if matches!(
                parsed_name.as_str(),
                "content-length" | "transfer-encoding" | "connection"
            ) {
                return Err(NetworkError::InvalidHttpValue(format!(
                    "{} is managed by the HTTP server",
                    parsed_name.as_str()
                )));
            }
        }
        let status = StatusCode::from_u16(status)
            .map_err(|_| NetworkError::InvalidHttpValue("invalid status code".to_owned()))?;
        let mut stream = self.stream;
        let reason = status.canonical_reason().unwrap_or("");
        write!(stream, "HTTP/1.1 {} {}\r\n", status.as_u16(), reason)?;
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| NetworkError::InvalidHttpValue(name.clone()))?;
            let value = HeaderValue::from_bytes(value)
                .map_err(|_| NetworkError::InvalidHttpValue(name.to_string()))?;
            stream.write_all(name.as_str().as_bytes())?;
            stream.write_all(b": ")?;
            stream.write_all(value.as_bytes())?;
            stream.write_all(b"\r\n")?;
        }
        let status_code = status.as_u16();
        let suppress_body =
            self.method.eq_ignore_ascii_case("HEAD") || matches!(status_code, 204 | 205 | 304);
        if !matches!(status_code, 204 | 304) {
            let declared_length = if status_code == 205 { 0 } else { body.len() };
            write!(stream, "Content-Length: {declared_length}\r\n")?;
        }
        stream.write_all(b"Connection: close\r\n\r\n")?;
        if !suppress_body {
            stream.write_all(&body)?;
        }
        stream.flush()?;
        Ok(())
    }
}

fn read_http_request(
    stream: TcpStream,
    remote_addr: SocketAddr,
    max_request_body_bytes: usize,
    deadline: Instant,
) -> Result<HttpRequest, NetworkError> {
    let mut reader = BufReader::new(RequestDeadlineStream { stream, deadline });
    let raw_headers = match read_http_header_block(&mut reader) {
        Ok(headers) => headers,
        Err(error @ NetworkError::HeadersTooLarge { .. }) => {
            let _ = write_simple_http_response(reader.get_mut(), 431);
            return Err(error);
        }
        Err(error) => {
            let _ = write_simple_http_response(reader.get_mut(), request_error_status(&error));
            return Err(error);
        }
    };
    let mut header_slots = [httparse::EMPTY_HEADER; MAX_HTTP_HEADER_COUNT];
    let mut parsed = httparse::Request::new(&mut header_slots);
    match parsed.parse(&raw_headers) {
        Ok(httparse::Status::Complete(consumed)) if consumed == raw_headers.len() => {}
        Err(httparse::Error::TooManyHeaders) => {
            let _ = write_simple_http_response(reader.get_mut(), 431);
            return Err(NetworkError::TooManyHeaders {
                limit: MAX_HTTP_HEADER_COUNT,
            });
        }
        _ => {
            let _ = write_simple_http_response(reader.get_mut(), 400);
            return Err(NetworkError::HttpProtocolError(
                "malformed HTTP request headers".to_owned(),
            ));
        }
    }
    let method = parsed.method.unwrap_or_default().to_owned();
    let url = parsed.path.unwrap_or_default().to_owned();
    let version = parsed.version.unwrap_or_default();
    if version > 1 || method.is_empty() || url.is_empty() {
        let _ = write_simple_http_response(reader.get_mut(), 400);
        return Err(NetworkError::HttpProtocolError(
            "only HTTP/1.0 and HTTP/1.1 request lines are supported".to_owned(),
        ));
    }
    let headers: Vec<(String, Vec<u8>)> = parsed
        .headers
        .iter()
        .map(|header| (header.name.to_owned(), header.value.to_vec()))
        .collect();
    drop(parsed);
    if version == 1
        && headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("host"))
            .count()
            != 1
    {
        let _ = write_simple_http_response(reader.get_mut(), 400);
        return Err(NetworkError::HttpProtocolError(
            "HTTP/1.1 requires exactly one Host header".to_owned(),
        ));
    }

    let mut content_length = None;
    let mut transfer_encoding = None;
    for (name, value) in &headers {
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                let _ = write_simple_http_response(reader.get_mut(), 400);
                return Err(NetworkError::HttpProtocolError(
                    "duplicate Content-Length headers are not allowed".to_owned(),
                ));
            }
            let parsed_length = std::str::from_utf8(value).ok().and_then(|text| {
                text.trim_matches(|character| character == ' ' || character == '\t')
                    .parse::<usize>()
                    .ok()
            });
            let Some(parsed_length) = parsed_length else {
                let _ = write_simple_http_response(reader.get_mut(), 400);
                return Err(NetworkError::HttpProtocolError(
                    "invalid Content-Length header".to_owned(),
                ));
            };
            content_length = Some(parsed_length);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            if transfer_encoding.is_some() {
                let _ = write_simple_http_response(reader.get_mut(), 400);
                return Err(NetworkError::HttpProtocolError(
                    "duplicate Transfer-Encoding headers are not allowed".to_owned(),
                ));
            }
            transfer_encoding = Some(value.clone());
        }
    }
    if transfer_encoding.is_some() && content_length.is_some() {
        let _ = write_simple_http_response(reader.get_mut(), 400);
        return Err(NetworkError::HttpProtocolError(
            "Transfer-Encoding and Content-Length cannot be combined".to_owned(),
        ));
    }
    if content_length.is_some_and(|length| length > max_request_body_bytes) {
        let _ = write_simple_http_response(reader.get_mut(), 413);
        return Err(NetworkError::BodyTooLarge {
            limit: max_request_body_bytes,
        });
    }
    let mut expectation = None;
    for (name, value) in &headers {
        if name.eq_ignore_ascii_case("expect") {
            if expectation.is_some() {
                let _ = write_simple_http_response(reader.get_mut(), 417);
                return Err(NetworkError::HttpProtocolError(
                    "duplicate Expect headers are not allowed".to_owned(),
                ));
            }
            expectation = Some(value.as_slice());
        }
    }
    if let Some(expectation) = expectation {
        if version != 1 || !expectation.eq_ignore_ascii_case(b"100-continue") {
            let _ = write_simple_http_response(reader.get_mut(), 417);
            return Err(NetworkError::HttpProtocolError(
                "unsupported HTTP Expect header".to_owned(),
            ));
        }
        reader
            .get_mut()
            .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        reader.get_mut().flush()?;
    }
    let body = if let Some(encoding) = transfer_encoding {
        if !encoding.eq_ignore_ascii_case(b"chunked") {
            let _ = write_simple_http_response(reader.get_mut(), 501);
            return Err(NetworkError::HttpProtocolError(
                "only chunked Transfer-Encoding is supported".to_owned(),
            ));
        }
        match read_chunked_body(&mut reader, max_request_body_bytes) {
            Ok(body) => body,
            Err(error @ NetworkError::BodyTooLarge { .. }) => {
                let _ = write_simple_http_response(reader.get_mut(), 413);
                return Err(error);
            }
            Err(error) => {
                let _ = write_simple_http_response(reader.get_mut(), request_error_status(&error));
                return Err(error);
            }
        }
    } else if let Some(length) = content_length {
        if length > max_request_body_bytes {
            let _ = write_simple_http_response(reader.get_mut(), 413);
            return Err(NetworkError::BodyTooLarge {
                limit: max_request_body_bytes,
            });
        }
        let mut body = Vec::new();
        body.try_reserve_exact(length)
            .map_err(|error| NetworkError::Io(io::Error::other(error.to_string())))?;
        body.resize(length, 0);
        if let Err(error) = reader.read_exact(&mut body) {
            let network_error = NetworkError::Io(error);
            let _ =
                write_simple_http_response(reader.get_mut(), request_error_status(&network_error));
            return Err(network_error);
        }
        body
    } else {
        Vec::new()
    };
    Ok(HttpRequest {
        stream: reader.into_inner().stream,
        method,
        url,
        remote_addr,
        headers,
        body,
    })
}

fn read_http_header_block<R: BufRead>(reader: &mut R) -> Result<Vec<u8>, NetworkError> {
    let mut block = Vec::new();
    loop {
        let remaining = MAX_HTTP_HEADER_BYTES.saturating_sub(block.len());
        let line = read_crlf_line(reader, remaining)?;
        let is_end = line == b"\r\n";
        if block.len().saturating_add(line.len()) > MAX_HTTP_HEADER_BYTES {
            return Err(NetworkError::HeadersTooLarge {
                limit: MAX_HTTP_HEADER_BYTES,
            });
        }
        block.extend_from_slice(&line);
        if is_end {
            return Ok(block);
        }
    }
}

fn read_crlf_line<R: BufRead>(reader: &mut R, limit: usize) -> Result<Vec<u8>, NetworkError> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Err(NetworkError::HttpProtocolError(
                "unexpected end of HTTP message".to_owned(),
            ));
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(available.len(), |index| index + 1);
        if line.len().saturating_add(count) > limit {
            return Err(NetworkError::HeadersTooLarge { limit });
        }
        line.extend_from_slice(&available[..count]);
        reader.consume(count);
        if newline.is_some() {
            if !line.ends_with(b"\r\n") {
                return Err(NetworkError::HttpProtocolError(
                    "HTTP lines must end with CRLF".to_owned(),
                ));
            }
            return Ok(line);
        }
    }
}

fn read_chunked_body<R: BufRead>(
    reader: &mut R,
    max_body_bytes: usize,
) -> Result<Vec<u8>, NetworkError> {
    let mut body = Vec::new();
    loop {
        let line = read_crlf_line(reader, 1024)?;
        let line = std::str::from_utf8(&line[..line.len() - 2])
            .map_err(|_| NetworkError::HttpProtocolError("invalid chunk size".to_owned()))?;
        let size_text = line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| NetworkError::HttpProtocolError("invalid chunk size".to_owned()))?;
        if size == 0 {
            let mut trailer_bytes = 0usize;
            let mut trailer_count = 0usize;
            loop {
                let line =
                    read_crlf_line(reader, MAX_HTTP_HEADER_BYTES.saturating_sub(trailer_bytes))?;
                trailer_bytes = trailer_bytes.saturating_add(line.len());
                if line == b"\r\n" {
                    return Ok(body);
                }
                trailer_count += 1;
                if trailer_count > MAX_HTTP_HEADER_COUNT {
                    return Err(NetworkError::TooManyHeaders {
                        limit: MAX_HTTP_HEADER_COUNT,
                    });
                }
                if !line[..line.len() - 2].contains(&b':') {
                    return Err(NetworkError::HttpProtocolError(
                        "malformed HTTP trailer".to_owned(),
                    ));
                }
            }
        }
        if size > max_body_bytes.saturating_sub(body.len()) {
            return Err(NetworkError::BodyTooLarge {
                limit: max_body_bytes,
            });
        }
        body.try_reserve(size)
            .map_err(|error| NetworkError::Io(io::Error::other(error.to_string())))?;
        let old_length = body.len();
        body.resize(old_length + size, 0);
        reader.read_exact(&mut body[old_length..])?;
        let mut delimiter = [0; 2];
        reader.read_exact(&mut delimiter)?;
        if delimiter != *b"\r\n" {
            return Err(NetworkError::HttpProtocolError(
                "chunk data must end with CRLF".to_owned(),
            ));
        }
    }
}

fn write_simple_http_response<W: Write>(stream: &mut W, status: u16) -> io::Result<()> {
    let (reason, body): (&str, &[u8]) = match status {
        400 => ("Bad Request", b"Bad Request"),
        413 => ("Payload Too Large", b"Payload Too Large"),
        431 => (
            "Request Header Fields Too Large",
            b"Request Header Fields Too Large",
        ),
        408 => ("Request Timeout", b"Request Timeout"),
        417 => ("Expectation Failed", b"Expectation Failed"),
        501 => ("Not Implemented", b"Not Implemented"),
        _ => ("Bad Request", b"Bad Request"),
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

fn request_error_status(error: &NetworkError) -> u16 {
    match error {
        NetworkError::BodyTooLarge { .. } => 413,
        NetworkError::HeadersTooLarge { .. } | NetworkError::TooManyHeaders { .. } => 431,
        NetworkError::Io(error)
            if matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) =>
        {
            408
        }
        _ => 400,
    }
}

struct RequestDeadlineStream {
    stream: TcpStream,
    deadline: Instant,
}

impl Read for RequestDeadlineStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "HTTP request deadline exceeded",
            ));
        }
        self.stream.set_read_timeout(Some(remaining))?;
        self.stream.read(buffer)
    }
}

impl Write for RequestDeadlineStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.stream.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

fn validate_http_headers(headers: &[(String, Vec<u8>)]) -> Result<(), NetworkError> {
    if headers.len() > MAX_HTTP_HEADER_COUNT {
        return Err(NetworkError::TooManyHeaders {
            limit: MAX_HTTP_HEADER_COUNT,
        });
    }
    let mut bytes = 0usize;
    for (name, value) in headers {
        HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| NetworkError::InvalidHttpValue(name.clone()))?;
        HeaderValue::from_bytes(value).map_err(|_| NetworkError::InvalidHttpValue(name.clone()))?;
        bytes = bytes
            .checked_add(name.len())
            .and_then(|size| size.checked_add(value.len()))
            .and_then(|size| size.checked_add(4))
            .ok_or(NetworkError::HeadersTooLarge {
                limit: MAX_HTTP_HEADER_BYTES,
            })?;
    }
    if bytes > MAX_HTTP_HEADER_BYTES {
        return Err(NetworkError::HeadersTooLarge {
            limit: MAX_HTTP_HEADER_BYTES,
        });
    }
    Ok(())
}
struct PinnedResolver {
    policy: Arc<RwLock<NetworkPolicy>>,
}

impl fmt::Debug for PinnedResolver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PinnedResolver")
    }
}

impl Resolver for PinnedResolver {
    fn resolve(
        &self,
        uri: &Uri,
        _config: &ureq::config::Config,
        _timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, UreqError> {
        let (host, port) = http_host_port(uri).map_err(|_| UreqError::HostNotFound)?;
        let policy = self.policy.read().map_err(|_| UreqError::HostNotFound)?;
        let Some(addresses) = policy.http_hosts.get(&(normalize_host(&host), port)) else {
            if let Ok(address) = host.parse::<IpAddr>() {
                let endpoint = SocketAddr::new(address, port);
                if policy.connect_addresses.contains(&endpoint) {
                    let mut result = self.empty();
                    let _ = result.try_push(endpoint);
                    return Ok(result);
                }
            }
            return Err(UreqError::HostNotFound);
        };
        let mut result = self.empty();
        for address in addresses.iter().take(MAX_RESOLVED_ADDRESSES) {
            if policy.connect_addresses.contains(address) {
                let _ = result.try_push(*address);
            }
        }
        if result.is_empty() {
            Err(UreqError::HostNotFound)
        } else {
            Ok(result)
        }
    }
}

fn require_capability(
    capabilities: &BTreeSet<Capability>,
    capability: Capability,
) -> Result<(), NetworkError> {
    if capabilities.contains(&capability) {
        Ok(())
    } else {
        Err(NetworkError::PermissionDenied(capability))
    }
}

fn check_policy_address(
    policy: &Arc<RwLock<NetworkPolicy>>,
    address: SocketAddr,
    connect: bool,
) -> Result<(), NetworkError> {
    let policy = policy.read().map_err(|_| NetworkError::PolicyPoisoned)?;
    let allowed = if connect {
        policy.connect_addresses.contains(&address)
    } else {
        policy.listen_addresses.contains(&address)
    };
    if allowed {
        Ok(())
    } else {
        Err(NetworkError::EndpointNotAllowed(address))
    }
}

fn resolve_host(host: &str, port: u16) -> Result<Vec<SocketAddr>, NetworkError> {
    if let Ok(address) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(address, port)]);
    }
    let endpoint = format!("{host}:{port}");
    let mut addresses = endpoint
        .to_socket_addrs()
        .map_err(NetworkError::HostResolution)?
        .collect::<Vec<_>>();
    addresses.sort();
    addresses.dedup();
    addresses.truncate(MAX_RESOLVED_ADDRESSES);
    Ok(addresses)
}

fn http_host_port(uri: &Uri) -> Result<(String, u16), NetworkError> {
    let host = uri
        .host()
        .ok_or_else(|| NetworkError::InvalidEndpoint(uri.to_string()))?;
    let scheme = uri.scheme_str().unwrap_or_default();
    let port = uri.port_u16().or_else(|| {
        if scheme.eq_ignore_ascii_case("http") {
            Some(80)
        } else if scheme.eq_ignore_ascii_case("https") {
            Some(443)
        } else {
            None
        }
    });
    let scheme_is_supported =
        scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https");
    let Some(port) = port else {
        return Err(NetworkError::InvalidEndpoint(uri.to_string()));
    };
    if !scheme_is_supported {
        return Err(NetworkError::InvalidEndpoint(uri.to_string()));
    }
    Ok((host.to_owned(), port))
}

fn normalize_host(host: &str) -> String {
    host.trim_matches(['[', ']']).to_ascii_lowercase()
}

fn unspecified_address(ipv6: bool) -> SocketAddr {
    if ipv6 {
        SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)
    } else {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
    }
}
