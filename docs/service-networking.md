# Native Service forwarding and Pod DNS

The server's local agent and each separate agent can run the native Service
reconciler using `--service-proxy-nft /absolute/path/to/nft`. It uses the enrolled
node's mTLS identity to list/watch Services, EndpointSlices and Nodes. The
`h3s-service-discovery` role grants discovery reads to `system:nodes`; it grants
no Service/EndpointSlice writes or Secret access. Node topology reads use the
existing topology role. No privileged API credential is passed to the proxy.

The reconciler handles IPv4 ClusterIP TCP/UDP Services and concrete EndpointSlice
ports, including named target ports resolved by the EndpointSlice controller.
It ignores stale Service-owned slices, deleting slices, terminating endpoints
and endpoints explicitly marked unready. Missing readiness means ready, matching
EndpointSlice semantics. Manual slices without Service ownership work within
their namespace and Service-name label. `internalTrafficPolicy: Local` selects
only local endpoints; an empty local selection drops traffic. A Service with no
ready endpoints under Cluster policy rejects traffic. Headless and ExternalName
Services require DNS records rather than packet forwarding.

Every resource LIST must finish with consistent resourceVersion pagination.
Malformed/repeated/excessive continuation, excess items/bytes, API errors and
timeouts never publish partial membership. Watches prompt a fresh complete
snapshot; a periodic refresh covers watch loss. Limits are 8,192 objects and
16 MiB per resource list, 64 continuation tokens, 4,096 Service frontends,
16,384 backends and a 2 MiB generated ruleset. Node CIDRs must be canonical,
private, nonoverlapping and disjoint from the fixed `10.43.0.0/16` Service range.

Only the `ip h3s_proxy` table is managed. Its ownership comment contains a SHA256
digest of the cluster CA and node name. A preexisting table with another owner
is refused. Rules are submitted as one nft transaction; no global flush,
shell interpolation, include files or packet marks are used. The proxy routes
host and Pod Service traffic, balances new connections, masquerades non-Pod
sources and self-hairpin connections, and preserves ordinary cross-node Pod
source addresses. Periodic comparison ignores counters/handles but repairs
owned rule drift, table flushes and deletion. API loss preserves the last valid
rules; process shutdown also retains the table. After process restart, readiness
waits for a fresh API snapshot. Established conntrack flows can outlive a backend
change; a new connection is the membership-change test.

Private generated rules live in the agent's `service-proxy/rules.nft` with a
lifetime process lock. An enabled proxy participates in Node readiness. Missing
or broken nft execution makes networking unready rather than claiming success.
Firewall, forwarding, Flannel/CNI, nft and CRI installation remain operator
prerequisites until the declarative NixOS module supervises those dependencies.
The opt-in flag is a development installation interface, not completion of that
module or the single-binary deployment acceptance contract.

Unsupported Service execution policies are rejected by the API: SCTP,
ClientIP affinity, externalIPs, trafficDistribution, NodePort and LoadBalancer.
IPv6/dual-stack forwarding and full Kubernetes conformance remain unimplemented.
Do not interpret the bounded M1 fixture as full proxy conformance.

## ClusterFirst DNS

Configure both nodes with `--cluster-dns 10.43.0.10`, plus an optional
`--cluster-domain cluster.local` shared with the DNS server. The native kubelet
passes that resolver address, `<namespace>.svc.<domain>`, `svc.<domain>`, the
cluster domain, inherited host searches and `ndots:5` through CRI. Pod dnsConfig
adds nameservers/searches with deduplication and overrides options by name.
`Default` uses `/etc/resolv.conf` plus Pod overrides. `None` uses only explicit
Pod configuration and requires a nameserver. The API defaults omitted dnsPolicy
to ClusterFirst. ClusterFirst without configured cluster DNS fails explicitly.

Resolver input is bounded and rejects newline/whitespace injection, malformed
addresses/domains/options, more than three merged nameservers, more than 32
searches or a search list over 2,048 bytes. Options are limited to 32 entries and
4,096 bytes. Oversized merged configurations fail rather than silently truncate;
this is a documented difference from upstream kubelet. The host resolver file
is bounded to 64 KiB. Cluster DNS currently accepts one unicast IPv4 address.
Host-network Pods and ClusterFirstWithHostNet remain outside native execution
support. Changing node DNS settings affects newly created sandboxes; replace
existing Pods when applying that change.

CoreDNS must run as a real Pod with its Kubernetes plugin watching the h3s API,
using scoped discovery credentials. The DNS Pod uses Default policy to avoid
a bootstrap dependency on its own Service. Use an unprivileged listening port
such as 1053 behind a ClusterIP Service exposing TCP/UDP 53. The upstream 1.14.7
image is pinned at
`docker.io/coredns/coredns@sha256:7efd3c635b03efd68c4e8398fc45f0d993d0e9ab016f72c1cefb0fd6d01aa286`;
its Linux ARM64 manifest is
`sha256:9a631b1e34491f93a35334bc02d8ae190f16224be41689c7f42cc1711a95fe3a`.
Image identification alone does not prove DNS works; require actual cross-node
Pod DNS queries and Service HTTP, EndpointSlice changes and recovery evidence.

The upstream binary carries `cap_net_bind_service=ep`, which conflicts with the
M1 Pod's empty capability bounding set despite listening on 1053. The project
image variant preserves the original base layers, configuration and binary
bytes, replacing only the `/coredns` inode without that file capability. Its
ARM64 manifest is
`sha256:e245030a7f772c63d33f2fa392b73e270fbeb77e5c2590d7688cc754c4d38d0a`;
the executable SHA256 remains
`e9a0052a67f70f59a88092ce466c1605308b41db2a5e4ddffd171fe670863401`.
Import the verified project OCI archive before using its local digest reference
with `imagePullPolicy: Never`; this is not a published registry image.

CoreDNS uses streaming lists to initialize its Service, Namespace and
EndpointSlice caches. h3s supports `watch=true&sendInitialEvents=true` with
`resourceVersionMatch=NotOlderThan`: filtered ADDED events describe one fresh
snapshot, followed by a BOOKMARK annotated `k8s.io/initial-events-end: "true"`,
then changes strictly after the snapshot revision. The completion bookmark is
sent even when periodic bookmarks are disabled, including for an empty result.
Compacted requested revisions can initialize from newer state; future revisions
are rejected using h3s's existing revision error instead of waiting for writes.
Explicit `sendInitialEvents=false` also requires `NotOlderThan`, suppresses the
initial snapshot, and preserves replay from a nonzero revision. Omitting the
flag retains the legacy watch behavior. Node relationship guards remain active
through initialization and later events.

Exclude overlay devices and Pod veths from the host DHCP client's discovery.
For the dedicated NixOS guests the exclusions include `flannel.*`, `h3s-test0`
and `veth*`. A DHCP-assigned link-local address on `flannel.1` can become the
route's preferred source and break host-to-remote-Service masquerading. This
belongs in the declarative host network configuration, not in the proxy.

## Validation and installation boundaries

Unit tests exercise backend selection, ruleset scoping, pagination failure,
kernel-drift comparison and DNS policy translation. TLS API tests verify node
discovery read access and write denials before/after restart. The
`h3s-proxy` example `fixture` emits a representative TCP/UDP ruleset for a real
`nft --check --file -` parse without installing it. These checks do not replace
the NixOS runtime forwarding/DNS tests or Debian/Fedora portability suites.

Installation must record the exact h3s and nft paths, source and binary hashes,
service arguments, DNS image/configuration, scoped credentials' runtime paths,
and activation/verification results. Keep previous package roots and service
configuration for rollback. To uninstall the proxy, stop its agent first and
remove `ip h3s_proxy` only after independently verifying its ownership comment;
do not delete unrelated nft tables. Restoring an older binary leaves generated
rules in the kernel until explicitly removed or replaced by a compatible proxy.

References: [Kubernetes v1.34 kubelet DNS implementation](https://github.com/kubernetes/kubernetes/blob/v1.34.11/pkg/kubelet/network/dns/dns.go),
[CoreDNS Kubernetes plugin](https://coredns.io/plugins/kubernetes/),
[CoreDNS image configuration](https://github.com/coredns/coredns/blob/v1.14.7/Dockerfile),
[Kubernetes watch validation](https://github.com/kubernetes/apimachinery/blob/v0.34.1/pkg/apis/meta/internalversion/validation/validation.go),
and [nftables reference](https://netfilter.org/projects/nftables/manpage.html).
