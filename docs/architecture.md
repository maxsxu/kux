# Kux architecture

## Goals

Kux's interaction loop must remain responsive when the API server is slow, a relist contains
hundreds of thousands of objects, or many watch events arrive in a burst. Memory must be bounded
by the active resource dataset rather than by queued events.

## Data path

```text
Kubernetes API
    │ discover + list/watch (active GVR + warm all-namespace Pods)
    ▼
watch task ── recovery + atomic relist
    │ compact to ResourceRow
    ▼
FxHashMap cache ── coalesce ── background sort
    │ Arc<Vec<ResourceRow>> via tokio::watch (latest value only)
    ▼
App state ── filter/sort indices + stable key selection
    │ visible range only
    ▼
Ratatui diff renderer
```

The terminal task never performs network I/O. Switching context, namespace, or resource type
aborts the old active-view watcher and starts a new generation. A separate all-namespace Pod watch
stays live so opening the common `pods(all)` view can reuse its current snapshot immediately.
Context changes replace both watches and create a new `kube::Client`; Kux authenticates that client
before making it visible to a resource view, and subsequent watches and actions share it. Background
discovery builds descriptors for all
listable core, CRD, and aggregated resources. Runtime views carry the exact group, version,
plural, scope, and supported operations rather than guessing from a Kind name. Every snapshot
carries its generation, so even a result already in flight cannot overwrite the current view.
Kubeconfig contexts are retained as typed Name/Cluster/User/Namespace metadata. Namespace
metadata results carry their source context, so a slower result from a previous client is ignored
instead of replacing the active context's Name/Status/Age list.

## Large-list behavior

The watcher handles `Init`, `InitApply`, and `InitDone` explicitly. A relist goes into a secondary
map and replaces the live cache only at `InitDone`. Normal apply/delete events update the active
map. Publication is rate-limited, and the `tokio::watch` channel retains only its newest value.

Before starting a built-in view's typed watch and enrichment traffic, Kux gives a gzip-compressed
Kubernetes Table request a bounded head start. For all-namespace Pods, the first 250 rows omit
embedded objects for the fastest possible paint; a paged typed Pod snapshot then restores namespace,
labels, timestamps, owner references, container defaults, resource requests/limits, ports, selectors,
and full status while the typed watch initializes in parallel. Both typed sources overlay complete
Pod rows without shrinking the visible bootstrap set. Larger Table and typed results continue in
5,000-row pages. Other built-in resources begin with metadata Table rows.

Kux requests Kubernetes streaming lists for those typed built-in watches by default. They avoid a
separate, potentially huge typed list response when supported by the server. `--legacy-list` uses
500-object pages for compatibility and bounded response memory. Dynamically discovered resources
use Kubernetes Table responses with metadata-only embedded objects and bounded pages, avoiding
transfers of large CRD specs and statuses just to render a list; full objects are fetched only for
on-demand actions such as YAML. Relists remain atomic. Resources that advertise list but not watch
use the same compact Table pages and initial progress snapshots with a five-second minimum polling
interval.

The all-namespace Pod cache starts when the source manager starts, uses the same progressive Table
bootstrap and typed watch, and remains current while another resource kind is active. Selecting
`pods(all)` retargets the cached snapshot to the new view generation synchronously, then continues
forwarding its live updates. The cache is memory-only, is replaced on context changes, and is
discarded when Kux exits.

Pod and node views run a separate five-second Metrics API task. Metric results contain only compact
CPU and memory numbers and are merged into display rows before publication. Node pod counts come
from an auxiliary incremental pod watch, so the node view does not repeatedly list every pod.
Every enrichment task is aborted when its resource view closes.

## Rendering behavior

Ratatui already diffs terminal buffers. Kux additionally avoids building table widgets for rows
outside the viewport. The frame timer is only a maximum rate: a frame is produced when the state
is dirty, plus a one-second heartbeat for age labels. Resize, input, or a new snapshot marks the
state dirty.

Pod tables size the name column for the longest loaded pod name and keep all columns available.
When the table is wider than the terminal, Left/Right scrolls its visible rows and column headers
horizontally while the border and section title stay fixed. Mouse column hitboxes follow the
horizontal offset. The scroll offset is clamped on resize and reset on resource navigation.

## Memory model

Each Kubernetes object is converted to an `Arc<ResourceRow>` and then dropped. Strings inside a
row use `Arc<str>`, and publishing a snapshot clones only one row pointer per object while an older
snapshot is still being rendered. The active source cache, warm all-namespace Pod cache, current
immutable snapshot, and filtered integer indices are the principal live allocations. Kux does not
persist resource snapshots to disk.

## On-demand actions

UI intents for YAML, decoded Secret data, describe, logs, container enumeration, authorization
policy and owner resolution, resource/context deletion, and kill use a separate bounded channel. Network and
subprocess work runs in the source manager rather than the render loop. Content replaces the table
inside the existing header/body layout and is retained only while that view is open. Initial YAML
fetches strip managed fields and serialize only the compact object on the blocking pool. The first
`m` fetches managed fields through a separate cancellable read and inserts only
`metadata.managedFields` into the displayed snapshot. Other YAML text, including encoded/decoded
Secret data, remains unchanged; later toggles are local and preserve scrolling and filtering.
Open YAML views also request a background GET every five seconds, with one request in flight and a
ten-second timeout. Refresh results preserve view state and include managed fields only after they
have been loaded. Generation, target, and YAML-session checks reject stale results after navigation
or reopening the same resource. Errors keep the displayed document and retry on the next interval.
Unwrapped content syntax-highlights and allocates only the visible terminal rows. Read
actions carry the view generation and are cancelled when resource navigation changes, so a late
YAML, log, container, policy, owner, or child-discovery result cannot replace a newer resource view. Describe invokes
`kubectl describe` with explicit
context, resource, and namespace arguments. Server version and namespace metadata refresh when
contexts change. A normal resource delete uses background propagation after a Tab + Enter
confirmation. The same
confirmation deletes only inactive, persisted contexts by running `kubectl config delete-context`
with explicit arguments and reloading kubeconfig metadata after success. Kill uses a zero-second
grace period without a confirmation, matching the intentionally sharp K9s binding. `--read-only`
prevents all three paths. YAML and resource deletion use `Api<DynamicObject>` with the discovered
API resource, so Secrets, CRDs, aggregated resources, namespaced resources, and cluster-scoped
resources follow the same path. Describe uses `plural.group` for non-core APIs to avoid ambiguous
names.

Pod logs have a dedicated cancellable streaming task and request ID in addition to the view
generation. Range changes replace the stream, and late batches cannot append to a newer log
session. The source emits bounded batches instead of waiting for the entire response. Tail and
time-range requests follow from up to 100 recent lines; head reads the first 5,000 bytes without
following, and previous-instance requests never follow. The display retains at most 5,000 lines
or 4 MiB. Pausing autoscroll freezes displayed arrivals while the backing buffer continues to
update; resuming appends retained pending lines and returns to the end. Column lock preserves the
horizontal offset on arrivals. Clear and separator marks are local operations.

Log requests always include Kubernetes timestamps, which the renderer can hide locally without
reopening the stream. Unwrapped logs style only the visible filtered rows after calculating and
clamping the scroll extent; wrapped logs measure rendered height before scrolling. The log header
reflows all log-specific bindings at narrow widths, and fullscreen logs remove borders and header
padding. Copy and save export the full displayed buffer using the current filter/timestamp settings
without terminal control sequences. Save is an explicit local action: it creates a unique `0600`
file under `$XDG_STATE_HOME/kux/screen-dumps/<context>/` (or
`~/.local/state/kux/screen-dumps/<context>/`). Logs are never persisted automatically.

Node drilldown starts a Pod watch with a server-side `spec.nodeName` field selector. Pod drilldown
fetches one Pod and compacts regular, init, and ephemeral containers into transient rows. The
parent Pod snapshot continues receiving coalesced watch updates and is restored locally on Esc;
container log requests, including previous logs, set `LogParams.container` explicitly. Pod-to-Node
navigation reuses the compact `spec.nodeName` and starts a Node watch with a server-side name
selector. The
workload drilldown contract stores the discovered child kind and compact Kubernetes label selector
on the parent row. Deployment watches open ReplicaSets, while ReplicaSet, StatefulSet, DaemonSet,
and selector-backed Service watches open Pods; the next watch applies that selector server-side.
Selecting a CRD resolves its `plural.group` name against API discovery and opens the preferred served
custom-resource version across its scope. For other resources without a predefined relationship,
Kux looks for dependents using lightweight `PartialObjectMetadata` lists, then opens the discovered
kind with an exact client-side controller-UID filter. If no dependent exists, Enter retains its
normal details behavior.

Secret list decoding fetches the selected core/v1 Secret on demand and builds a transient body from
its decoded `data` map only; the regular list cache never retains Secret values. Authorization
drilldown likewise builds a transient policy table. A ServiceAccount lookup pages RoleBindings and
ClusterRoleBindings, matches the exact namespaced subject, and resolves unique referenced Roles and
ClusterRoles with bounded concurrency. Direct Role, ClusterRole, RoleBinding, and
ClusterRoleBinding drilldowns reuse the same rule-to-verb-matrix projection.

## Planned extensions

1. Preserve server-provided discovery short names and singular aliases in addition to plural/Kind
   completion.
2. Implement attach and specialized workload actions.
3. Benchmark update throughput and resident memory at 100k, 500k, and 1M synthetic rows in CI.
4. Add Kubernetes impersonation-aware permission checks before presenting mutating actions.
