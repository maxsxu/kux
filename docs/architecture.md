# Kux architecture

## Goals

Kux's interaction loop must remain responsive when the API server is slow, a relist contains
hundreds of thousands of objects, or many watch events arrive in a burst. Memory must be bounded
by the active resource dataset rather than by queued events.

## Data path

```text
Kubernetes API
    │ discover + list/watch (one active GVR)
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
aborts the old watcher and starts a new generation. Context changes create a new `kube::Client`;
subsequent watches and actions share that client. Background discovery builds descriptors for all
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

Kux requests Kubernetes streaming lists for its optimized built-in views by default. They avoid a
separate, potentially huge list response when supported by the server. `--legacy-list` uses
500-object pages for compatibility and bounded response memory. Dynamically discovered watchable
resources use paginated list-then-watch, since some aggregated API servers do not implement
streaming initial events. Resources that advertise list but not watch use paginated polling with a
five-second minimum interval.

Pod and node views run a separate five-second Metrics API task. Metric results contain only compact
CPU and memory numbers and are merged into display rows before publication. Node pod counts come
from an auxiliary incremental pod watch, so the node view does not repeatedly list every pod.
Every enrichment task is aborted when its resource view closes.

## Rendering behavior

Ratatui already diffs terminal buffers. Kux additionally avoids building table widgets for rows
outside the viewport. The frame timer is only a maximum rate: a frame is produced when the state
is dirty, plus a one-second heartbeat for age labels. Resize, input, or a new snapshot marks the
state dirty.

## Memory model

Each Kubernetes object is converted to an `Arc<ResourceRow>` and then dropped. Strings inside a
row use `Arc<str>`, and publishing a snapshot clones only one row pointer per object while an older
snapshot is still being rendered. The source cache, current immutable snapshot, and filtered
integer indices are the principal live allocations.

## On-demand actions

UI intents for YAML, decoded Secret data, describe, logs, container enumeration, authorization
policy resolution, resource/context deletion, and kill use a separate bounded channel. Network and
subprocess work runs in the source manager rather than the render loop. Content replaces the table
inside the existing header/body layout and is retained only while that view is open. YAML fetches
produce a compact object plus an optional managed-fields variant, allowing an instant local toggle
without another API request. Describe invokes `kubectl describe` with explicit context, resource,
and namespace arguments. Server version and namespace metadata refresh when contexts change. A
normal resource delete uses background propagation after a Tab + Enter confirmation. The same
confirmation deletes only inactive, persisted contexts by running `kubectl config delete-context`
with explicit arguments and reloading kubeconfig metadata after success. Kill uses a zero-second
grace period without a confirmation, matching the intentionally sharp K9s binding. `--read-only`
prevents all three paths. YAML and resource deletion use `Api<DynamicObject>` with the discovered
API resource, so Secrets, CRDs, aggregated resources, namespaced resources, and cluster-scoped
resources follow the same path. Describe uses `plural.group` for non-core APIs to avoid ambiguous
names.

Node drilldown starts a Pod watch with a server-side `spec.nodeName` field selector. Pod drilldown
fetches one Pod and compacts regular, init, and ephemeral containers into transient rows. The
parent Pod snapshot continues receiving coalesced watch updates and is restored locally on Esc;
container log requests set `LogParams.container` explicitly. The
workload drilldown contract stores the discovered child kind and compact Kubernetes label selector
on the parent row. Deployment watches open ReplicaSets, while ReplicaSet, StatefulSet, DaemonSet,
and selector-backed Service watches open Pods; the next watch applies that selector server-side.

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
