# ConfigMap and Secret volumes

The native worker resolves `spec.volumes[].configMap` and `.secret` through its
node-authenticated API client, in the assigned Pod's namespace. It mounts their
directories into containers through read-only CRI binds, including when the
Pod omits `readOnly` or sets it false. Node authorization still permits only
objects referenced by assigned Pods.

Supported inputs are ConfigMap `data` and `binaryData`, decoded Secret `data`,
`items` key/path selection, `defaultMode` (0644 by default), per-item modes and
optional references/keys. File modes are preserved, with root ownership and
0755 projection directories. A non-root process needs the corresponding Unix
read permission; fsGroup and ownership overrides are not yet implemented.
Required missing objects/keys delay startup and retry. Missing optional sources
produce empty directories. Invalid paths, file/directory collisions, duplicate
mounts, unsafe `/proc`/`sys`/`dev` destinations and unsupported execution fields
produce explicit errors. Limits are 32 volumes per Pod, 1024 files and 1 MiB of
payload per volume, and 32 path components.

Each Pod's `agent/pods/<uid>/volumes` is a dedicated Linux tmpfs with `noswap`,
`nosuid`, `nodev`, mode 0700, a 40 MiB limit and 65536 inode limit. ConfigMap
payloads also use this RAM-backed store. Linux must support tmpfs `noswap`;
mount failure never falls back to writing Secret payloads to disk. Only the
root agent can traverse the host parent; containers see only their specified
read-only volume directories. Credentials stay outside the Nix store and logs.

Each successful reconciliation fetches current references and writes a complete
new generation before atomically replacing the `..data` symlink. File opens
after the switch see new contents; already-open descriptors retain the old inode.
New top-level links and removal of obsolete links follow publication, as in the
[upstream atomic writer](https://github.com/kubernetes/kubernetes/blob/v1.34.0/pkg/volume/util/atomic_writer.go).
Updates are eventual, per-volume, and do not change container fingerprints or
restart running processes. A multi-file reader needing one snapshot can pin a
generation directory. Failed refreshes retain the previous payload and leave
running containers intact, while publishing the existing PodSyncError status.

Agent restart reads the on-disk link layout inside the existing tmpfs, preserves
an unchanged generation, and repairs interrupted link/cleanup operations. After
a complete authenticated assigned-Pod list and successful CRI orphan cleanup,
the worker unmounts the Pod's tmpfs and removes its private directory. It also
collects directories left before sandbox creation. An incomplete API list or
busy mount prevents cleanup; there is no lazy unmount or global cleanup.

SubPath mounts, projected ServiceAccount tokens, downwardAPI volumes, emptyDir,
hostPath, persistent storage and fsGroup are still unsupported. This feature
does not complete the broader M1 workload or recovery acceptance checks.

Validation: `cargo test -p h3s-kubelet` exercises byte decoding, mode/selection
semantics, path rejection, CRI read-only flags, generation switching, held file
descriptors, removed keys, crash repair, unchanged restart and symlink rejection.
Actual tmpfs/CRI permissions, live refresh, process adoption and cleanup require
the separate two-node Linux integration run and its recorded evidence.
