# Strategic merge and Node network metadata

The API accepts `application/strategic-merge-patch+json` for its fifteen served
resource types, alongside JSON Patch and JSON Merge Patch. A native Rust merger
uses Kubernetes v1.34.11 field strategies: maps merge, ordinary arrays replace,
keyed arrays merge by their declared key, and primitive merge arrays deduplicate.
For example, patching Node `status.conditions` by `type` preserves other
conditions; patching a Deployment container by `name` preserves its sidecars.

Supported directives include map/list `$patch: replace`, `$patch: delete`,
`$retainKeys`, `$deleteFromPrimitiveList/<field>` and
`$setElementOrder/<field>`. Ordering preserves the relative patch order and the
untouched server order, interleaving where their old positions allow it. Conflicting
order directives and malformed keyed entries return errors. `$patch: merge` is
not a valid explicit directive in the upstream implementation and is rejected.
New subtrees follow upstream copy/pruning behavior before ordinary typed decoding.

The merger is not a separate authorization path. The result goes through the
existing resource strategy, identity/namespace validation, admission and revision
CAS. A resourceVersion precondition still returns 409 when stale. Rejected
patches commit nothing. Request/result limits remain 2 MiB; strategic recursion
is bounded to 64 levels and a one-million-unit work budget bounds traversals and
list comparisons. Oversized work returns 413. These deliberate resource limits
can reject a patch that a larger Kubernetes deployment could process.

## Flannel and Node status

Flannel's kube subnet manager publishes its backend type, VTEP data, public IP
and management marker through strategic patches to its Node's `/status` endpoint.
Node status now preserves submitted metadata changes, while resetting `spec` to
its stored value. This follows the Node status boundary rather than applying
the metadata reset used by other resource status strategies.

Mandatory node admission still enforces own-node identity and protects
administrative labels, ownership, finalizers and deletion metadata, including
when RBAC is overbroad. A status writer cannot allocate Pod CIDRs or change
taints through `/status`; these desired fields are reset. The same changes to
the main Node endpoint remain denied for node identities. The agent preserves
network annotations and conditions from other components while replacing its
own Ready condition, using the fetched revision to prevent lost concurrent writes.

These API changes do not launch Flannel, allocate Node CIDRs, broaden default
Node topology reads, install CNI rules or prove cross-node traffic. Those are
separate network integration requirements. Server-side apply/managed ownership,
OpenAPI serving, broader validation and full Kubernetes conformance remain
unfinished.

## Provenance and verification

`crates/h3s-apiserver/proto/strategic-schema.json` contains 195 compact reachable
type definitions from the pinned [Kubernetes v1.34.11 OpenAPI document](https://github.com/kubernetes/kubernetes/blob/v1.34.11/api/openapi-spec/swagger.json).
The source SHA-256 is
`d3b0cdc2fda15c753206d25ab459dc7c12df64e2fd652b6809687471ea751c37`.
The schema extractor verifies that hash before writing generated data. The
Apache-2.0 attribution is retained in the generated file; the repository carries
the license. Regenerate with `python3 crates/h3s-apiserver/proto/generate-strategic-schema.py /path/to/swagger.json`.

The 110 checked-in comparison cases use expected outputs/errors from
`k8s.io/apimachinery/pkg/util/strategicpatch` v0.34.11 with real Kubernetes Go
types. They cover Flannel-shaped annotations and conditions, named/numeric field
strategies, nested container environment edits, retained fields, list replacement
and deletion, primitive lists, absent fields/nulls and generated condition ordering.
Rust tests compare complete JSON results. Real TLS/API tests also exercise node
authorization denials, persisted annotations/conditions, stale revisions,
restart and the native agent's heartbeat. These are API compatibility evidence;
they are not the full two-node networking acceptance suite.

The development-only oracle and its Go module checksums live in
`crates/h3s-apiserver/tests/fixtures/strategic-oracle`. It can read the existing
fixture JSON and regenerate expected results into a separate file for comparison.
Go is not used by h3s builds or linked into the control plane. See that directory's
README for exact commands. The implementation reference is the pinned
[upstream merger](https://github.com/kubernetes/apimachinery/blob/v0.34.11/pkg/util/strategicpatch/patch.go);
the network consumer is [Flannel v0.28.9](https://github.com/flannel-io/flannel/blob/v0.28.9/pkg/subnet/kube/kube.go).
