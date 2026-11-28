# JSON and Network Host APIs

Status: stable

This document describes the Rust host APIs for JSON, TCP, UDP, and HTTP. Star
1.0 does not register these APIs as guest `std.json`, `std.net`, or `std.http`
modules. A Rust host may call the APIs directly, subject to the capability and
endpoint checks documented here; host APIs do not imply guest module support.

## JSON

`star_interpreter::runtime::json` exposes `JsonValue`, an alias for
`serde_json::Value` configured with arbitrary-precision number support. Parsing
preserves JSON number tokens without first converting them to a fixed-width
integer or binary floating-point value.

- `parse(input, max_bytes)` accepts UTF-8 JSON bytes only when the complete
  input length is at most `max_bytes`.
- `stringify(value, max_bytes)` emits compact UTF-8 JSON and stops before
  writing beyond the limit.
- `stringify_pretty(value, max_bytes)` emits indented UTF-8 JSON under the same
  output limit.
- Malformed JSON, invalid UTF-8, and over-limit input/output are explicit
  `JsonError` results.

## Capabilities and endpoint allowlists

Network access requires both a capability grant and an explicit endpoint
allowlist entry. `net.connect` authorizes outbound TCP/UDP and HTTP client
operations. `net.listen` authorizes TCP/UDP binds and plain HTTP server binds.
Neither capability grants access to every address or port.

`HostServices::network()` returns a `NetworkServices` view that uses the same
immutable capability grants as the interpreter:

- `allow_connect_address(SocketAddr)` adds one exact outbound IP/port.
- `allow_connect_host(host, port)` resolves a host during trusted host setup,
  pins up to 16 resulting IP/port pairs, and records the host/port mapping for
  HTTP. DNS is not consulted again for those requests.
- `resolve_allowed_host(host, port)` returns the saved resolution for an
  allowed name without making a new DNS query.
- `allow_listen_address(SocketAddr)` adds one exact inbound IP/port. Port zero
  may be configured when the host wants the operating system to select an
  ephemeral port; the resulting listener exposes its actual local address.

All allowlist methods themselves require the corresponding capability. The
allowlist is additive for the lifetime of a `NetworkServices` instance. Rebuild
the host services to change or refresh its network policy.

## TCP and UDP

- `tcp_connect(address, timeout)` checks `net.connect` and the exact outbound
  allowlist before connecting.
- `tcp_listen(address)` checks `net.listen` and the exact inbound allowlist
  before binding.
- `udp_connect(peer)` checks the exact outbound endpoint before creating and
  connecting a socket.
- `udp_listen(address)` checks the exact inbound endpoint before binding.
- `UdpSocketHandle::send_to` and `connect` recheck `net.connect` and the exact
  destination for every outbound datagram. Receive methods do not authorize a
  new outbound destination.

TCP sockets are returned as standard-library streams/listeners after the
endpoint check. The host remains responsible for setting timeouts and managing
accepted connections.

## HTTP client

`NetworkServices::http_request` supports `http` and `https` URLs whose host and
effective port were explicitly allowed. An IP-literal URL is allowed when its
exact IP/port was added with `allow_connect_address`. Hostname URLs require a
matching `allow_connect_host` entry. The client uses the pinned DNS addresses,
the `net.connect` capability, and Rustls for HTTPS.

The client has a 30-second global I/O timeout. Environment proxies are disabled
and redirects are not followed, so proxy configuration and redirect targets
cannot bypass endpoint checks. HTTP status codes, including 4xx and 5xx, are
returned as ordinary responses. Callers provide independent request and
response byte limits; an oversized body returns `NetworkError::BodyTooLarge`.
Request and response headers are bounded to 64 KiB and 128 fields. Header names
and values are validated as HTTP syntax, with values represented as raw bytes.

## HTTP server

`NetworkServices::http_listen` starts a plain HTTP server on an allowlisted
`net.listen` endpoint. `HttpServer::receive_request` can block or wait up to a
caller-specified timeout for a client connection; accepted connections have a
30-second total request-read deadline and a 30-second write timeout. The parser
supports HTTP/1.0 and HTTP/1.1, bounds headers to 64 KiB and 128 fields before
collecting them, and accepts `Content-Length` or chunked request bodies. It
handles `Expect: 100-continue` and rejects other expectations with 417. It
reads at most the caller's body limit and returns 413 when that limit is
exceeded. It returns the method, path and query, raw headers, body, and peer
address. `HttpRequest::respond` validates the final status and
response headers, applies a caller-supplied response-body limit, sends a
`Content-Length`, and closes the connection after one request.

HTTPS server/TLS configuration is separate and is not implied by `net.listen`.
The host must also apply OS-level isolation when guest code needs stronger
network confinement than this capability and endpoint policy provides.

## Scope

These are host-side Rust APIs. Guest-visible standard-library modules and
structured guest values are outside the Star 1.0 surface. The API does not
provide DNS refresh, arbitrary
proxy configuration, redirect following, TLS server setup, persistent HTTP
connections, or unbounded JSON or HTTP body reads.
