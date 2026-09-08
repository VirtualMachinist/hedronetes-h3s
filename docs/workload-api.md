# Workload API checkpoint

The API can persist and watch the objects needed by M1's controllers and worker. It does not yet execute containers, register a real worker, reconcile Deployments, or program Service traffic. API fixtures and a stored Node object cannot satisfy those runtime checks.

| API group | Resources | Subresources |
|---|---|---|
| core/v1 | Namespace, ConfigMap, Secret, Pod, Node, Service, ServiceAccount | Namespace/Pod/Node/Service status; Pod binding |
| apps/v1 | Deployment, ReplicaSet | status |
| discovery.k8s.io/v1 | EndpointSlice | none |
| coordination.k8s.io/v1 | Lease | none |
| rbac.authorization.k8s.io/v1 | Role, RoleBinding, ClusterRole, ClusterRoleBinding | none |

All listed resources support create/get/list/watch/update, JSON Patch and JSON Merge Patch. Namespace deletion remains unimplemented; other deletes use the existing immediate, revision-checked path and reject pending finalizers. Graceful Pod termination and garbage collection remain runtime/controller work. The protobuf decoder uses the corresponding unchanged Kubernetes 1.34.11 descriptors; ordinary JSON requests remain supported.

Status supports GET/PUT/PATCH with separate RBAC permissions such as `pods/status`. Ordinary create/update requests cannot set observed status. Status writes preserve the stored spec and metadata, including ownership, while assigning a new resourceVersion. This intentionally constrained metadata behavior still needs comparison with all upstream per-resource status strategies before full conformance. Status payloads remain typed through k8s-openapi; full semantic validation remains incomplete.

Pods begin Pending; the API does not report Running before a worker observes it. Pod, Deployment, and ReplicaSet generations start at one and advance on admitted spec changes; Deployment annotation changes also advance generation. Metadata-only relabeling and status updates preserve generation. Defaults include the common container image policy, restart/DNS/scheduler/service-account settings, replica count and rolling-update settings. Validation covers required containers/images, unique container names, ports/protocols, matching nonempty workload selectors, nonnegative replica counts, and immutable selectors. Secret/ConfigMap immutable data is enforced. This is a documented subset of field validation, not the complete admission chain.

The create-only `/api/v1/namespaces/<namespace>/pods/<name>/binding` endpoint accepts a v1 Binding targeting an existing Node. It requires `create` on `pods/binding`, validates endpoint identity and optional Pod UID/resourceVersion preconditions, and uses CAS to assign an unbound, nonterminating Pod once. It cannot substitute for the scheduler's placement checks or worker registration. Existing Pod updates permit image changes; other spec mutations, including direct nodeName changes, currently fail. More upstream-approved Pod update transitions remain to be implemented.

Service allocation currently supports IPv4 SingleStack ClusterIP Services in the initial M1 range `10.43.0.0/16`, headless Services, and ExternalName objects. Network/broadcast addresses and `.1` are reserved; `.1` is intended for the eventual API Service. One API instance serializes Service writes, chooses an unused IP from persisted Services at a consistent revision, and commits the allocation with the Service itself. Restart needs no separate allocation repair; deletion frees the address. Updates preserve allocation fields. Multi-server allocation, configurable CIDRs, dual-stack, NodePort/LoadBalancer behavior, and Service-type transitions remain incomplete. Persisting an ExternalName or ClusterIP does not yet implement DNS/proxy behavior.

Validation uses actual TLS/HTTP and SQLite persistence, including concurrent allocation and binding, restart, status-only RBAC, generation behavior, immutable updates and invalid workload fixtures. Run `integration/tower/check-workload-api.py` with explicit absolute `--kubectl`/`--kubeconfig` and an existing project `--namespace` to exercise stock client creation, reads, status and binding. Its synthetic Node and all other uniquely named API fixtures are removed with UID preconditions; it explicitly reports that containers and worker join were not tested.

Upstream strategy references: [Pod](https://github.com/kubernetes/kubernetes/blob/v1.34.11/pkg/registry/core/pod/strategy.go), [Deployment](https://github.com/kubernetes/kubernetes/blob/v1.34.11/pkg/registry/apps/deployment/strategy.go), [Node](https://github.com/kubernetes/kubernetes/blob/v1.34.11/pkg/registry/core/node/strategy.go), and [Service](https://github.com/kubernetes/kubernetes/blob/v1.34.11/pkg/registry/core/service/strategy.go).
