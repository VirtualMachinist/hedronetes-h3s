# Native assigned-Pod runtime

`h3s agent --container-runtime-endpoint unix:///absolute/containerd.sock` runs a
native CRI reconciler alongside independent Node/Lease heartbeats and the
supervisor tunnel. Without the endpoint it still enrolls, but remains NotReady.
The endpoint must identify a local Unix socket. With runtime execution enabled,
run the agent with the privileges required by the configured runtime socket and
private Pod state; the Tower fixture uses a root systemd service.

The node identity lists only Pods assigned to its node through the authenticated
API. A complete, validated list is required before orphan cleanup. Sandboxes and
containers carry h3s node and Pod UID labels; user Pod labels cannot override
these. After process restart the agent discovers/adopts existing runtime
objects. It handles committed-but-unacknowledged create/start operations on the
next reconciliation rather than assuming a failed RPC had no effect. Cleanup
checks ownership and removes only its own resources. Pod status writes retain
API resourceVersion/UID protection; conflicts retry from the next list.

The implementation creates sandbox network/PID/IPC isolation through CRI,
selects the configured youki handler, creates and starts containers, observes
CRI state, and reports Pod/container IDs, addresses, restart counts and readiness.
A changed container specification replaces the container. Always, OnFailure
and Never govern exited-container restart; crash retries back off from ten to
300 seconds. An API failure does not remove workloads. Root Pod logs/state live
under `<data-dir>/agent/pods/<pod-uid>`. A successful orphan cleanup removes that
Pod's local state; container images remain cached.

Environment inputs support explicit values and Kubernetes-style `$(NAME)`
expansion, ConfigMap/Secret key references, envFrom, and basic downward identity
fields. References use node-scoped API access and resolve at container creation,
not on every sweep of a running container. Optional missing references are
honored. Binary ConfigMap data is not treated as environment text. Secret values
and CRI error bodies are not written to operator logs or Pod error messages.

CPU/memory limits translate with exact fixed-point arithmetic into CRI Linux
resources. The current security implementation requires an explicit non-root
numeric UID, disabled privilege escalation, dropped ALL capabilities and
RuntimeDefault seccomp. It preserves requested root-filesystem read-only mode and explicitly supplies the
Kubernetes v1.34 default masked/read-only proc paths. The container fingerprint
includes a runtime-profile version and Pod security context, so upgrading these
defaults replaces containers created with the older profile.
Image pull policy is honored for public images. Readiness supports exec, TCP and
HTTP with delay, period, timeout and success/failure thresholds; network probes
use the runtime-observed Pod IP, never a caller-selected host, credentials,
redirect destination or ambient proxy.

## Current limits

This is an implementation checkpoint toward the full M1 workload contract.
Scheduling, Deployment/ReplicaSet reconciliation, cross-node networking,
ClusterFirst DNS and the server's embedded agent remain unfinished. A Ready Node
means its configured CRI runtime and CNI plugin report ready; it does not certify
cross-node Service/DNS or the complete Kubernetes behavior.

The initial workload path requires `automountServiceAccountToken: false`,
`enableServiceLinks: false`, and DNS policy Default or None. Service-account
projection, service environment injection, volumes, init/ephemeral containers,
private image credentials, liveness/startup probes, host networking/ports,
custom security profiles and other unimplemented execution fields return a
PodSyncError instead of being silently ignored. Termination grace is currently
bounded to 0–30 seconds. Termination-message files and full Kubernetes status/
backoff conventions remain to be completed. These limits do not waive any M1
acceptance requirement or change M2's full-conformance scope.

The current API deletes Pods immediately; the reconciler removes the orphan's
runtime resources on its next complete list. General graceful API deletion,
finalizers/garbage collection, volume cleanup and hard power-loss behavior still
need their own implementation and verification.

## Verification

Workspace tests cover assignment/path/ownership boundaries, resource arithmetic,
probe destination restrictions, environment expansion, restart-policy/adoption
choices, stable condition transitions, and the existing authenticated API,
enrollment, supervisor and serving identities. A protocol/unit test is not proof
of a running container. Installed tests must create real Pods through stock
kubectl, inspect actual CRI IDs/processes, verify readiness and ConfigMap/Secret
consumption without leaking values, restart the agent/runtime, force a container
exit, observe replacement, and delete the Pod with scoped runtime cleanup.

The live acceptance manifest records the tested commit/package and the exact
configuration. Temporary Tower root service installation is not the final
NixOS module or a portability result. The complete 24-check contract remains
in Atrium.

Proc policy source: [Kubernetes v1.34 security-context utilities](https://github.com/kubernetes/kubernetes/blob/v1.34.0/pkg/securitycontext/util.go), Apache-2.0.
