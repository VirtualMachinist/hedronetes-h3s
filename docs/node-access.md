# Node API access

Verified client certificates with CN `system:node:<name>` and organization
`system:nodes` now receive node API permissions without an administrator
kubeconfig or a broad `system:node` ClusterRole. Both identity attributes are
required. TLS validation and ordinary RBAC still apply; HTTP headers cannot
supply a node identity.

The [native agent](worker-bootstrap.md) now uses these permissions for token/CSR
enrollment, Node registration and Lease renewal. The supervisor WebSocket
tunnel and CRI workload runtime remain unfinished. The agent reports NotReady;
a Node registration or status test does not demonstrate container execution.

## Supported permissions

| Resource | Built-in node access |
|---|---|
| Node | Create its own object; get or exact-name list/watch of itself; update/patch its own object and status |
| Pod | Get assigned Pods; list/watch with an exact `spec.nodeName` selector or a related named Pod; update/patch assigned Pod status; delete assigned Pods with normal preconditions |
| Secret, ConfigMap | Get or exact-name list/watch when referenced by a Pod assigned to this node in the requested namespace |
| Lease | Get/create/update/patch/delete its own named Lease in `kube-node-lease` |
| Service, EndpointSlice | Read/list/watch for the node's service-network implementation |

There is no built-in grant to mutate workloads, bind Pods, read unrelated
Secrets, list all Pods or Nodes, mutate RBAC, or delete the node itself. Discovery
uses the existing authenticated discovery binding. ServiceAccount TokenRequest
and its Pod/audience binding checks will be added with token issuance.

The pure permission policy lives in `h3s-auth`; the API supplies current
relationships from persisted Pod objects through its storage boundary. Secret
and ConfigMap relationships include ordinary and projected volumes, image pull
Secrets, env/envFrom for normal/init/ephemeral containers, and CSI node-publish
Secret references. Arbitrary names in annotations, literal environment values
or commands do not grant access. Legacy storage-plugin reference forms are not
implemented. A paginated registry snapshot supplies each relationship scan;
there is no separate node graph that can survive with stale grants after restart.

Parsed field selectors constrain the actual LIST/WATCH output. Name-only Pod
watches also acquire a node selector so deletion/recreation under the same name
on another node cannot expose that replacement. Relationship-based Secret and
ConfigMap streams recheck before emitting data; relationship loss produces a
watch ERROR/403 or closes a partially streamed list. Watch rechecks occur when
events arrive, not on a separate timer. Already delivered data cannot be revoked.

Explicit RBAC grants remain additive, as in Kubernetes's Node/RBAC authorizer
combination. An administrator can deliberately broaden a node's reads. Node
write admission still runs when RBAC granted the request, including an
overbroad role. Avoid binding unrestricted roles to node identities.

## Write restrictions

Admission is serialized with registry writes and uses the persisted object's
assignment, not a submitted `spec.nodeName`. A node may not bind Pods, update
their desired state, create ordinary or mirror Pods, modify foreign Pod status,
or delete foreign Pods. Pod resource-claim allocation status is protected.
Status strategy preserves stored spec and metadata before admission.

Node updates cannot add, change, or remove administrative labels in reserved
Kubernetes domains. The v1.34 kubelet label exceptions and node/kubelet label
namespaces remain available; `node-restriction.kubernetes.io` and its subdomains
are always protected. Existing administrator labels are preserved. Nodes cannot
change taints after registration, allocate Pod CIDRs, configure dynamic
`configSource`, or change Node ownership/finalizer/deletion metadata. Initial
taints are allowed. Empty optional string/list representations are treated as
equivalent when checking these fields.

M1 deliberately keeps network allocation and Node ownership with administrative
controllers. Full NodeRestriction coverage for unimplemented resources, mirror
Pods, certificate renewal, storage allocation, token audiences and delegated
kubelet authentication is still pending. This does not claim full conformance.

Policy references: [Kubernetes v1.34 node authorization](https://v1-34.docs.kubernetes.io/docs/reference/access-authn-authz/node/),
[v1.34 NodeRestriction implementation](https://github.com/kubernetes/kubernetes/blob/v1.34.0/plugin/pkg/admission/noderestriction/admission.go),
and [kubelet label definitions](https://github.com/kubernetes/kubernetes/blob/v1.34.0/staging/src/k8s.io/kubelet/pkg/apis/well_known_labels.go).

## Verification

`cargo test -p h3s-apiserver --test nodes --locked` runs real rustls/HTTP requests
against the SQLite API. It covers self-registration and persisted status,
identity attributes, lease scope, overbroad-RBAC mutation restrictions, reserved
label removal and patch forms, assigned/unassigned/foreign Pods, reference and
namespace isolation, status/binding/delete restrictions, watch recreation, and
Secret stream revocation. These are API integration tests, not the two-VM
join/runtime acceptance suite.
