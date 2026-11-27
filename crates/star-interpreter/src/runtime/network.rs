//! Capability-gated TCP, UDP, and HTTP host APIs.
//!
//! Network access is deny by default. A trusted host must grant `net.connect`
//! or `net.listen` and add exact socket endpoints or DNS names to the relevant
//! allowlist before guest code can use the service. HTTP client DNS results are
//! pinned when a host grants a name, preventing later DNS rebinding. Proxies
//! and redirects are disabled so requests cannot escape the checked endpoint.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{self, Read};
use std::net::{
    IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket,
};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use http::{HeaderName, HeaderValue, Method, Request, Uri};
use tiny_http::{Header, Response as ServerResponse, Server, StatusCode};
use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};
use ureq::{Agent, Error as UreqError};

use super::host::Capability;

const MAX_RESOLVED_ADDRESSES: usize = 16;

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
        max_request_bytes: usize,
        max_response_bytes: usize,
    ) -> Result<HttpResponse, NetworkError> {
        self.require(Capability::NetConnect)?;
        if body.len() > max_request_bytes {
            return Err(NetworkError::BodyTooLarge {
                limit: max_request_bytes,
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
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| (name.as_str().to_owned(), value.as_bytes().to_vec()))
            .collect();
        let limit = u64::try_from(max_response_bytes).unwrap_or(u64::MAX);
        let body = response
            .body_mut()
            .with_config()
            .limit(limit)
            .read_to_vec()
            .map_err(|error| match error {
                UreqError::BodyExceedsLimit(_) => NetworkError::BodyTooLarge {
                    limit: max_response_bytes,
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
        let server = Server::from_listener(listener, None)
            .map_err(|error| NetworkError::Io(io::Error::other(error.to_string())))?;
        Ok(HttpServer {
            server,
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
    server: Server,
    local_address: SocketAddr,
}

impl HttpServer {
    /// Returns the actual local endpoint.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_address
    }

    /// Waits up to `timeout` for one request (`None` blocks indefinitely) and
    /// reads no more than `max_request_bytes` from its body.
    pub fn receive_request(
        &self,
        timeout: Option<Duration>,
        max_request_bytes: usize,
    ) -> Result<Option<HttpRequest>, NetworkError> {
        let request = match timeout {
            Some(timeout) => self.server.recv_timeout(timeout)?,
            None => Some(self.server.recv()?),
        };
        let Some(mut request) = request else {
            return Ok(None);
        };
        if request
            .body_length()
            .is_some_and(|length| length > max_request_bytes)
        {
            let _ = request.respond(ServerResponse::empty(StatusCode(413)));
            return Err(NetworkError::BodyTooLarge {
                limit: max_request_bytes,
            });
        }
        let limit = u64::try_from(max_request_bytes).unwrap_or(u64::MAX);
        let mut body = Vec::new();
        request
            .as_reader()
            .take(limit.saturating_add(1))
            .read_to_end(&mut body)?;
        if body.len() > max_request_bytes {
            let _ = request.respond(ServerResponse::empty(StatusCode(413)));
            return Err(NetworkError::BodyTooLarge {
                limit: max_request_bytes,
            });
        }
        let method = request.method().to_string();
        let url = request.url().to_owned();
        let remote_addr = request.remote_addr().copied();
        let headers = request
            .headers()
            .iter()
            .map(|header| (header.field.to_string(), header.value.as_bytes().to_vec()))
            .collect();
        Ok(Some(HttpRequest {
            request: Some(request),
            method,
            url,
            remote_addr,
            headers,
            body,
        }))
    }
}

/// One bounded HTTP request received by [`HttpServer`].
pub struct HttpRequest {
    request: Option<tiny_http::Request>,
    /// Request method.
    pub method: String,
    /// Request path and query.
    pub url: String,
    /// Client address if the server socket provides it.
    pub remote_addr: Option<SocketAddr>,
    /// Raw request header names and values.
    pub headers: Vec<(String, Vec<u8>)>,
    /// Request body.
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// Sends an HTTP response and consumes this request.
    pub fn respond(
        mut self,
        status: u16,
        headers: &[(String, Vec<u8>)],
        body: Vec<u8>,
    ) -> Result<(), NetworkError> {
        if !(100..=599).contains(&status) {
            return Err(NetworkError::InvalidHttpValue(format!(
                "status code {status} is outside 100..=599"
            )));
        }
        let mut response = ServerResponse::from_data(body).with_status_code(StatusCode(status));
        for (name, value) in headers {
            let header = Header::from_bytes(name.as_bytes(), value.clone())
                .map_err(|_| NetworkError::InvalidHttpValue(name.clone()))?;
            response.add_header(header);
        }
        let request = self
            .request
            .take()
            .ok_or_else(|| NetworkError::InvalidHttpValue("request already answered".to_owned()))?;
        request.respond(response).map_err(NetworkError::Io)
    }
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
