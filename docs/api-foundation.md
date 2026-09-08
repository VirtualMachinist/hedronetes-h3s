# Executable API checkpoint

The `server --disable-agent` path now serves a real rustls/Axum API backed by the SQLite registry and persistent cluster PKI. Default-ServiceAccount/public-CA reconciliation runs through the API; workload controllers, scheduling, container execution, worker join, and cluster networking are still incomplete. This checkpoint is not a functioning workload cluster or completed M1.

For a local development run:

```sh
cargo run -p h3s -- server --disable-agent \
  --data-dir "$PWD/../runtime" \
  --write-kubeconfig "$PWD/../runtime/server/admin.kubeconfig" \
  --bind-address 127.0.0.1
export KUBECONFIG="$PWD/../runtime/server/admin.kubeconfig"
kubectl get namespaces
kubectl create namespace api-smoke
kubectl create configmap api-check -n api-smoke --from-literal=result=verified
kubectl get configmap api-check -n api-smoke -o json
kubectl delete configmap api-check -n api-smoke
```

Private runtime data is outside the source tree. Use `--tls-san` for additional addresses. Startup refuses a different existing kubeconfig or a persisted certificate that does not cover the requested SANs. This protects existing credentials rather than silently replacing the trust root. Database and PKI directories require mode 0700; generated kubeconfigs are 0600.

Implemented discovery/resource handlers cover Namespace, ConfigMap, Secret, Pod, Node, Service, ServiceAccount, Deployment, ReplicaSet, EndpointSlice, Lease, Role, RoleBinding, ClusterRole, and ClusterRoleBinding. Namespace/Pod/Node/Service/Deployment/ReplicaSet expose separate status endpoints; Pods expose a create-only binding endpoint. See [workload API boundaries](workload-api.md). Create/get/update/JSON patch/merge patch and bounded paginated LIST/WATCH use the API storage boundary. Deletes use revision and optional UID/resourceVersion preconditions; namespace deletion and pending-finalizer processing remain controller work. TLS identities drive RBAC, impersonation is rejected, and RBAC mutations remain administrator-only until escalation checks are implemented. HTTP health/version are public; other paths require a verified client certificate. Bound ServiceAccount and bootstrap bearer authentication remain pending.

Stock kubectl sends some writes as Kubernetes Protobuf. The API accepts JSON and these Protobuf envelopes, using the checked-in upstream 1.34 descriptor and then k8s-openapi validation. Responses currently use JSON. Watch responses are newline-delimited JSON events, never SSE; a replay after LIST uses the global resourceVersion and errors after compaction. Label selectors (equality, sets, existence and numeric comparisons) and resource-specific field selectors apply to LIST and WATCH. A watch reports ADDED when an object enters the selection and DELETED with its prior matching value when it leaves; durable replay preserves these transitions after compaction and restart. Name-restricted RBAC allows collection LIST/WATCH only when an exact metadata.name field selector restricts the result. Continuation tokens bind the snapshot, resource/namespace prefix, and selectors. The registry reads bounded pages, and LIST serializes each item into the response stream.

Remaining API work includes strategic merge patch/server-side apply, scale and streaming/eviction subresources, OpenAPI, the remaining admission chain and full field validation/defaulting, additional mutable Pod transitions, complete RBAC escalation/bind rules, remaining node-admission coverage, finalizers/namespace deletion, and Helm/client workflow coverage. Invalid selectors, unsupported field selectors, dry-run, unsupported patch media types, and unimplemented routes return errors; the existing tests do not certify the missing behavior.

The Tower checkpoint runs as the project guest user's transient `h3s-api-foundation` systemd unit. The binary is `/home/abdul-qadir.guest/hedronetes-m1/h3s/target/debug/h3s`; data and credentials are under `/home/abdul-qadir.guest/hedronetes-m1/runtime`. Restart/stop only this named project unit. A declarative NixOS service module and production runtime installation remain required for M1.

Validation covers TLS/HTTP authentication and negative cases, namespace-scoped RBAC, real storage mutations/conflicts/delete preconditions, fixed-snapshot pagination, watch replay, Secret stringData conversion, and restart persistence. Stock kubectl 1.34.11 on NixOS successfully created/read/deleted a ConfigMap and read the same UID/resourceVersion/data after a service restart. Namespace `api-smoke` remains as a known fixture until namespace deletion is implemented; its test ConfigMap was removed.

Run `integration/tower/check-selectors.py` with explicit absolute `--kubectl` and `--kubeconfig` paths and an existing project `--namespace`. It checks filtered pagination, durable watch transitions, stock kubectl paging, exact-name field selection, and JSON/merge patches, then removes its uniquely named ConfigMaps using UID delete preconditions. The script prints fixture-only results and does not print credentials.

PATCH accepts RFC 6902 `application/json-patch+json` and RFC 7396 `application/merge-patch+json`. The result follows the same validation and CAS write path as PUT, with a distinct RBAC `patch` permission; non-admin RBAC patches cannot bypass the temporary escalation guard. Failed tests, stale resourceVersion conditions, invalid values and oversized growth make no storage change. JSON patches are limited to 256 operations and each intermediate object is bounded to 2 MiB. Concurrent writes may return 409; callers should fetch current state and retry deliberately. Strategic merge and server-side apply still return 415 pending their separate merge/ownership implementations.

Patch contracts: [RFC 6902](https://www.rfc-editor.org/rfc/rfc6902), [RFC 7396](https://www.rfc-editor.org/rfc/rfc7396), and [json-patch 4.2.0](https://docs.rs/json-patch/4.2.0/json_patch/).

Pod Security enforcement and the terminating-namespace create guard are implemented at the write boundary; see [admission behavior and remaining work](admission.md).

[Node access](node-access.md) now supports scoped self-registration, status/lease updates, assigned-Pod reads, reference-based Secret/ConfigMap reads, and mandatory node write restrictions. Bootstrap exchange and the actual worker/tunnel/runtime are still pending.
