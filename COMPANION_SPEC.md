# Hedronetes Companion Plane Specification (h3s-cp)

**Four standalone tools that sit beside Hedronetes, not inside it.**

Version 0.1.0-draft · 7 September 2026 · Apache-2.0 (this document)
Parent: `SPEC.md` v0.1.0-draft

Tagline: *h3s runs the work. Facet performs the calls. HedronDB remembers the intent. Turso can persist the ledger. Herdr is where the agents sit.*

---

## C0. How to read this document

This is a **companion specification**. It does not amend, relax, or override
`SPEC.md` §§0–21. If a sentence here conflicts with the parent
spec, the parent spec wins.

Normative language: **MUST**, **SHOULD**, **MAY**, **MUST NOT**.

The four companions are products with their own repos, licenses, and
roadmaps. This document specifies *how Hedronetes treats them*, not how
those projects must evolve. Where a companion's own docs contradict a
"MUST" here, treat the MUST as an h3s integration constraint and file the
gap; do not fork the companion to force compliance.

| Companion | Repo (SoT at time of writing) | Role in this spec |
|---|---|---|
| **Facet** | `VirtualMachinist/facet` | Action plane — API client, Lattice ledger, MCP |
| **HedronDB** | `VirtualMachinist/hedrondb` | Intent plane — desired/observed + causal log |
| **Turso / libSQL** | `tursodatabase/libsql`, `tursodatabase/turso` | Optional engine under Lattice (and, later, under `--store=sqlite`) |
| **Herdr** | `herdrdev/herdr` | Habitat plane — agent multiplexer, local socket |

h3s itself remains the **substrate plane**. It is specified in the parent
document. This companion does not add a third personality to the `h3s`
binary, a new `--store=` default, or a RuntimeClass.

---

## C1. Charter

Hedronetes is agentic Kubernetes only in this sense:

> Agents are workloads and operators that **reconcile declared intent
> against observed cluster and world state**, using the same spec/status
> split Kubernetes already has. They speak the Kubernetes wire protocol
> as clients. They do not live inside `kube-apiserver`.

The four companions supply the missing planes that Kubernetes does not:

1. A place for agent *processes* to live and be herded (Herdr).
2. A way for those processes to *call* APIs with a durable, queryable
   record (Facet + Lattice).
3. A way for those processes to *declare and audit intent* that is not
   a Pod object (HedronDB).
4. An optional SQLite-family engine so the action ledger can move off a
   single rusqlite file later (Turso / libSQL).

This is an overlay. Stock `kubectl`, CRI, chunked-HTTP watch, join tokens,
and `--store=sqlite|etcd` stay exactly as the parent spec wrote them.

### C1.1 Why a companion spec exists

The parent spec is a k3s-shaped distro. "Agentic" is a workload class and
an operator story, not a rewrite of the apiserver. Without this document
those four names collapse into `--store=turso` or "put the LLM in the
scheduler." This spec exists so a later contributor cannot do that in
review and call it architecture.

### C1.2 Binding to the parent spec

| Parent | Companion must honor |
|---|---|
| §1.2 paths | `/etc/hedronetes/`, `/var/lib/hedronetes/`, kubeconfig `h3s.yaml`, `:6443` |
| §3 personalities | `h3s server` / `h3s agent` keep their meanings. See §C4. |
| §6 compatibility | Stock kubectl. No new YAML dialect. No SSE watches. |
| §7 Storage | Cluster objects stay behind `trait Storage`. Companions MUST NOT implement that trait by wrapping HedronDB or Lattice. |
| §13 flags | v0.1 flag surface unchanged. Companion flags live on `facet`, `hedron`, `herdr`, not on `h3s`. |
| §16 DoD | h3s v0.1 / v0.2 / v0.3 DoD does not include these four tools. |
| §21 provenance | Ideas and interfaces, not git subtrees. Same rule here. |

---

## C2. Goals and non-goals

### C2.1 Goals

1. Name the four planes and forbid collapsing them into one process or
   one SQLite file.
2. Define join keys so a Lattice run, a HedronDB desired state, a Herdr
   pane, and a Kubernetes object can be correlated.
3. Define attach points (workstation, jump box, optional node) without
   inventing an in-cluster Herdr control plane.
4. Keep Facet as the agent-facing Kubernetes client (OpenCollection +
   MCP + deterministic JSON).
5. Keep HedronDB as the intent store (Warm `current_state` vs Cool
   `causal_chain`, never mixed).
6. Treat Turso/libSQL as an *engine option*, not a product plane and not
   default HA for h3s.
7. Treat Herdr as the recommended operator habitat, not a scheduler.
8. Preserve air-gap: no companion MAY require Turso Cloud or a hosted
   Herdr control plane for the default story.

### C2.2 Non-goals

- Embedding Facet, HedronDB, Turso, or Herdr in the `h3s` multicall
  binary.
- Replacing kubectl, kube-scheduler, or the kubelet FSM.
- Using HedronDB or Lattice as `--store=sqlite` / `--store=etcd`.
- Exposing `herdr.sock` or `facet mcp` as a Kubernetes Service on `:6443`.
- Requiring Herdr to boot a cluster.
- An Agent CRD in h3s v0.1.
- Claiming Facet 0.5.7, HedronDB Phase 0, or Herdr v0.9 as soaked h3s
  components.
- Vendor-forking Herdr (third-party, Apache-2.0, 36k stars).
- Treating "Turso" as one thing. See §C7.1.

### C2.3 "Beside, not instead"

Copied from Facet's own engine rule and applied to the whole overlay:

> Engines and planes sit **beside** each other. No cutover that makes
> Lattice the cluster store, HedronDB the watch bus, Herdr the kubelet,
> or Turso Cloud the HA default.

A change that merges two planes into one database or one binary is out
of scope for this spec.

---

## C3. Planes and stores

### C3.1 Four planes

```
                    humans
                      |
                      v
+------------------- Herdr --------------------+
|  habitat: workspaces, tabs, PTYs, sidebar    |
|  socket + CLI; states working/blocked/idle   |
+---+----------+-----------+-------------------+
    |          |           |
    v          v           v
+--------+ +--------+ +-----------+
| Facet  | |HedronDB| | h3s       |
| action | | intent | | substrate |
| MCP,   | | Warm / | | :6443     |
| Lattice| | Cool   | | CRI, RBAC |
+----+---+ +---+----+ +-----+-----+
     |         |            |
     v         v            v
 lattice.db  vault.db   Storage trait
 (rusqlite   (hedron-   (sqlite|etcd|
  or libsql)  core)      postgres|…)
```

| Plane | Job | Source of truth | Query language |
|---|---|---|---|
| Habitat (Herdr) | Where agent *processes* live | Herdr session server (layout + pane metadata) | CLI + NDJSON socket |
| Action (Facet) | How agents *call* APIs | Lattice (`lattice.db`) | SQL over Lattice; MCP tools |
| Intent (HedronDB) | What agents *want / remember* | vault file (`nodes/edges/desired_states/events`) | HQL |
| Substrate (h3s) | What *runs* | `trait Storage` + CRI | Kubernetes API |

Turso/libSQL is **not a fifth plane**. It is an engine that MAY sit under
Lattice (and, after the rusqlite Kine table exists, MAY sit under
`--store=sqlite`). See §C7.

### C3.2 Three stores, three jobs

| Store | File / backend | Holds | Does not hold |
|---|---|---|---|
| h3s-storage | `/var/lib/hedronetes/` per parent spec | Pods, Deployments, RBAC, Events, secrets at rest as the API defines | Agent tool-call bodies, HQL graphs, PTY layout |
| Lattice | `.facet/lattice.db` + machine store | Request runs, blobs, timings, sessions, actor | Cluster objects, HedronDB nodes |
| HedronDB vault | operator-chosen path, mode **0600** | nodes, edges, desired_states, events | Lattice runs, `/registry/…` keys |

A single Turso/libSQL database MUST NOT contain more than one of these
schemas. Compact, watch, and HQL contracts differ. Mixing them is the
failure mode this spec exists to prevent.

### C3.3 Projection, not identity

Objects may be *projected* across planes. They MUST remain distinct.

| From | To | Meaning |
|---|---|---|
| Pod status | HedronDB node observed fields | "what the agent believes the cluster showed" |
| Facet run | HedronDB `caused_by` / `reconciles` edge | "this call was in service of that goal" |
| Herdr pane | Facet `session.meta.herdr` | "this call was typed in that PTY" |
| HedronDB desired_state | a future Agent CRD / Job | "the cluster should carry out this intent" |

Projection adapters are later work (§C16). Identity is never shared: a
pane is not a Pod; a desired_state is not a Deployment; a Lattice run is
not a Kubernetes Event.

---

## C4. Vocabulary

These collisions WILL happen in review. Use the table.

| Term | Means here | MUST NOT be used to mean |
|---|---|---|
| `h3s agent` | Parent spec §3 worker personality | A coding LLM, a Facet actor, a Herdr pane |
| **Herdr agent** | A process Herdr recognizes in a pane (Claude Code, Codex, Grok CLI, …) | The `h3s agent` binary |
| **Facet actor** | `FACET_ACTOR` string on a session (default `human`) | A Kubernetes ServiceAccount by itself |
| **agent workload** | A Pod + ServiceAccount that calls the API | A Herdr pane |
| Lattice | Facet's SQLite run history | HedronDB, etcd, Turso Cloud |
| vault | HedronDB isolation unit | Kubernetes Secret, Vault by HashiCorp |
| Warm | HedronDB `current_state` | Cluster cache warmth |
| Cool | HedronDB `causal_chain` | Idle Herdr pane |
| blocked | Herdr: agent waiting on a human | Pod `Blocked` (not a Pod phase) |
| working | Herdr: agent producing output | Deployment progressing |
| replica | Turso/libSQL embedded replica of a SQL file | etcd follower, ReplicaSet |
| engine | Lattice or sqlite-backend driver | Kubernetes scheduler engine |
| session | Facet session (ULID) or Herdr named session | Kubernetes Session (does not exist) |

When a document says "the agent," it MUST say which row.

---

## C5. Facet — action plane

### C5.1 What Facet is

Facet (`VirtualMachinist/facet`, v0.5.7 at time of writing) is
Hedronite's terminal-native fork of Probe.

- OpenCollection YAML collections on disk; Git is the sync layer.
- Same core as Probe (`probe-core` / `probe-cli`); `facet` and `probe`
  coexist on `PATH`.
- **Lattice**: SQLite run history (workspace `.facet/lattice.db` +
  machine store). Content-addressed blobs, `--sql`, gc.
- Ratatui TUI (`facet tui`) and agent CLI (`--json`).
- `facet mcp`: Model Context Protocol over **stdio** (JSON-RPC 2.0, one
  message per line). Same functions as the CLI. No HTTP transport in
  Facet v1.
- Sessions: `facet session start|end|list|show`. ULID. `FACET_SESSION`,
  `FACET_ACTOR`. `meta.herdr` when Herdr env is present.
- Secrets: declared `secret: true` variables; machine store; redaction
  before Lattice write. Secrets MUST NOT appear in `history --json`,
  `--sql`, session dumps, or `env list`.

License: Facet-original crates MIT (Copyright 2026 Hedronite); upstream
Probe Apache-2.0. Integration MUST keep that split visible in NOTICE.

### C5.2 What Facet is not

- Not an apiserver, kubelet, or Storage backend.
- Not HedronDB. `lattice-hedron` is an optional second data plane that
  *projects* into HedronDB; it is internal-only until pin alignment.
- Not Herdr. Facet records which pane a call came from; it does not own
  PTYs.
- Not kubectl. Humans still use kubectl. Agents SHOULD use Facet when
  they need a ledger, replay, `--expect`, or MCP.

### C5.3 Attach to h3s

Facet talks to `:6443` as any other HTTPS client. Authn is a kubeconfig
or a bound ServiceAccount token.

**SHOULD:**

- Ship `facet` next to `h3s` on operator boxes the way k3s environments
  ship kubectl. Separate binary, separate install.
- Keep cluster runbooks as OpenCollection YAML under a documented path.
  Recommended: `/var/lib/hedronetes/collections/` on servers that also
  host operator tools; a repo-local `./collections/` on workstations.
- Run `facet mcp` in a Herdr pane when agents should call the cluster.
- Stamp every recorded run with `FACET_ACTOR` and, when present,
  `session.meta.herdr`.

**MUST NOT:**

- Embed Facet in the `h3s` process.
- Parse Facet TUI output as an API.
- Store kubeconfig secrets in OpenCollection YAML.
- Treat Lattice as Kubernetes Events.

### C5.4 MCP tool surface (informative, Facet-owned)

At time of writing Facet MCP exposes tools over the same functions as
the CLI, including approximately:

`session_start`, `session_end`, `request_list`, `request_get`,
`request_run`, `history_list`, `history_get`, `blob_get`, `run_diff`,
`run_replay`, `sql_query`.

h3s does not version this list. Agents MUST treat Facet's own docs as
SoT. An h3s-specific MCP server is a non-goal; wrap Facet.

### C5.5 Lattice engines

| Feature | Engine | Default | Notes |
|---|---|---|---|
| *(none)* | bundled `rusqlite` | **Yes** | Two files: workspace + machine |
| `lattice-turso` | libSQL **local mode** | No | Same file format; no migration |
| `lattice-duckdb` | DuckDB ATTACH | No | Analytics |
| `lattice-hedron` | HedronDB | No | Second schema; see §C6 |

rusqlite and libsql MUST NOT be linked in one binary (Facet-verified
`sqlite3_config(SERIALIZED)` clash). An operator picks one Lattice
engine per Facet build.

Default for Hedronetes documentation: **rusqlite**. Document
`lattice-turso` as the path for later replica/sync of *agent history*,
not as the shipped default.

### C5.6 OpenCollection against a cluster

A collection that talks to h3s is ordinary OpenCollection YAML:

- environment `cluster` holds `H3S_URL` / `KUBECONFIG` references, not
  raw tokens.
- requests are Kubernetes HTTP (`GET /api/v1/namespaces`,
  `PATCH .../deployments/...`, …) or `kubectl` wrappers only when the
  collection is explicitly a shell collection.
- `--expect` encodes the assertion the agent cared about.
- Replay uses Lattice blobs; it does not re-apply mutations unless the
  operator says so.

v0.2 of this companion MAY add a stock collection set (`cluster-status`,
`drain-node`, `apply-manifest`). That set is not an h3s addon.

---

## C6. HedronDB — intent plane

### C6.1 What HedronDB is

HedronDB (`VirtualMachinist/hedrondb`) is a Phase 0 local-first
knowledge OS for AI agents. Product binary: `hedron`. Kernel crate:
`hedron-core`.

Storage: one SQLite file via **rusqlite only**, mode **0600**, four
tables:

| Table | Role |
|---|---|
| `nodes` | vault / agent / document identity + `extra.*` |
| `edges` | `caused_by`, `reconciles`, `supersedes`, `mentions` |
| `desired_states` | declarative spec + observed status |
| `events` | append-only causal log |

Vaults isolate named containers. Agent tokens live in process memory
and are rotated at the library gate. `hedron hql` is read-only and
does not take a token.

Two query paths, separate APIs, **MUST NOT be mixed**:

| Path | Name | Meaning |
|---|---|---|
| Warm | `current_state` | What is true now (latest desired/observed) |
| Cool | `causal_chain` | Supersession and causation |

HQL is a pipeline language (`vault`, `agent`, `state`, `history`,
`search`, `traverse`, `filter`, `select`, `limit`), not SQL. A Python
stdlib twin exists for result comparison. Schema drift is reported, not
migrated.

The README's own analogy is the one we keep: desired spec vs observed
status is **Kubernetes-like**. The implementation is not Kubernetes.

License on the VirtualMachinist mirror was unspecified at time of
writing. Integration MUST NOT depend on the crate in a shipped h3s
artifact until a LICENSE file exists. Prefer MIT or Apache-2.0.

### C6.2 What HedronDB is not

- Not `trait Storage`. No `resourceVersion`, no watch, no `410 Gone`,
  no `/registry/{resource}/{ns}/{name}`, no multi-server, no Tokio, no
  HTTP, no TCP.
- Not Lattice. Facet docs: Turso/Lattice *retrieves*; HedronDB
  *records/reconciles*. Different schema. `hedron-core` cannot read
  `lattice.db`.
- Not a recon CLI yet. Phase 0 surface is `hedron import` (markdown
  tree) and `hedron hql`.
- Not an in-cluster database service.

### C6.3 Attach to h3s

**SHOULD (after h3s v0.2, this companion v0.2+):**

- Treat a vault as one agent's (or one team's) intent world.
- Project selected cluster *status* fields into nodes as observed
  facts. Never project Secrets or token material.
- Store agent goals as `desired_states`. A later kube-rs controller MAY
  read those goals and write ordinary Kubernetes objects. The
  controller talks to `:6443`. HedronDB stores the `reconciles` edge.
- Keep Warm and Cool on separate call paths in any wrapper we write.

**MUST NOT:**

- Add `--store=hedron`.
- Serve HQL on `:6443`.
- Mix Warm and Cool in one API response.
- Put tokens or secrets in YAML frontmatter or `extra.*`.
- Infer `extra.domain` or `extra.name` from filesystem paths
  (HedronDB's own rule; we keep it).

### C6.4 Facet coexistence

Facet feature `lattice-hedron` is the existing design for a projection
from Lattice runs into HedronDB. It is **internal-only** until:

- rusqlite 0.40 pin alignment on `hedron-core`
- projection adapters exist

h3s MUST follow the same rule: projection adapters, not a SoT flip.
Read/search of API calls stays Lattice. Intent stays HedronDB.

### C6.5 Future Agent object (not v0.1)

An `Agent` CRD, if ever added to h3s, MUST:

- be an ordinary CRD behind `trait Storage` and RBAC
- refer to memory by URI (`hedron://vault/…` or a file path), not by
  embedding the vault
- never make the apiserver query HQL

That CRD is out of scope for parent spec v0.1 and for this companion
v0.1. It is listed so nobody invents it as a hidden sidecar of §C6.3.

---

## C7. Turso / libSQL — engine option

### C7.1 Name hygiene

Three different things share a brand. This spec names them apart.

| Name | What | Facet / h3s use |
|---|---|---|
| **libSQL** | Open-contribution fork of SQLite. Single-writer. Embedded replicas + `sqld`. Production. Crate: `libsql`. | Facet `lattice-turso` smoke is this, **local mode** |
| **Turso Database** | From-scratch Rust SQLite (`tursodatabase/turso`). Async, MVCC concurrent writes incoming, Postgres frontend experimental. Crate: `turso`. | Not what Facet smokes today |
| **Turso Cloud** | Hosted primary + replica product | **MUST NOT** be required for air-gap h3s or default Lattice |

Saying "TursoDB is the Lattice store" is false as a default and sloppy
as a category. The accurate sentence is:

> Lattice defaults to rusqlite. `lattice-turso` MAY open the same
> `lattice.db` through libSQL local mode.

### C7.2 What this engine is allowed to do

**Lattice (companion v0.1 note, v0.2 work):**

- `--lattice-engine=rusqlite|libsql` (Facet feature names win).
- Same on-disk format. No migration.
- Later: libSQL embedded replica or Turso Sync so *agent run history*
  follows an agent across machines. That is Lattice HA, not cluster HA.

**h3s-storage (parent spec §7.3, future only):**

- After the rusqlite Kine-shaped MVCC table exists and has tests,
  `--store=sqlite` MAY be implemented with libsql instead of rusqlite.
- Watch, RV, 409, compact, `410 Gone` remain **h3s code**. The engine
  is a file driver.
- `--store=turso` MUST NOT appear on the v0.1 flag list (parent §13.3).
- Multi-server SQLite remains forbidden (parent rule). Embedded replicas
  of a cluster-store file are not etcd quorum. Do not advertise them as
  HA.

**MUST NOT:**

- Put cluster objects, Lattice runs, and HedronDB nodes in one Turso
  database.
- Make Turso Cloud the default or the air-gap path.
- Claim libSQL single-writer is solved by branding.
- Claim Turso-rewrite MVCC is a Kubernetes watch bus.

### C7.3 Binary / link rule

rusqlite and libsql MUST NOT coexist in one shipped binary. Consequences:

- `h3s` default stays rusqlite/sqlx per parent §5.3.
- A Facet build that enables `lattice-turso` is a different artifact
  from the rusqlite Facet.
- HedronDB (`rusqlite` only) and a libsql Facet MUST remain separate
  processes. That is already the plane split.

---

## C8. Herdr — habitat plane

### C8.1 What Herdr is

Herdr (`herdrdev/herdr`, v0.9.0 at time of writing, ~36k stars) is a
Rust terminal workspace manager for coding agents.

- Apache-2.0 on current trunk (relicensed from AGPL mid-2026). Pin a
  post-relicense release in any documented recipe.
- Single binary, no Electron. Runs inside the terminal the operator
  already uses.
- Server owns PTYs. Client attaches / detaches. Layout restore after
  reboot; original processes do not survive a machine restart.
- Workspaces → tabs → panes. A pane is a real terminal.
- Detects agents (Claude Code, Codex, OpenCode, Grok CLI, Copilot CLI,
  Cursor Agent, …) and projects `idle` / `working` / `blocked` / `done`.
- Agents drive Herdr via CLI and a local NDJSON socket
  (`~/.config/herdr/herdr.sock`, named pipe on Windows): split, read,
  `agent.prompt`, `agent.wait`, event subscribe.
- Injects `HERDR_WORKSPACE_ID`, `HERDR_TAB_ID`, `HERDR_PANE_ID`.
- v0.9: `herdr machine add` — one client, many SSH machines.
- Plugins: `herdr-plugin.toml` + argv. The CLI is the plugin API.

Herdr is third-party (herdrdev, YC F26). Complementary lock, not a
Hedronite crate. Do not vendor.

### C8.2 What Herdr is not

- Not Kubernetes, CRI, or a kubelet.
- Not Facet, HedronDB, or a SQL engine.
- Not durable agent supervision across reboot (kubelet/CRI owns that
  for cluster workloads).
- Not a cluster API. The socket is local-trust (filesystem
  permissions). Multi-machine is SSH attach, not a Service.
- Not a scheduler. `blocked` means "waiting on a human," not
  `PodPending`.

### C8.3 Attach to h3s

**SHOULD:**

- Document Herdr as the default *operator habitat* for agentic use of
  h3s. A recipe, not a dependency.
- Recommended first layout (informative):

  | Pane | Runs |
  |---|---|
  | `cluster` | `export KUBECONFIG=/etc/hedronetes/h3s.yaml` + kubectl / k9s |
  | `facet` | `facet mcp` or `facet tui` |
  | `intent` | `hedron hql` against the operator vault |
  | `agent-*` | coding agents with `HERDR_ENV=1` |
  | `logs` | `h3s` journal / pod logs |

- Keep Facet's `session.meta.herdr` join. When `HERDR_*` is set, Facet
  MUST record it (already Facet behavior; h3s docs SHOULD mention it).
- Optional later: a Herdr plugin `h3s-status` that reads `:6443` and
  displays Ready nodes. Glue, not a plane.

**MUST NOT:**

- Link Herdr into the `h3s` binary.
- Expose `herdr.sock` as a Kubernetes Service or through the supervisor
  tunnel.
- Treat a Pod as a Herdr pane or a pane as a Pod.
- Run Herdr as a DaemonSet "agent runtime" that replaces the kubelet.
- Teach agents to call Herdr when `HERDR_ENV` is unset.

### C8.4 Where Herdr is allowed to run

| Location | Allowed | Notes |
|---|---|---|
| Operator laptop | **Yes** — default | Detach / reattach |
| Jump box / bastion next to `h3s server` | **Yes** — optional | v0.9 machines; lid-close story |
| Same host as `h3s agent` (edge) | **Yes** — optional | Node-local ops (drain, logs). Not required |
| Inside a Pod as the cluster control plane | **No** | Wrong object model |
| As the CRI runtime | **No** | |

---

## C9. Composition and join keys

### C9.1 The happy path

```
operator
  └─ herdr (habitat)
        ├─ pane: coding agent   HERDR_ENV=1
        │     ├─ facet mcp  ──HTTP+SA──►  h3s :6443
        │     │     └─ record ► lattice.db  (actor, session, meta.herdr)
        │     └─ hedron hql / later write ► vault.db
        ├─ pane: kubectl        same kubeconfig
        └─ pane: logs
```

A call that matters has four identifiers, when each plane is present:

| Key | Issued by | Carried on |
|---|---|---|
| Kubernetes uid / resourceVersion | h3s apiserver | object metadata |
| `FACET_SESSION` (ULID) | Facet | Lattice session + run rows |
| `FACET_ACTOR` | Facet / env | Lattice session |
| `meta.herdr.{workspace,tab,pane}` | Herdr env → Facet | Lattice session meta |
| HedronDB node id / state_version | HedronDB | desired_states, events |
| ServiceAccount name | h3s | request auth |

Correlation query (informative): "show Lattice runs for session S whose
`meta.herdr.pane` is P, joined to HedronDB events with `caused_by` S."
No single system executes that join today. Wrappers MAY. None of the
four stores is required to grow a foreign-key to the others.

### C9.2 Minimum viable glue (companion v0.1)

The only glue that already exists and MUST be preserved:

1. Facet records `meta.herdr` when Herdr env vars are set.
2. Facet talks to `:6443` with ordinary kubeconfig / bearer tokens.
3. Planes remain separate processes and separate files.

Everything else in §C9.1 is recommended shape, not shipped code.

### C9.3 Forbidden glue

- Herdr socket multiplexed onto `:6443`.
- Lattice table inside the h3s sqlite MVCC file.
- HedronDB vault opened by `h3s-storage`.
- A controller that `pane.send_text`s into Herdr as its reconcile
  action.
- An admission webhook that requires `FACET_SESSION`.

---

## C10. Identity and trust

### C10.1 Cluster identity (unchanged)

Parent spec §15 and Kubernetes RBAC remain the policy gate for anything
that hits `:6443`.

An agent workload that mutates the cluster MUST use a ServiceAccount
with least privilege. Facet does not bypass RBAC.

### C10.2 Actor vs account

| Identity | Scope | Authority |
|---|---|---|
| Kubernetes user / SA | API server | RBAC, admission, NodeRestriction |
| `FACET_ACTOR` | Lattice ledger | Attribution of a tool call |
| Herdr pane id | Habitat | Which PTY typed |
| HedronDB agent token | Vault mutations | In-process, not a cluster credential |

A Facet actor string is not a Kubernetes user. Docs and UIs MUST NOT
print `FACET_ACTOR=claude` as if it were `system:serviceaccount:…`.

### C10.3 Secrets

- Cluster secrets stay in Kubernetes Secrets / the parent spec's
  encryption-at-rest path.
- Facet secrets stay in the Facet machine store / keyring. Redact
  before Lattice write.
- HedronDB: no tokens in frontmatter; vault file mode 0600.
- Herdr socket: local filesystem trust. Do not copy `herdr.sock` into
  a container and call it multi-tenant.

### C10.4 Air-gap

Default companion story MUST work with:

- local rusqlite Lattice
- local HedronDB vault
- local Herdr server
- h3s `--store=sqlite` or `--store=etcd` on the operator's network

Turso Cloud, hosted Herdr, and any SaaS LLM are optional overlays.
They MUST NOT appear as required install steps in h3s quickstart.

---

## C11. Interfaces (normative summary)

| From → To | Transport | Auth | Notes |
|---|---|---|---|
| Facet → h3s | HTTPS `:6443` | kubeconfig / SA token | Kubernetes wire. Parent §6. |
| Agent → Facet | stdio MCP or CLI | env (`FACET_SESSION`, keyring) | No HTTP in Facet v1 |
| Agent / human → HedronDB | `hedron` CLI (HQL read; import write) | in-process tokens for writes | No TCP |
| Agent / plugin → Herdr | Unix socket NDJSON or `herdr` CLI | socket path permissions | Local only |
| Facet → HedronDB | future `lattice-hedron` projection | same process feature or sidecar | Beside, not instead |
| Herdr → h3s | none directly | — | Go through Facet or kubectl |
| h3s → Herdr | none | — | Forbidden as a controller action |

---

## C12. Deployment topologies

### C12.1 Workstation (default)

One human, one laptop, one local or remote cluster.

```
laptop:  herdr + facet + hedron + kubectl
network: HTTPS to h3s :6443
files:   ~/.facet/, HedronDB vault, ~/.config/herdr/
```

This is the documented agentic story.

### C12.2 Jump box

`h3s server` on a machine that also runs `herdr` server. Operator
attaches with Herdr 0.9 machines. Agents keep running when the laptop
closes. Facet and HedronDB MAY live on the jump box so Lattice and the
vault sit next to the work.

### C12.3 Edge node

`h3s agent` + optional Herdr server for node-local operators. Do not
make this the control-plane HA story.

### C12.4 In-cluster agent workloads

A Pod that is an agent:

- identity: ServiceAccount
- tools: Facet binary + MCP in the container, or a sidecar
- memory: HedronDB vault on a volume, mode 0600, or a projected URI
- habitat: Herdr is *not* in the Pod by default. The operator watches
  that Pod from a Herdr pane via `kubectl logs` / `exec`.

If someone later runs Herdr inside a privileged debug Pod, that is a
jump-box-in-a-pod, not a new plane.

---

## C13. Security baseline (companion)

In addition to parent §15:

1. Companion binaries run as the operator user on workstations. They
   are not setuid wrappers around `h3s`.
2. `facet mcp` inherits the environment's kubeconfig. Treat that
   kubeconfig like a password.
3. Do not pass cluster tokens as OpenCollection `--var` on a shared
   pane recording.
4. HedronDB vault files MUST be 0600. Backups follow the same mode.
5. Herdr socket MUST remain user-local. Multi-user machines use
   separate OS users or named Herdr sessions, not a shared sock.
6. No companion listens on `0.0.0.0:6443`.
7. Supply-chain: pin Herdr to a post-Apache-2.0 release; pin Facet and
   HedronDB by commit until they tag; do not `curl | sh` Herdr from
   h3s install script as a required step.

---

## C14. Packaging

| Artifact | Ships in `h3s` binary? | Install |
|---|---|---|
| `h3s` | yes | parent §13 |
| `facet` / `probe` | **no** | Facet repo / later Hedronite bundle |
| `hedron` | **no** | HedronDB repo |
| `herdr` | **no** | herdr.dev install |
| libsql / turso crates | **no** (not in default `h3s`) | Facet feature or future sqlite driver |

A future "Hedronite workstation pack" MAY wrap the four installs. That
pack is not Hedronetes. The `curl | sh` get.hedronetes.dev script
MUST NOT pull Herdr, Facet, or HedronDB as a hard dependency of
`h3s server`.

Docs MAY say: "for the agentic operator story, also install …"

---

## C15. Observability across planes

Do not build a unified observability product in v0.1 of this companion.

| Plane | Signal |
|---|---|
| h3s | parent §14 (`/readyz`, `/metrics`, tracing) |
| Facet | Lattice SQL, `facet history`, session list |
| HedronDB | HQL `state` / `history` |
| Herdr | sidebar states, socket events |

A later `h3s-status` Herdr plugin MAY read `/readyz` and print it.
Correlation remains the join keys in §C9.

---

## C16. Roadmap and definition of done

Parent spec §16 is unchanged. This section is the companion's own
ladder. **Nothing here is a gate on h3s v0.1.**

### Companion v0.1 — paper

- This document exists.
- Parent spec gains at most a pointer ("agentic overlay: see companion
  spec"). No new `h3s` flags.
- Facet `meta.herdr` behavior documented as the join key we keep.

### Companion v0.2 — workstation recipe

- Documented layout for Herdr + Facet + kubectl against a live h3s
  cluster.
- Stock OpenCollection directory for read-only cluster queries.
- `facet mcp` verified against h3s `:6443` with an SA token.
- HedronDB LICENSE present; vault used only for operator notes / goals.

### Companion v0.3 — projection

- `lattice-hedron` (or an h3s-world sidecar) projects Facet runs into
  HedronDB events without mixing Warm and Cool.
- Optional `--lattice-engine=libsql` documented for Lattice only.
- Optional Herdr plugin that shows node Ready.

### Companion v1.0 — overlay product

- Documented identity story (SA + actor + pane) with examples.
- Soak: one operator, three agents, one 3-node h3s cluster, Lattice
  retained for 7 days, vault mode 0600 in backup drill.
- Still four binaries. Still three stores. Still no `--store=hedron`.

### Explicitly later / never

| Item | Status |
|---|---|
| Agent CRD in h3s | After parent v0.3, if ever |
| Herdr-over-TCP cluster service | Never as an h3s API |
| Turso Cloud required | Never |
| Facet-in-apiserver | Never |
| HedronDB as etcd | Never |
| Pane == Pod | Never |

---

## C17. Ownership matrix

| Piece | Own / reuse | Notes |
|---|---|---|
| This document | Own (Hedronite / h3s) | Companion to parent spec |
| `h3s` binary, Storage, kubelet | Own | Parent spec |
| Facet crates | Reuse Hedronite | Do not merge into h3s workspace |
| Probe upstream | Reuse (Apache-2.0) | Cherry-pick via Facet, not via h3s |
| HedronDB / `hedron-core` | Reuse Hedronite | Separate repo; license first |
| Herdr | Reuse third-party | Recommend, integrate env, do not fork |
| libSQL | Reuse | Lattice engine / future sqlite driver |
| Turso rewrite | Watch | Not a v0.1 dependency |
| Turso Cloud | Reject as default | Air-gap charter |

---

## C18. Claims we will and will not make

### Will claim, when true

- h3s is a k3s-shaped Rust distro (parent spec).
- Facet is how agents record calls against that distro.
- HedronDB is how agents declare intent beside the cluster store.
- Herdr is a recommended habitat, independently useful.
- The four tools compose at env vars, files, and HTTPS.

### Will not claim

- "Hedronetes includes Herdr."
- "Turso is the h3s store."
- "HedronDB is etcd for Kubernetes."
- "Agentic Kubernetes" as an excuse to skip CRI, watch, or Sonobuoy.
- That Facet MCP is a cluster-grade multi-tenant API.
- That Herdr pane state is Pod phase.
- Any companion's star count as an h3s readiness metric.

---

## C19. Decision log (frozen by this companion spec)

1. Four companions, four jobs. Beside, not instead.
2. Three stores stay three stores.
3. Parent §§1–21 unchanged. No `--store=turso|hedron` on v0.1 flags.
4. Facet is the agent kubectl+ledger. kubectl remains the human client.
5. HedronDB Warm and Cool stay separate APIs.
6. Turso in Facet means libSQL local mode unless a later revision says
   otherwise in this same table.
7. Herdr is third-party habitat. Pin Apache-2.0 trunk. Do not vendor.
8. `h3s agent` ≠ Herdr agent ≠ Facet actor.
9. Join key we keep: Facet `session.meta.herdr`.
10. Air-gap default: no Cloud, no hosted mux, no required SaaS LLM.
11. Ideas and interfaces, not git subtrees — including these four.

---

## C20. Design provenance

| Source | Take | Drop |
|---|---|---|
| Parent `SPEC.md` | k3s product, Storage trait, CRI kubelet, stock kubectl | Any urge to reopen §7 for agents |
| Facet `docs/FACET.md` | Lattice, MCP, sessions, secret redaction, engine-beside rule, `meta.herdr` | Probe GPUI desktop as a cluster UI; `lattice-hedron` as default |
| HedronDB README | Four tables, Warm/Cool split, 0600, HQL, Kubernetes-like desired/observed as *analogy* | HTTP/TCP, SQL-as-HQL, schema auto-migrate, tokens in YAML |
| libSQL / Turso docs | Local engine, later replica of a SQL file | Cloud as default, MVCC-as-watch, one DB for all planes |
| Herdr docs v0.9 | Habitat, socket, agent states, `HERDR_*` env, Apache-2.0 | Pane-as-Pod, sock-as-Service, AGPL-era tags, vendor fork |
| k3s | Companion binaries live *next to* the distro (kubectl, Helm) | Embedding those binaries |

When a later companion is proposed (editor plugin, board, mobile
attach), add a row here rather than a fifth plane. If it does not fit
habitat / action / intent / engine-under-Lattice, it does not fit this
spec.

---

## C21. Document control

| Item | Value |
|---|---|
| Status | Draft |
| Parent | `SPEC.md` v0.1.0-draft |
| Filename | `HEDRONETES_COMPANION_SPEC.md` |
| Short name | h3s-cp |
| Changes to parent required for v0.1 | Optional one-line pointer from parent §16 "After v1" or a new §22 stub. Not required to implement h3s. |

Suggested parent stub, if added later:

> ## 22. Agentic companion plane
>
> Agents are a workload class and an operator story. They are specified
> in `HEDRONETES_COMPANION_SPEC.md`. They do not change §§1–21.

---

*End of companion specification.*