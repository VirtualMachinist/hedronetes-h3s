# Native worker bootstrap

`h3s agent` enrolls with the h3s API, obtains a node-scoped client certificate,
registers its own Node, and renews its own Lease every ten seconds. The worker
creates and retains its private key locally. The server signs a verified CSR;
neither the CA key nor an administrator credential is sent to the worker.

This implements enrollment and identity persistence. The supervisor tunnel,
CRI/Pod execution, CNI, service networking and DNS remain unfinished. The agent
reports `Ready=False`, reason `RuntimeNotReady`; enrollment does not demonstrate
running workloads. The server still requires `--disable-agent`.

## Operator configuration

Start the server with a stable private data directory and the actual reachable
server address in its TLS SANs. Example for the isolated Tower pair:

```sh
h3s server --disable-agent --data-dir /var/lib/hedronetes \
  --write-kubeconfig /etc/hedronetes/h3s.yaml --tls-san 192.168.104.1
```

On first startup, the server creates `/var/lib/hedronetes/server/node-token`
(256 random bits encoded as 64 hexadecimal characters) and exports the public
CA to `/var/lib/hedronetes/server/ca.crt`. These files are mode 0600. Later
starts preserve them and reject a CA export that disagrees with the private PKI.
An explicit `--token-file` or `H3S_TOKEN` supplies a different enrollment token;
use the same configuration on subsequent server starts. Token files must be
private regular files, at most 1 KiB; token values must be 32–256 printable ASCII
bytes. Tokens are validated without echoing their values. CLI `--token` is also
supported, but a private file avoids putting a secret in process arguments.

Copy **only the public CA and enrollment token** through an authenticated
operator channel to a private worker configuration directory. Do not fetch and
trust an unauthenticated CA or disable TLS verification. Do not place secrets in
Nix derivations, the Nix store, Git, evidence archives, or logs.

```sh
h3s agent --server https://192.168.104.1:6443 \
  --server-ca-file /etc/hedronetes/ca.crt \
  --token-file /etc/hedronetes/node-token \
  --node-name hedronetes-worker --node-ip 192.168.104.3 \
  --data-dir /var/lib/hedronetes
```

The server URL must be an HTTPS origin with no user information, extra path,
query or fragment. Redirects, ambient proxies, system trust roots and TLS key
logging are disabled for this client. The explicitly supplied CA pins trust;
normal server hostname/IP verification remains enabled. The node name follows
lowercase DNS subdomain syntax and the IP identifies its reachable interface.

## Identity, restart and recovery

`<data-dir>/agent/identity.json` (0600, in a 0700 directory) persists the server
origin, CA, node name, private key, signed CSR, random node password, and issued
certificate. A process lock prevents concurrent agents using this directory.
Pending enrollment is saved atomically before the network exchange, so a lost
response can be retried without losing the original node password.

The API stores only the node password's SHA-256 hash under its internal
`/registry/h3s-node-identities/<name>` key. This key is not a Kubernetes resource.
The shared token allows new enrollment but cannot reclaim an enrolled name
without its original random node password. A pre-existing Node with no
registration record is rejected; there is no automatic adoption. These checks
and registry writes serialize against ordinary API mutations.

After successful enrollment, omit `--token-file` and remove the worker's shared
token copy. Startup uses the stored node identity and requires no shared token.
The CA file is still required. A restart validates the certificate chain, expiry,
key pair and exact node subject before contacting the API; changing the origin,
CA or node name fails without rewriting the saved identity. The server must
retain both PKI and registry across restarts. Back up private state through an
appropriate private operator channel.

The Node and Lease survive ordinary process restarts. If an administrator
deletes the Node, the agent recreates it and updates its existing Lease's owner
UID. Loss of the worker identity is not automatically recoverable with the
shared token. Enrollment record reset, certificate renewal/revocation, token
rotation orchestration, automatic IP discovery and a declarative NixOS service
module are not implemented yet. Certificates currently expire after one year;
no renewal claim is made.

## Protocol and validation

`POST /v1-h3s/join` accepts strict JSON (`node_name`, `password`, `csr_pem`) with
exactly one `Authorization: Bearer …` header. Impersonation headers are rejected.
The body is bounded to 16 KiB and ten seconds, the CSR to 8 KiB, and concurrent
accepted enrollment requests to four. The signer verifies the CSR signature
and replaces requested subject, SANs, extensions and privileges with a
client-auth leaf for `CN=system:node:<name>, O=system:nodes`. The response contains
only the public certificate and uses `Cache-Control: no-store`. First enrollment
returns 201, a valid retry 200, and invalid credentials 401 without secret data.
Ordinary Kubernetes API bearer authentication is not enabled by this endpoint.

```sh
cargo test -p h3s-apiserver --test bootstrap --locked
```

Real HTTPS tests cover token and CSR failure, oversized bodies, privilege
requests, duplicate/impersonation headers, competing node-name claims,
persisted name ownership, actual agent enrollment and scoped API operations,
process exclusion, restarts without a token, API restart, Node/Lease recovery,
CA/name rebinding and insecure-origin rejection. The separate two-VM evidence
must still establish the installed native worker's behavior on NixOS; these
in-process tests alone do not satisfy the full M1 join/tunnel acceptance check.
