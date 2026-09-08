# Native single-server scheduling

`h3s server --disable-agent` starts one scheduler alongside the API and namespace
controller. The scheduler uses a separate in-memory client certificate for
`system:h3s:scheduler`. Its bootstrapped RBAC permits Node/Pod reads, Lease lists,
Pod bindings and Pod status updates. It cannot read Secrets, create/delete Pods,
change Pod specifications, or mutate Nodes/RBAC. It has no registry dependency.

Each cycle reads complete bounded Node, Pod and kube-node-lease Lease snapshots
through kube-rs. Incomplete lists or API failures prevent placement. The current
bound is 4096 objects per list; pagination at larger scales remains work. Cycles
run sequentially with a two-second interval and a 60-second request-cycle limit.
The single-server process owns this loop; there is no HA leader election yet.

Only pending, nonterminating Pods for `default-scheduler` are candidates. Named
other schedulers and preassigned Pods are left alone. Priority then creation time
and UID determine queue order. Scheduling gates leave Pods pending. Candidate
Nodes must be Ready, not cordoned/terminating, free of reported pressure/network
failure, and have a fresh Lease for the current Node UID. Lease age is bounded by
its declared duration and at most 40 seconds; a stale Ready condition alone is
insufficient. The explicit condition filter precedes a future node-taint controller.

Placement honors nodeSelector, required node affinity (OR between terms, AND
within a term), preferred node affinity, and NoSchedule/NoExecute tolerations.
Feasible Nodes are ranked by untolerated PreferNoSchedule taints, preferred
node-affinity weight, reserved Pod count and deterministic Node name. CPU/memory
requests are summed across containers; omitted requests use explicit limits.
Pod overhead and existing bound nonterminal Pods count toward reservations.
Resource arithmetic is checked, uses the same quantity conversion as the CRI
limits, and rounds requested units upward. Unsupported resource accounting on
an existing bound Pod makes its Node ineligible rather than assuming zero use.

The native worker reports observed CPU/memory capacity bounded by its cgroup-v2
ancestry. CPU quota capacity rounds downward. Allocatable reserves 100 millicores
and 256 MiB for the OS/runtime and advertises a 110-Pod limit. These are explicit
initial defaults, not a complete kubelet reservation/eviction policy. Missing or
malformed measurement omits capacity; the scheduler then refuses placement.
No capacity is invented for non-Linux development tests. The values describe the
project guest/agent constraints, not the host Studio's resources.

Bindings carry the original Pod UID and resource version and use the authenticated
`pods/binding` endpoint. Successful bindings reserve capacity immediately within
the cycle; subsequent cycles reconstruct usage from API state. Conflicts and
removed Pods are retried from a new snapshot. PodScheduled conditions explain
unschedulable/gated/unsupported placement and use CAS status updates, preserving
other conditions and stable transition times. After a successful binding, a fresh
UID check prevents writing status onto a same-name replacement Pod.

The scheduler does not yet implement preemption, inter-Pod affinity/anti-affinity,
topology spread, persistent-volume topology, host-port allocation, runtime classes,
init/sidecar resource accounting, Pod-level resource budgets or extended-resource
allocation. Those constraints remain pending with an explicit reason. Scheduling
support does not imply that every admitted Pod field is implemented by the
[runtime](pod-runtime.md). Deployment/ReplicaSet control, complete lifecycle,
cross-node Service/DNS and full M1 acceptance remain required.

Validation covers scoped real-TLS API binding, capacity reservation across two
candidates, selector denial, leaving another scheduler's Pod alone, reconstruction
after API/process restart, release after deletion and forbidden API operations.
Placement tests cover stale/wrong-owner Leases, missing capacity, cordon/pressure,
selectors, affinity, taints, gates, unsupported constraints and resource limits.
Installed Linux proof is recorded separately in the Atrium acceptance manifests;
a passing unit test is not a two-node runtime claim.

Kubernetes references: [assignment constraints](https://kubernetes.io/docs/concepts/scheduling-eviction/assign-pod-node/),
[taints/tolerations](https://kubernetes.io/docs/concepts/scheduling-eviction/taint-and-toleration/),
and [scheduler responsibilities](https://kubernetes.io/docs/concepts/scheduling-eviction/kube-scheduler/).
The tested API target remains Kubernetes v1.34; this is a documented native subset,
not a conformance claim.
