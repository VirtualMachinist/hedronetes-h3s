# Supervisor WebSocket transport

The native worker maintains a rustls WebSocket connection to
`wss://<server>:6443/v1-h3s/connect`, negotiating `h3s.tunnel.v1`. Its certificate
comes from native enrollment. No join token, administrator kubeconfig, browser
Origin or user-selected node-name header is used for this connection.

The transport multiplexes server-initiated byte streams for the worker's local
kubelet service. The destination is the agent's own loopback TLS listener at
`127.0.0.1:10250` by default (`h3s agent --kubelet-port` changes the port).
A request cannot select a different host, port, Unix socket or CRI endpoint.
A one-byte service discriminator and success/refusal acknowledgement precede
each stream. The transport preserves byte content and TCP-style half-close.
The API establishes a second, mutually authenticated TLS connection through
that byte stream before sending a kubelet request.

**Current boundary:** the actual worker API serves process health and runtime
readiness through authorized node proxy requests. `/healthz` returns 200;
`/readyz` returns 503 while CRI/Pod execution is unfinished. Pod logs, exec,
metrics, arbitrary proxy paths, and workload execution remain unimplemented.
The worker reports RuntimeNotReady. An echo fixture in a transport test does
not qualify as a functioning kubelet or workload.

## Private kubelet API

An authenticated caller needs `get` on core `nodes/proxy`, optionally restricted
with `resourceNames`. Built-in node permissions do not grant this operation.
The implemented routes are exactly:

```sh
kubectl get --raw /api/v1/nodes/hedronetes-worker/proxy/healthz
kubectl get --raw /api/v1/nodes/hedronetes-worker/proxy/readyz
```

Only GET is supported. The API accepts the single `timeout` client hint added
by stock kubectl, while retaining its own 15-second cap; other or duplicate
query parameters are rejected. No query is sent to the worker. It constructs a fresh
fixed-path request and never forwards caller headers, credentials, cookies or
Host. An absent Node returns 404, an unavailable or untrusted backend returns
502, and the complete proxied operation has a 15-second budget (504 on timeout).
Backend responses are bounded to 4 KiB; only 200 and 503 are accepted.

The worker generates a separate serving key locally and requests its public
certificate at `/v1-h3s/serving` using its enrolled node client certificate.
The signer requires both the Node and native enrollment record and derives the
identity from the certificate, not request fields. It verifies the CSR signature
and discards all requested subject, SAN, CA and usage privileges. The new leaf
has ServerAuth only, with no node client organization. The request is limited to
16 KiB/ten seconds, the CSR to 8 KiB, and signing shares four admission slots with
enrollment. No private key leaves the worker.

Each node's serving certificate uses a private TLS name:
`node-<first 32 SHA-256 hex characters>.<last 32>.h3s.invalid`, hashing the exact
node name. The API verifies this node-specific name and the cluster CA using
normal rustls certificate verification. No DNS lookup is needed over the tunnel.
This dedicated namespace prevents enrolling as `kubernetes` or another ordinary
control-plane hostname from obtaining that hostname's serving certificate.
These certificates are for the private h3s kubelet route, not direct IP-address
access by stock external kubelet clients.

The loopback listener requires a trusted client certificate and grants requests
only to the API's scoped `system:h3s:kubelet-client` identity. Direct node and
administrator certificates are denied; callers use API RBAC instead. Handshakes
and HTTP headers each have five-second limits, with at most sixteen local
connections and no keep-alive. The service logs endpoint/status metadata only.
There is no additional external worker port to open.

`<data-dir>/agent/serving.json` is an atomic 0600 file in the existing private
agent directory. It persists the separate key/CSR before signing, then validates
and saves the returned certificate. Server/worker process restarts preserve it.
A different node/CA, corrupt state, expired leaf or mismatched key fails without
rewriting the saved identity. Serving certificates expire after one year;
automatic renewal and hot reload remain unimplemented.

## Authentication and lifecycle

The API requires a verified certificate identifying `system:node:<name>` in
`system:nodes`, a persisted native enrollment record, and a registered Node.
Ordinary clients, administrators and valid but unenrolled node certificates
cannot attach. Query parameters, Origin, unsupported/missing protocol headers,
impersonation and bearer headers are rejected. Only one connection/reservation
per node is permitted: duplicates get 409 until the earlier connection releases.

TLS connection permits remain held across HTTP upgrade. API shutdown cancels
upgraded sockets as well as ordinary HTTP connections. Failed upgrades,
disconnects, malformed transport traffic and cancellation release the node
reservation and its streams. The worker keeps its node identity, retries failed
connections with 1–30 second exponential backoff, and resets the delay after a
connection lasts at least a minute. Node/Lease heartbeats and tunnel I/O are
polled independently so either path can recover without starving the other.

Both peers send WebSocket pings every ten seconds. Only a matching pong refreshes
the liveness deadline; a stale or silent peer is disconnected after the
30-second limit is observed by the next tick. Individual blocked transport writes
are limited to ten seconds. This permits recovery from a dead connection without
silently replacing an active node session.

## Resource and protocol limits

Yamux 0.14.0 provides stream multiplexing and flow control. Each connection allows
16 wire streams and at most 4 MiB total receive window (256 KiB initial credit
per stream). An admission semaphore permits only eight simultaneous caller-held
streams, leaving headroom for closing streams. Excess opens return WouldBlock
without opening another wire stream. Cancelled requests are dropped before
opening a stream. The supervisor holds at most 64 node reservations, within the transport's
256 TLS connection limit. Pending server opens use a bounded queue. Opening a
stream, its fixed-service handshake and acknowledgement have a five-second
budget; the worker's initial request and local TCP connect each have three seconds.

Only binary application WebSocket messages are accepted, with a 64 KiB maximum
message/frame. The byte bridge buffers 64 KiB per direction and emits 16 KiB
chunks; the WebSocket write-buffer limit is 128 KiB. Text and oversized messages
close the connection. Worker-initiated Yamux streams toward the server are a
protocol error. Transport errors contain no credential or stream payload data.
There is no compression, redirect-following or ambient proxy configuration.
The client uses its explicit pinned-CA rustls configuration.

## Verification and remaining work

`cargo test -p h3s-apiserver --test supervisor --locked` uses actual TCP, rustls,
WebSocket upgrades, the persisted API, native enrollment and real agent logic.
It verifies authentication/negotiation/duplicate denials; four concurrent streams
each larger than their receive window; full duplex and half-close; no cross-talk;
unknown-node and unavailable-target failures; admission capacity, stream churn
and continued existing traffic; malformed-frame cleanup; and API
restart with automatic agent reconnect and preserved Node identity. The local
byte endpoint is explicitly an echo fixture, not a kubelet substitute.

`cargo test -p h3s-supervisor --locked` verifies the stalled-peer deadline with
controlled time and stale pong rejection. Separate installed two-VM evidence
must verify actual health/readiness traffic and recovery. The workload suite
must still prove the required Pod traffic and runtime behavior; health alone
does not establish a working workload cluster.

`cargo test -p h3s-apiserver --test kubelet --locked` runs the actual local TLS
service through the actual supervisor and checks scoped RBAC, mandatory client
identity, untrusted node names inside authenticated tunnels, CSR privilege
stripping, distinct private serving keys, token-free restart, and preserved
state after API restart. A separate TLS fixture verifies that a trusted
certificate for another node cannot receive the proxied request.

Primary implementation references: [Yamux connection API](https://docs.rs/yamux/0.14.0/yamux/struct.Connection.html),
[Yamux window configuration](https://docs.rs/yamux/0.14.0/yamux/struct.Config.html),
[Axum WebSocket upgrade](https://docs.rs/axum/0.8.9/axum/extract/struct.WebSocketUpgrade.html),
and [explicit rustls WebSocket connector](https://docs.rs/tokio-tungstenite/0.29.0/tokio_tungstenite/fn.connect_async_tls_with_config.html).
