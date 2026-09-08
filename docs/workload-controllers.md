# Native Deployment and ReplicaSet controllers

The standalone server runs kube-rs Deployment and ReplicaSet controllers plus a
bounded background workload collector. Separate in-memory client certificates
have bootstrapped RBAC for each role. Deployment reconciliation reads/writes
ReplicaSets and its own status; ReplicaSet reconciliation manages Pods and its
own status; collection reads owners and deletes supported dependents. None can
read Secrets, mutate Nodes/RBAC, or change/delete Deployment specifications.
All operations use the authenticated API; this crate has no registry dependency.

Controllers watch their parent and owned child resources, re-read the parent,
and requeue periodically. Complete namespace LISTs bound the supported inventory
to 4096 entries. ReplicaSet creation/deletion uses bursts of at most eight Pods
per reconciliation. Desired replicas are currently bounded to 0–4096. Errors or
incomplete lists never authorize deletion. Parent UID/generation checks precede
changes; writes and deletes use resource-version/UID preconditions. A racing
parent change is re-read on the next cycle. This is single-server reconciliation,
not HA leader election.

ReplicaSets count active owned Pods, claim matching orphans and release owned
Pods whose labels no longer match. Another controller's ownership is preserved.
Names use server-side generateName; an ambiguous create failure is followed by
a fresh complete list before another attempt. Unready/unassigned/newer Pods are
preferred for scale-down. Status reports replicas, full template labeling,
readiness and availability after minReadySeconds. Pod creation failures produce
ReplicaFailure with a bounded API status/message; successful repair clears it.
Terminated Pods do not satisfy the replica count. Template labels and annotations
are supported; other populated Pod-template metadata currently fails explicitly.

Deployments create UID-owned ReplicaSets with a deterministic template hash,
controller-owned pod-template-hash labels, and revision annotations. Existing
matching sets are reused; matching orphan sets can be adopted. ReplicaSet changes
wait for observedGeneration. RollingUpdate rounds percentage maxSurge upward and
maxUnavailable downward, grows the new set within the surge budget and removes
old available replicas only when aggregate availability permits. Unavailable old
replicas can be replaced without removing healthy capacity. Recreate scales old
sets to zero and waits for their API Pods to disappear before scaling the new set.
The API currently removes Pods before runtime grace completes; Recreate is not a
claim of process-level zero overlap through all failure/deletion races.

Deployment status reports observed generation, total/updated/ready/available
replicas, availability, progress and ReplicaFailure. Progress deadlines remain
failed until a genuine observation or desired-state change advances reconciliation.
Paused Deployments do not initiate a rollout; scaling while paused is not yet
implemented. Zero-replica old sets are retained according to revisionHistoryLimit
and removed only after completion and verification that no Pod references them.
Rollback can reuse a retained matching template; complete upstream revision/history
annotation and rollout-undo behavior remains unfinished.

Background collection currently supports one controller owner for Pod→ReplicaSet
and ReplicaSet→Deployment relationships. It re-reads the named owner and compares
UIDs, so same-name replacement does not preserve a dependent of the old UID.
Unknown kinds, additional owner references and API errors are preserved for a
future general GC graph. Deletion carries UID and resource-version preconditions;
conflicts are retried, not counted as successful removal. Foreground and orphan
propagation are explicitly rejected by the API instead of silently applying
background collection. General finalizers, namespace deletion and graceful Pod
API deletion remain unfinished.

API tests cover actual scoped TLS requests, rolling availability budgets, scale,
replacement, persisted-state restart, ownership, collection, forbidden operations
and real Pod Security admission failure/recovery. Their Ready statuses are supplied
by the test and are not runtime evidence. The installed two-node fixture is
recorded separately with actual worker/container readiness. Config/Secret mounts,
full lifecycle/status, cross-node Service/DNS, persistent NixOS activation and full
M1 integration/portability acceptance remain required.

The older check-workload-api.py fixture predates active workload reconciliation;
its synthetic bindings/status writes need isolation/adaptation before using it
against an active controller set. Its earlier results remain historical API proof.
The native Deployment fixture must prove actual ownership, container execution,
rollout, recovery and cascade cleanup and must not substitute API-only status writes.

References: Kubernetes [Deployments](https://kubernetes.io/docs/concepts/workloads/controllers/deployment/)
and [ReplicaSets](https://kubernetes.io/docs/concepts/workloads/controllers/replicaset/).
The API target remains v1.34; full conformance belongs to M2.
