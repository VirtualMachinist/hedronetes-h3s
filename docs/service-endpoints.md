# Service endpoint reconciliation

The server runs an in-process EndpointSlice controller with the scoped identity
`system:h3s:endpointslice-controller`. It reads Services and Pods and writes
EndpointSlices through the authenticated API. It has no registry dependency,
Secret access, input mutation, Node write or RBAC administration permission.

For each selector-bearing Service, it selects same-namespace, assigned,
nonterminal Pods with a usable Pod IP. It resolves named target ports against
container ports and protocol, then groups endpoints by IP family and the resolved
port set. IPv4 and IPv6 groups are supported by the controller; that does not
claim a working dual-stack datapath. Slices contain at most 100 endpoints, a
Service owner UID, the service-name and manager labels, Pod targetRef UID,
nodeName, readiness/serving/terminating conditions and matching Pod hostname.

Readiness follows the Pod Ready condition. Terminating endpoints are normally
not ready, but retain their serving condition. `publishNotReadyAddresses`
overrides ready without inventing serving readiness. Headless Services can have
selected endpoints; ExternalName and selectorless Services do not receive
automatic endpoints. When there are no selected usable endpoints, one empty
slice per family advertises the Service's ports. Unresolved named ports are
omitted for that Pod; empty slices carry no resolved value for named targets.

The kube-rs controller watches Services and owned slices and requeues each live
Service every two seconds to observe Pod changes. Each pass fetches complete,
bounded Pod and EndpointSlice lists. A continuation or more than 4096 entries
fails the pass before writes. Plans are deterministic across input ordering;
unchanged objects keep their resourceVersion. Port-group replacements are
published before obsolete owned slices are collected, so consumers must
deduplicate endpoint addresses across slices while changes converge.

Every mutation rechecks the current Service UID and resourceVersion (Service
generation alone is insufficient). Replacements and deletes use object CAS;
deletion includes UID and resourceVersion preconditions. A create conflict or
ambiguous response ends the pass and is resolved by a fresh list. The controller
does not adopt another manager's slices or overwrite a colliding foreign name.
Removing a selector collects only the controller's own automatic slices.

An independent five-second cleanup loop re-reads Service owners. It removes
only this manager's single-controller-owned slices whose Service is gone or
has a different UID. API errors preserve data. Other managers, selectorless
manual endpoints and unsupported ownership graphs remain untouched.

Tests cover grouping/partitioning, IP-family selection, unsafe address exclusion,
terminal/terminating/readiness behavior, real TLS API writes, Pod relabeling,
port changes, no-op resourceVersion stability, API restart, Service name reuse,
selector removal, foreign manager preservation and permission denials.

This supplies backend discovery for the networking milestone. Native nftables
proxy execution, Flannel VXLAN configuration, CoreDNS, the server's local agent
and actual cross-node Service/DNS acceptance remain required. No EndpointSlice
test alone proves that network traffic reaches a Pod.
