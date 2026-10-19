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

## Connection identity

`src/connection.rs` owns `ConnectionOptions`, shared by CLI parsing, kubeconfig metadata loading,
client construction, source-manager context changes, and built-in kubectl argument construction.
The explicit kubeconfig file, cluster/user selection, server, impersonation user/groups, TLS
materials, bearer token, and request timeout therefore follow the same built-in action path.
Explicit kubeconfig/context/cluster/user errors do not silently select another credential source.
Context switching rebuilds the client with the original overrides. Built-in describe, edit,
exec/attach, port-forward, and context deletion pass the applicable connection arguments directly
to kubectl; no shell expansion is involved.

The Rust client maps request timeout to separate connect, read, and write timeouts. It is not an
end-to-end operation deadline, and zero disables these timeout overrides. Kubectl receives the
original timeout value and applies its own request semantics. Connection debug output redacts an
explicit token, and associated error/notice paths use the same redactor. Plugin processes receive
identity and kubeconfig environment values; arbitrary plugin commands remain responsible for their
own arguments and do not receive an automatically appended set of kubectl flags.

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
use the same compact Table pages and initial progress snapshots with a configurable polling
interval, defaulting to five seconds with a two-second minimum.

The all-namespace Pod cache starts when the source manager starts, uses the same progressive Table
bootstrap and typed watch, and remains current while another resource kind is active. Selecting
`pods(all)` retargets the cached snapshot to the new view generation synchronously, then continues
forwarding its live updates. The cache is memory-only, is replaced on context changes, and is
discarded when Kux exits.

Pod and node views run a separate Metrics API task at that same configurable cadence. Metric
results contain only compact CPU and memory numbers and are merged into display rows before publication. Node pod counts come
from an auxiliary incremental pod watch, so the node view does not repeatedly list every pod.
Every enrichment task is aborted when its resource view closes.

`src/source/refresh.rs` consumes interval changes through a latest-value watch channel. Metrics
and list-only API pollers adopt a new interval without restarting resource watches; missed ticks
are skipped. CLI `--refresh` overrides YAML `refreshRate`, including after configuration reload.
Live YAML/Describe polling has a separate fixed five-second interval matching the pinned reader
models. Event-driven watch updates and their publication coalescing rate remain independent of
these schedulers.

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
inside the existing header/body layout and is retained only while that view is open.

Initial YAML, Describe, and decoded Secret-data opens immediately show a Loading Content view.
`Content { id, target, content }` is answered by `OpenedContent { id, target, result }`, wrapping
the typed content or redacted error under the original identity. The source applies a ten-second
timeout. App acceptance requires the current opening-kind marker, request ID, exact target,
pending state, and Content overlay. Escape, another open (including the same target), editor
entry, or a forced refresh invalidates the old request, so queued success/error responses cannot
reopen or replace content. Open-time defaults are applied before dispatch; response application
preserves those defaults and display changes made while loading, including across config reload.
While the initial YAML/Describe read is pending, `r` only changes the future auto-refresh flag
and allows that read to complete. A matching failure clears Loading and pending/opening state
without closing the content view; successful retry clears the matching failure notice.

Initial YAML fetches strip managed fields and serialize only the compact object on the blocking
pool. The first
`m` fetches managed fields through a separate cancellable read and inserts only
`metadata.managedFields` into the displayed snapshot. Other YAML text, including encoded/decoded
Secret data, remains unchanged; later toggles are local and preserve scrolling and filtering.
Managed-field requests carry their own live-view request ID and exact target, including failures,
and have a ten-second timeout. Toggling refresh, reopening the same object, starting a newer
refresh, or entering an editor invalidates the old result. A matching failure clears that pending
read and keeps the YAML view and its prior body; `m` can retry. These response guards specifically
cover the initial content requests listed above, managed fields, and live YAML/Describe refresh;
they do not imply matching lifecycle coverage for every application action.
`src/app_live.rs` controls background YAML GETs and Describe subprocesses. Newly opened views
initialize from `liveViewAutoRefresh` (false by default); after the initial load, `r` enables an
immediate refresh and subsequent five-second polling, or cancels the pending refresh read and
disables polling. Reloaded settings
apply to the next opened view. There is one refresh in flight, with a ten-second operation timeout.
Refresh results preserve view state and include managed fields only after they have been loaded.
Generation, target, content kind, request ID, and pending-state checks reject stale refresh results after
navigation, toggling, or reopening the same resource. Failures keep the displayed document and
use jittered exponential backoff within a two-minute retry budget; success resets that budget.
Exhaustion stops requests until auto-refresh is toggled off/on. Leaving the view cancels pending
work. Interactive edit pauses reads before releasing the terminal, restores the same view, and
requests one refresh without changing its auto-refresh setting, even after an unsuccessful edit.
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

Pod and workload logs have a dedicated cancellable session and request ID in addition to the
view generation. `src/source/log_scope.rs` reads the exact parent, resolves valid default-container
annotations and regular/init/ephemeral names, and pages selector-matching Pods for Deployments,
StatefulSets, DaemonSets, Services and Jobs. Explicit container targets stay fixed. Resolution
metadata precedes lines and controls the available all-container toggle. Membership resolves once
per session; opening again, changing range or toggling containers resolves it anew. It does not
continuously discover replacement Pods during a rollout.

Every resolved source has a concurrently polled stream future owned by that session. One failed
source reports its identity without ending successful streams, and completion occurs after all
sources finish. Dropping the session cancels all reads and backpressure waits. Batches carry raw
text and optional `LogSource` identity; local markers have no source. The global channel is bounded,
and batch/replay budgets are divided among the source count, with at least one line per batch.
Partial-line buffers retain up to 64 KiB per stream, so total transport memory still grows with
the number of sources. A fixed number of long-lived follower slots would prevent later sources
from ever opening, so concurrency is not capped that way. Range changes replace the stream, and
late batches cannot append to a newer log session. The source emits bounded batches instead of
waiting for the entire response. Tail and
time-range requests follow from the configured tail count (100 by default); head reads the first
5,000 bytes per source without following. Current and previous-instance requests both follow
outside head mode. `LoggerSettings` supplies tail, initial sinceSeconds, line capacity, wrapping,
autoscroll, column lock, and timestamp
defaults when a log view opens. Tail and buffer settings are normalized to bounded ranges; both
the backing and displayed buffers honor the selected capacity, at most 5,000 lines or 4 MiB.
Changing a range preserves the active view's toggles and capacity. Configuration reload leaves an
open log session unchanged, and its next opening adopts the new defaults. Pausing autoscroll freezes
displayed arrivals while the backing buffer continues to
update; resuming appends retained pending lines and returns to the end. Column lock preserves the
horizontal offset on arrivals. Clear and separator marks are local operations.

`src/source/logs.rs` retries request/open/read errors for up to 20 total attempts, independently
of the global connection-health `maxConnRetry` setting (not implemented). The first delay is
500 ms; later nominal intervals grow by 1.5 with ±50% jitter and a 15-second nominal cap.
Clean EOF completes once. After an error, Pod status checks stop retries for deletion, termination,
or completed phases; other status-check failures retain the retry budget during cluster outages.
Dropping `LogTask` aborts HTTP, status checks, backoff, and channel waits. Every emitted batch retains
the original view generation and log request ID.

Resume requests use inclusive `sinceTime` and remove the initial tail/time-range constraints;
head requests retain their first-5,000-byte limit. The Kubernetes client's query encoder rounds
timestamps to whole seconds, so the local nanosecond cursor discards older replayed lines and
counts occurrences at the boundary to preserve distinct or repeated messages sharing a timestamp.
Incomplete lines from failed reads are discarded and fetched again. Boundary tracking divides
the session's 5,000-line/1-MiB payload budget among sources, with a separate bounded replay copy
of up to 1 MiB across the session. Missing timestamps,
observed clock regressions, or a full replay window prevent unsafe reconnects until timestamp
progress restores a usable cursor; interruption then reports an explicit reopen error. This is
stricter than the pinned upstream loop, which reuses its initial log options on internal reconnect.
Log targets do not yet bind the Pod UID, so
same-name replacements and undetectable timestamp regressions remain identity limitations.

`LogView` retains source identities alongside both the backing queue and displayed lines, keeping
them aligned through trimming, pause/resume and local marks. One formatter builds plain text for
filter/copy/export: optional padded timestamp, Pod for workload scopes, container unless the source
is explicitly single-container, then payload. `src/ui_logs.rs` applies stable source colors from
the Pod or container identity, without deriving identity or message severity from the prefix text.
Its eight-color hash follows pinned upstream, correcting the palette spelling `fushia` to fuchsia.

Log requests always include Kubernetes timestamps, which the renderer can hide locally without
reopening the stream. Unwrapped logs style only the visible filtered rows after calculating and
clamping the scroll extent; wrapped logs measure rendered height before scrolling. The log header
reflows all log-specific bindings at narrow widths, and fullscreen logs remove borders and header
padding. Copy and save export the full displayed buffer using the current filter/timestamp settings
without terminal control sequences. Log saves use the shared screen-dump worker and store described
below. Logs are never persisted automatically.

`src/app_images.rs` loads an image snapshot for an exact readable core/v1 Pod or apps/v1
Deployment, StatefulSet, or DaemonSet and presents init/regular fields through `src/ui_images.rs`.
The form retains its target and loaded revision when the backing table changes. Request IDs and
read-only/context transitions reject stale forms and results. Saving skips blank/unchanged fields;
`src/source/image_actions.rs` strategically patches only changed images, preserves container order,
and includes UID/resourceVersion preconditions. Conflicts are surfaced without automatic retries.
No-op edits send no write request. Any Pod owner reference blocks image editing, matching upstream;
Pod saves fetch the object again to recheck owners, UID and resourceVersion before the guarded
patch. Workload owners do not prevent template edits. Update completions carry the original view
generation and display transient notices without closing a newer form. Coverage includes
App/renderer fixtures, fake HTTP APIs, and isolated demo/fake-kubectl PTY flows; disposable-cluster
acceptance remains unverified.

ReplicaSet rollback is a request-identified modal in `src/app_rollback.rs` and
`src/ui_rollback.rs`; Ctrl-L overrides the logo toggle only in the exact apps/v1 ReplicaSet
resource list. Loading captures the selected row independently of marks, and the form defaults
to Cancel. The renderer requires the whole context/Deployment/revision notice to fit before
confirming. Read-only/context changes close the form; matching request IDs and generations gate
loads and transient completion notices.

`src/source/rollback.rs` reads raw Deployment and ReplicaSet JSON so unknown Pod-template fields
survive rollback. It requires an inactive ReplicaSet, positive revision, one controlling Deployment,
matching owner UID and Deployment selector, valid identities and an unpaused Deployment. Submission
refetches both objects and compares them with the reviewed snapshot before writing. A JSON Patch
tests the Deployment UID and resourceVersion, replaces its Pod template without `pod-template-hash`,
and restores applicable ReplicaSet annotations while preserving deployment-controller annotations.
It does not change replicas or unrelated spec fields. Equivalent templates are skipped. Preview
reads use ten-second deadlines; submission uses the shared call timeout. An ambiguous PATCH timeout
reports an unknown outcome and never triggers an automatic retry. Fake APIs exercise routing,
payloads, conflicts and failures; a real rollout remains a separate acceptance requirement.

Pod file transfer uses `src/app_transfers.rs` and `src/ui_transfers.rs` for the K9s-style
Download, From, To, No Preserve, Container and Retries form. Container loading is a cancellable,
request-scoped Pod read; the form's original target is retained through table updates. The
`src/transfer.rs` runtime captures context, generation and shared connection options at submission,
validates the local filesystem path on a blocking worker, and launches `kubectl cp` with literal
arguments. One transfer runs at a time. Separate readers drain stdout and stderr while retaining
at most 16 KiB from each; inherited pipes cannot keep reader tasks alive after the bounded drain.
Escape, context/generation changes, read-only reload and shutdown cancel the child. Completion
must match the current context, generation, request ID and open dialog before changing the UI.
The remote path remains editable, as upstream permits. Kubectl addresses its Pod by name; this
workflow does not provide an object-UID precondition or resume/transactional file semantics.

Pod sanitization uses a separate modal that captures context, generation and namespace before
requiring the exact `Yes Please!` phrase and an explicit OK. `src/ui_sanitize.rs` measures the full
scope/eligibility notice and disables confirmation when it cannot fit. `src/source/pod_actions.rs`
lists every page in the captured namespace or all namespaces before deleting anything; table
filters, server selectors and marks do not narrow the operation. Eligibility follows pinned K9s
computed Pod-status precedence, including Pending, init containers, restartable sidecars and
terminating states. Deletes are sequential with zero grace and listed UID/resourceVersion
preconditions; the first failure stops the operation and preserves partial counts. The operation
uses the shared call timeout (120 seconds by default), with a total budget five times that value;
`--request-timeout` overrides it and zero disables these deadlines. Success/error results retain
the original generation and use notices that preserve a newer form. Fake APIs and processes cover
the execution boundary; disposable-cluster acceptance remains separate.

Node drilldown starts a Pod watch with a server-side `spec.nodeName` field selector. Pod drilldown
fetches one Pod and compacts regular, init, and ephemeral containers into transient rows. The
parent Pod snapshot continues receiving coalesced watch updates and is restored locally on Esc;
container log requests, including previous logs, set `LogParams.container` explicitly. Pod-to-Node
navigation reuses the compact `spec.nodeName` and starts a Node watch with a server-side name
selector. The
workload drilldown contract stores the discovered child kind and compact Kubernetes label selector
on the parent row. Deployment Enter opens Pods and `z` opens ReplicaSets using that same selector;
ReplicaSet, StatefulSet, DaemonSet and selector-backed Service Enter opens Pods. Typed Deployment
rows, progressive Table metadata and demo rows share the Pod child default. `src/app_navigation.rs`
keeps context and the selected row's namespace, uses server-side labels and the existing history,
and refuses absent namespaces or unusable server selectors. Missing compact selector metadata
starts a request-identified ten-second Deployment GET in `src/source/deployment_navigation.rs`.
The current list remains available while it loads. Matching ID/target/mode/context/generation/view,
selection, overlay and input state gate the response; Escape or another interaction cancels it.
Validated matchLabels and matchExpressions become the new watch's server-side selector. As in
upstream, matching-label objects remain visible regardless of controller ownership.
CronJob and Job Enter use the same scoped asynchronous navigation state, with backend reads in
`src/source/batch_navigation.rs`. CronJobs open a namespace-local Job watch whose local predicate
matches any owner-reference name, preserving the pinned K9s behavior rather than imposing a
controller or UID requirement. Job selectors become server-side Pod label selectors; nil selectors
match nothing and explicitly empty selectors match every Pod in that namespace. Local owner-name
and match-none predicates stay out of API field selectors. Both routes preserve namespace/history,
clear the parent text filter for the child view, and work in read-only mode.

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

UsedBy uses a separate asynchronous read path in `src/source/references.rs`. It validates the exact
source GVR and scope, then concurrently pages Deployment, StatefulSet, DaemonSet, Job and CronJob
Pod templates. Namespaced sources stay in the source namespace; PriorityClass scans retain each
workload's namespace across the cluster. The scanner matches container environment and volume
references, Secret pull references and named ServiceAccounts, explicit/default ServiceAccounts,
PriorityClass names and StatefulSet generated claims. Projected ConfigMap/Secret volumes,
ServiceAccount image-pull secrets, and Job/CronJob PVCs extend the pinned scanner's match coverage.
Each request uses the configured call timeout. Bounded pages, continuation tokens, account cache
and diagnostics limit retained state; cancellation drops all five scanner futures. Per-kind and
per-object errors preserve successful matches and make the report explicitly partial.

`src/app_references.rs` accepts only the current request ID, target, generation, context and list
state. Reference rows store the exact destination descriptor and use GVR/namespace/name keys, so
same-named workloads remain distinct. The synthetic list keeps its original resource snapshot
separate, has only NAMESPACE/NAME/GVR columns, and refreshes at the configured cadence without
an overlapping scan. Enter starts an exact instance read for the selected GVR/namespace/name
and switches to that namespace, matching upstream UsedBy Goto. Escape restores the saved references, filter, sort
and selected identity before returning to the source list. The source faults-only setting is
ignored for reference rows and temporarily disabled in the destination resource list, then restored
on return. Reference list commands disable resource mutation, marking and the upstream-removed
display/export shortcuts. These scans are read-only;
no Kubernetes Secret payload is requested.

`src/source/resource_views.rs` serves exact instance views and the synthetic Workloads collection.
An instance target is carried in App/history/watch state separately from the active namespace and
uses GET for one named object. It never widens into a collection watch or a name field selector.
Both views use the configured refresh cadence and timeout, cancel on scope replacement, and
publish generation-scoped snapshots. An instance error clears its old row so a deleted or denied
object cannot remain actionable as stale data.

Workloads concurrently reads the six pinned resource kinds: Pods, Services, DaemonSets,
StatefulSets, Deployments and ReplicaSets. Each kind pages its server Table response; only a
complete six-kind result replaces the previous snapshot. `ResourceData::Workload` carries the
actual descriptor, while the GVR/namespace/name key prevents mixed-kind collisions. The App uses
that descriptor for YAML, Describe, Edit, marked deletion and Goto. The aggregate table exposes
KIND/NAMESPACE/NAME/STATUS/READY/AGE, with VALID only in wide mode; CSV and plugin column values
reuse that projection. Kind is initially ascending, and Kind/Ready/Age have dedicated shortcuts.
Readiness uses natural order, while ascending age shows youngest resources first.
Degraded rows use the pending color, while selection and mark visibility retain their usual styles.
Narrow tables can scroll horizontally to preserve full names and column access. Returning from
a concrete instance restores the aggregate filter, order, selected identity and marks.

## Configuration and extensions

Configuration I/O runs on a dedicated thread in `src/config_runtime.rs`. Context requests coalesce
into one pending slot, and `tokio::watch` retains only the latest effective configuration with its
cluster/context identity. `src/config_layers.rs` combines settings/aliases/hotkeys, the plugin
catalog, and the selected skin on this worker. A one-second poll reloads files without blocking
terminal input; unchanged effective results are suppressed, while explicit context requests are
acknowledged even when their values match an earlier scope.
The terminal rejects stale context updates. While the new context configuration is loading,
mutating commands remain unavailable. CLI read-only overrides YAML; explicit per-context
settings override global YAML settings. Explicit `--write` supersedes both CLI and configured
read-only settings. Invalid live edits retain the last valid file, and files
have bounded size and must be regular files. User aliases and hotkeys use the same command parser;
startup commands wait for discovery before resolving dynamic resources.

`src/plugins.rs` loads global and context plugin files plus sorted system/user plugin directories.
Each file retains its last valid definitions independently. Directory traversal, file size, form
size, and pipeline length are bounded. Scope and key resolution happen before dispatch; built-in
and hotkey bindings are retained unless a plugin explicitly overrides them. `src/app_plugins.rs`
captures the selected resource identity and form values, validates typed input, and constructs the
same expanded invocation that the confirmation displays. Plugins marked dangerous are excluded
in read-only mode and checked again at submission/execution. This policy depends on the plugin's
declaration; it cannot classify an arbitrary external program's effects.

The plugin environment includes resource, namespace, context, connection identity, Pod/container,
filter, and input values. Column variables reuse the actual table projection, including hidden
columns. Substitution changes argument contents without splitting them or recursively evaluating
shell syntax. `src/plugin_process.rs` spawns each pipeline stage directly, wires standard streams,
and reports failures from any stage. Foreground execution suspends and restores the terminal via
`src/plugin_commands.rs`. The background manager admits at most 16 pipelines and drains stdout and
stderr into bounded 64 KiB buffers with bounded drain time after exit. Completion polling is
nonblocking; `overwriteOutput` selects the first nonempty stdout line for a success notice.
Cancellation/drop manages directly spawned children; daemonized grandchildren are outside this
lifecycle guarantee. Foreground terminal restoration has not yet been established by a PTY test
for this plugin batch.

`src/theme.rs` parses named K9s-style skins into semantic component and state colors. Lookup checks
config, user data, and system data skin directories. Reads are bounded and reject nonregular files;
Unix opens are nonblocking to avoid a replaced FIFO blocking reload. The worker keeps the last
valid theme on errors. `src/ui.rs` applies body, component, then explicit state styles to existing
widgets, rather than globally substituting palette colors. Renderer tests cover table selection,
headers, prompts, help, dialogs, YAML, and logs. Breadcrumbs use their semantic skin group.
`src/theme_invert.rs` implements Oklch lightness inversion with pinned upstream color fixtures;
the renderer transforms the final frame, caching repeated colors within that frame and retaining
explicit terminal-default skin colors. The skin picker and rendering colors for future graph
views remain open.

## Screen dumps and exports

`src/app_dumps.rs` maps `Ctrl-S` to an immutable resource-table snapshot or the active text/log
variant. `src/ui_export.rs` captures shared row pointers, filtered/sorted indices, current
normal/wide columns, and the age timestamp. The filesystem worker formats CSV one row at a time,
including off-screen rows/columns and proper CSV quoting, without copying the resource dataset or
constructing the whole output string on the UI task. YAML preserves its complete document during
search; other text and logs use the displayed filter and log timestamp settings. Chooser exports
are not implemented.

`src/dump_commands.rs` owns a bounded request/result queue and runs filesystem work through
`spawn_blocking`. Requests carry the context, view generation, and browser request identity; stale
results cannot reopen an exited browser or replace a different context's content. The browser
lists names/sizes/ages, previews UTF-8 text, dispatches a selected file to the configured external
editor, and confirms deletion. The main dispatcher rechecks read-only before queuing deletion.
Successful editor exit and deletion reload the list. Local export and editor access remain
available in read-only mode, matching the current local-file policy.

`src/screen_dumps.rs` owns path resolution, listing, reading, deletion, and private exports. Its
default root is `$XDG_STATE_HOME/kux/screen-dumps` or `~/.local/state/kux/screen-dumps`, overridden
by `--screen-dump-dir`. New paths include encoded cluster/context components; the default-root
browser also reads legacy sanitized-context log directories. Unix operations use rustix
directory-relative handles with no-follow opens for context components and files. Selected entries
carry size, modification time, and Unix file identity so replacement requires a refresh. Listings
are bounded to 10,000 entries, reads/exports to 64 MiB, and streaming writes stop at the byte limit.
Files are created exclusively with mode `0600` on Unix; failures remove incomplete output.
Existing `log_view::save_logs` callers delegate to this store.

## Launch and offline commands

CLI preferences override reloaded YAML without resetting unrelated interactive toggles. Headless,
logoless, crumbsless, splashless, inversion, and content fullscreen preferences are applied to App
state. A brief startup splash uses the selected theme unless disabled. `src/cli_commands.rs` runs
`info`, `version [--short]`, and static shell completion before client/terminal initialization.
Info resolves paths without creating directories or reading kubeconfig. Dynamic Kubernetes
completion and diagnostic logging flags remain unimplemented.

## Workload actions

Scale discovery preserves the scale subresource in aggregated and legacy discovery. Confirmed
scale requests target `/scale`, allowing arbitrary discovered scalable resources and scale-to-zero.
CronJob trigger copies the Job template into a new generated-name Job with the CronJob owner UID.
Suspension toggles fetch current state and patch with its resourceVersion to reject concurrent
changes. Mutation batches run at most 16 requests concurrently and report partial failures.
Pod/container attach suspends the TUI around explicit-context `kubectl attach`, then restores it.

## Planned extensions

The [full K9s feature inventory](k9s-full-parity.md) tracks remaining implementations and verification,
including specialized resource actions/views, complete plugin/skin compatibility, custom columns
and jumps, Helm, Pulse, XRay, benchmarking, and broader persistence. The extension and connection
paths above are covered by unit/renderer/mock API/fake subprocess tests; live-cluster and plugin
PTY acceptance remains separate. Performance checks at 100k, 500k,
and 1M synthetic rows and permission-aware action visibility remain part of the acceptance work.
