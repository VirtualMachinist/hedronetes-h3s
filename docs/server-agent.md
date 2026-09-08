# Native server agent

`h3s server` runs a native local agent in the same process as the API,
controllers, scheduler, registry and supervisor. It uses the same node-scoped
enrollment, Node/Lease reconciliation, kubelet TLS listener, supervisor tunnel
and CRI Pod reconciliation as `h3s agent` on a separate worker.

```sh
h3s server --data-dir /var/lib/hedronetes \
  --write-kubeconfig /etc/hedronetes/h3s.yaml \
  --tls-san 192.168.104.1 \
  --node-name hedronetes-server --node-ip 192.168.104.1 \
  --container-runtime-endpoint unix:///run/hedronetes-m1/containerd/containerd.sock
```

The local agent enrolls concurrently with API serving. It consumes the public
CA export and enrollment token in memory, creates its own private node key,
and stores its identity under `<data-dir>/agent`. It does not use the admin
certificate for Node or Pod requests. The server's PKI, registry and join-token
paths remain under `<data-dir>/server`. Existing API-only installations can
enable the agent while retaining the same bind address, port, TLS SANs, data
directory and kubeconfig path. Do not run a second agent against that same
agent directory or node name.

`--disable-agent` retains the API-only path and skips local hostname/IP
discovery, enrollment, agent state and kubelet listener creation. A default
server without a CRI endpoint registers **NotReady**, with reason
`RuntimeNotReady`; enrollment is not proof that it can execute Pods. An explicit
endpoint must refer to a separately configured real local CRI runtime. Native
runtime/Flannel supervision and declarative NixOS activation remain pending.

For a local agent, `--node-name` defaults to the system hostname converted to
lowercase and validated as a DNS subdomain. `--node-ip` takes precedence over
the API bind address. Otherwise a concrete non-loopback bind IP is used; for
wildcard/loopback bindings, a UDP socket asks the kernel for its IPv4 route's
source address without sending a datagram. Use explicit names and reachable
interface addresses on multi-interface, offline or IPv6-only hosts. Discovery
does not verify external reachability. The API's own wildcard connection uses
the corresponding IPv4 or IPv6 loopback address.

`--kubelet-port` defaults to 10250 and binds loopback only. A fatal local agent
configuration, enrollment or listener failure exits the composed server, so
the service manager can report and recover it. Initial Node registration
retries revision conflicts, throttling, server errors and transport failures
for at most 30 seconds. Every retry reads current state. Configuration and
authorization failures remain immediate; normal heartbeat retries continue
after startup. Preserve logs and repair the underlying cause if startup fails.

`cargo test -p h3s --test server_agent` launches actual binaries, verifies both
node identities and separate CIDRs, authenticated tunneled health, truthful
NotReady, API/local-agent crash recovery, remote tunnel reconnect, durable
credentials and allocation, and local Node recreation with a new Lease owner.
It also checks API-only isolation and process failure on a busy kubelet port.
Those process tests use no CRI and do not claim real two-node network or distro
acceptance; the NixOS installed fixture and Debian/Fedora suites remain required.
