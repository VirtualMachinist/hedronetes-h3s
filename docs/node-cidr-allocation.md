# Durable Node CIDR allocation

The native server enables an authenticated Node CIDR controller with
`--cluster-cidr 10.42.0.0/16 --node-cidr-mask-size 24` by default. Each Node
receives `spec.podCIDR` and the matching single-entry `spec.podCIDRs` through the
normal Kubernetes PATCH endpoint. The controller has no registry dependency.

M1 supports canonical private IPv4 pools, disjoint from the current Service
range `10.43.0.0/16`, with at most 4096 fixed-size node subnets and no prefix
longer than /30. IPv6, dual-stack, resizing and automatic subnet reclamation
are unsupported. Choose the pool before initializing the server registry.

## Reservation and recovery semantics

The API owns a versioned ledger at the private registry key
`/registry/h3s-node-cidrs/allocations`. It validates all existing reservations and
imports allocated Nodes from a complete, fixed-revision paginated LIST before
starting the server. Invalid, colliding or inconsistent allocations and changes
to the configured pool fail startup rather than reset the ledger.

All Node writes pass the API's admission mutex. The API checks UID and resource
version preconditions, object validation, node authorization and duplicate
creation before reserving a subnet. It commits the reservation with registry CAS
before committing the Node. Concurrent assignments to different Nodes therefore
cannot claim the same subnet. A failed or interrupted Node write can leave a
reservation; it cannot make that subnet available to another name. A retry or
controller restart resumes the same name's reservation. No multi-object atomic
transaction is implied.

Once assigned, a Node's CIDR cannot be changed or cleared, including by an admin.
Deletion retains its reservation. A recreated Node with the same stable name
receives that network again; the separate bootstrap name/password binding still
controls who can enroll with that name. Exhaustion fails explicitly. Investigate
retained allocations and stale runtime resources before any operator migration;
there is no automatic or exposed ledger-delete/reclaim API in this version.
Restore registry and network state together. Deleting the ledger is not a
supported recovery procedure.

The controller reads `GET /v1-h3s/network/node-cidrs` using its dedicated
`system:h3s:node-cidr-controller` client identity. This read-only endpoint exposes
configuration and reservations, accepts no query parameters, and requires an
exact non-resource RBAC grant. It is not part of Kubernetes discovery. Node and
ordinary authenticated identities do not receive access. The controller can
get/list/patch Nodes but cannot delete them, write their status or read Secrets.
A stale snapshot or competing patch may conflict; the controller retries from
fresh API state every two seconds. It rejects incomplete Node lists before
writing.

## Flannel topology access

The `h3s-network-topology` role grants the `system:nodes` group Node get/list/watch
for Flannel topology discovery. Nodes can submit their own Flannel annotations
through strategic Node status patches. Mandatory node admission still rejects
foreign Node writes, self-assigned CIDRs, privileged labels and other protected
metadata. Status strategy restores the stored spec; agent heartbeats preserve
network conditions. Topology read access does not grant Secret or allocation
ledger access.

## Migrating the existing Tower fixture

The initial worker runtime uses a local bridge configured for `10.42.2.0/24`.
Before installing this server version, inspect the actual CNI configuration and
Node inventory, verify no workloads are left, and record that existing subnet on
`hedronetes-worker` through an authenticated admin update with UID/resourceVersion
preconditions. The new server imports this allocation. Do not let the allocator
choose another subnet while the old CNI configuration still uses `10.42.2.0/24`.
This represents the existing runtime and does not prove cross-node networking.

Flannel supervision/CNI migration, the server's local agent, native nftables
Service forwarding, CoreDNS, and the complete cross-node traffic/recovery fixture
remain separate required implementation work. CIDR assignment alone does not
pass H3S-06 or the Debian/Fedora portability checks.

## Verification

`cargo test --workspace` includes real TLS API/controller tests for competing
assignments, concurrent controller passes, UID/RV-protected updates, invalid
subnets and list consistency, exhaustion, deletion/recreation and server restart.
Negative API tests cover topology-only node access, Flannel-shaped status
requests and scoped controller permissions. SQLite tests exercise an interruption
between reservation and Node commit, reopen, configuration drift, corrupt
reservations, and imports spanning more than one registry LIST page.
