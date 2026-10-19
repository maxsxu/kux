# K9s interaction parity

Kux follows K9s's command grammar and key placement without importing K9s code. This matrix makes
the current boundary explicit: **implemented** performs the action, **partial** has named
compatibility or verification gaps, **interaction** recognizes the K9s key and reports that its
backend is pending, and **planned** is not yet exposed. These statuses do not establish complete
upstream compatibility; the [full inventory](k9s-full-parity.md) records the acceptance gates.

## Global and navigation

| Interaction | Key or command | Status |
| --- | --- | --- |
| Active help | `?` | Implemented |
| Resource aliases | `Ctrl-A` | Implemented for every discovered listable resource; chooser replaces the body |
| Quit | `:q`, `:quit`, `Ctrl-C` | Implemented |
| Previous/next view | `Esc`, `[`, `-`, `]` | Implemented with hidden view history |
| Resource command | `:pod`, `:secret`, `:plural.group` | Implemented; discovery includes CRDs and aggregated APIs |
| Aggregate workloads | `:workloads`, `:wk` | Implemented six-kind table, per-row GVR actions, wide VALID, Kind/Ready/Age sorts, marks/filter/export and exact instance Goto; shared live/scale acceptance remains open |
| RBAC Users and Groups | `:users`/`:usr`, `:groups`/`:grp`, `:can` | Implemented cluster-wide subject enumeration (`NAME`, `KIND`, `FIRST LOCATION`), `Enter` policy drilldown, and `:can u:<user>`, `:can g:<group>`, `:can s:[namespace/]sa` subject policy queries |
| CustomResourceDefinition command | `:crd`, `:crds` | Implemented as exact aliases for the CRD resource |
| Resource in namespace | `:pod ns-x`, `-n`, `-l`, `-f`, `@ctx` | Implemented positional or flagged namespace, label selector, filter, and context arguments |
| Regex, inverse, fuzzy, label filter | `/x`, `/!x`, `/-f x`, `/-l k=v` | Implemented |
| Highlight and navigate filter matches | `n`, `Shift-N` | Implemented across resource lists, choosers, and content; navigation wraps at both ends |
| Command/filter in in-place views | `:`, `/` | Implemented for resource, resource chooser, namespace chooser, context, details, YAML, describe, logs, container, reference, and policy bodies |
| Restore previous/clear filter input | `Tab`, `Ctrl-U` | Implemented with muted history hint |
| Cancel mode | `Esc` | Implemented |
| Context chooser/direct context | `:ctx`, `:ctx name` | Implemented; body columns are Name/Cluster/User/Namespace, `c` copies the selected name, switching rebuilds the client, and `Ctrl-D` confirms deletion of inactive persisted contexts |
| Namespace chooser/direct namespace | `:ns`, `:ns name` | Implemented; body columns are Name/Status/Age, `y` opens the selected Namespace YAML, and direct selection switches immediately |
| Recent namespace shortcuts | `0`…`9` | Implemented; `0` is always all namespaces and `1`…`9` are named recents |
| K9s-style header menu | automatic | Implemented as a lossless responsive header: cluster information is measured and wrapped first, the upright logo is shown only when it fits, and only complete namespace/action hotkey columns are admitted in priority order |
| Categorized help body | `?` | Implemented in place with Navigation, General, Resources, Display, and Mutation categories |
| Command completion | `Tab`, `Shift-Tab` | Implemented for discovered resource names and short names, built-in aliases, contexts, and namespaces |
| Refresh | `Ctrl-R` | Implemented |
| Custom command aliases | `aliases.yaml` | Implemented with global/context overrides, command expansion, cycle detection, and completion |
| Custom hotkeys | `hotkeys.yaml` | Implemented with global/context overrides, header/help entries, and protected input/dialog keys |
| Startup command | `--command`, `-c`, `defaultView` | Implemented; discovered resources resolve after discovery, explicit CLI resource/command overrides configured default |
| Settings reload | `config.yaml` | Implemented for readOnly, noExitOnCtrlC, defaultView, refreshRate, liveViewAutoRefresh, logger defaults, display preferences, and skin selection; the worker also reloads aliases, hotkeys, plugins, and skin files with last-valid retention |
| Explicit write override | `--write` | Implemented; takes precedence over `--readonly` and configured readOnly across reloads |
| Display launch preferences | `--headless`, `--logoless`/`--no-logo`, `--crumbsless`, `--splashless`, `--invert` | Implemented with header/logo/breadcrumb/splash control, `ui.noIcons`/`ui.reactive`/`ui.enableMouse`, and Oklch frame inversion |
| Refresh cadence | `--refresh`, `refreshRate` | Implemented fractional seconds with two-second minimum for Metrics API and list-only API; live YAML/Describe use a separate fixed five-second cadence when enabled, and streaming watch publication remains independent |
| Offline information/version | `info`, `version`, `version --short` | Implemented before Kubernetes or terminal initialization; reports paths or version/build fields |
| Shell completion | `completion SHELL` | Partial; static commands/flags generated, dynamic Kubernetes values not implemented |

## Connection and extensions

| Interaction | Configuration | Status |
| --- | --- | --- |
| Explicit connection | `--kubeconfig`, `--cluster`, `--user`, `--server` | Implemented through shared client/built-in kubectl connection options, retained across context changes |
| Impersonation and TLS/auth | `--as`, repeated `--as-group`, CA/client certificate/key/token, `--insecure-skip-tls-verify` | Implemented; explicit token redaction and fake-config/API/argv coverage; live-cluster acceptance remains |
| Request timeout | `--request-timeout` | Partial; Rust client applies separate connect/read/write timeouts, not a complete-operation deadline; kubectl receives the original value |
| Plugin files and scopes | `plugins.yaml`, `plugins/` | Implemented global/context/system/user loading, three YAML forms, live reload, aliases/GVR/all scopes, key overrides, and header/help entries |
| Plugin inputs and confirmation | `inputs`, `confirm` | Implemented up to five typed fields, validation/defaults, cancel, and expanded command preview |
| Plugin arguments and pipelines | `args`, `pipes` | Implemented direct argv expansion with selected identity, connection/input/column variables, including hidden columns and braced names |
| Plugin foreground/background execution | `background`, `overwriteOutput` | Partial; foreground handoff and bounded background capture/notices are fake-process tested, with offline PTY confirmation/cancellation/restoration checks. Direct pipeline children are managed; daemonized descendants, resize, and live-cluster compatibility remain open |
| Plugin read-only policy | `dangerous: true` | Implemented at visibility/dispatch/submission/execution; arbitrary command effects are not inferred |
| Named skins | `ui.skin` or `skin`, `skins/*.yaml`, `:skin` | Implemented global/context selection, interactive `:skin`/`:skins` chooser, named/hex/default colors, semantic UI groups, Oklch inversion, reload and last-valid retention |
| Custom columns and jumps | `views.yaml`, `jumps.yaml` | Implemented global/context `views.yaml` (column selection/order, `NAME:.jsonpath`, `W`/`H`/`N`/`R`/`L`/`T` flags, `sortColumn`) and `jumps.yaml` (`Enter` resource/namespace override with `$NAME`/`$NAMESPACE` templates) |

These additions have unit, renderer, mock API, fake subprocess, and specifically recorded offline
PTY evidence in the [full inventory](k9s-full-parity.md). Live-cluster acceptance remains open.
See the [README examples](../README.md#configuration-aliases-and-hotkeys)
for paths, precedence, and syntax.

## Table interaction

| Interaction | Key | Status |
| --- | --- | --- |
| Vim navigation | `h/j/k/l`, `g/G`, `Ctrl-B/F` | Implemented contextually across resource lists, choosers, text bodies, help, details, and confirmation actions |
| Horizontal arrow scrolling | Left/Right | Scrolls text bodies and Pod tables; never opens, closes, selects, or activates items |
| Mark, range mark, clear | `Space`, `Ctrl-Space`, `Ctrl-\` | Implemented |
| Faults, wide, header, logo | `Ctrl-Z/W/E/L` | Implemented; Ctrl-L is rollback in the ReplicaSet resource list |
| Select column | `Shift-Left`, `Shift-Right` | Implemented |
| Sort selected/name/age/namespace/status | `Shift-O/N/A/P/S` | Implemented |
| Select resource by mouse | Left-click a resource row | Implemented; changes the active row without changing marks |
| Sort by mouse | Left-click a column header | Implemented; repeated clicks reverse direction |
| Copy name/namespace | `c`, `n` | Implemented with native clipboard commands and OSC 52 fallback |
| Decode Secret data | `x` | Implemented for exact core/v1 Secrets; list rows open decoded data only, while full YAML toggles encoded/decoded values |
| Details/YAML/object description | `Enter`, `y`, `d` | Implemented in place; `d` runs qualified `kubectl describe`. Initial YAML/Describe/Secret-data reads show Loading, use per-open identity and a ten-second timeout, and reject late success/errors after cancel, reopen, edit, or forced refresh |
| Warp to namespace | `w` | Implemented in all-namespace views |
| Pod and node metrics columns | automatic | Implemented through `metrics.k8s.io` |
| Node → pods on node | `Enter` on Node | Implemented with server-side field selector |
| Workload → Pods | `Enter` on Deployment/ReplicaSet/StatefulSet/DaemonSet | Implemented with server-side label selectors; selector-backed Services also open Pods |
| CronJob → Jobs → Pods | `Enter` on CronJob/Job | Implemented asynchronous reads with request identity and cancellation; Jobs match owner-reference name in the selected namespace, and Pods match the Job selector; read-only navigation remains available |
| Deployment → ReplicaSets | `z` on Deployment | Implemented with selected namespace/selector and back history; missing compact selector metadata is fetched asynchronously with cancellation and request identity |
| Pod → containers | `Enter` on Pod | Implemented for regular, init, and ephemeral containers |
| Container → logs | `Enter` on container | Implemented with explicit container selection |
| APIService status table | `:apiservice` | Implemented with kubectl-compatible `NAME`, `SERVICE`, `AVAILABLE`, and `AGE` values from both progressive Table rows and watched objects |
| UsedBy references | `u` on ConfigMap/Secret/PVC/ServiceAccount/PriorityClass | Implemented asynchronous scans of five workload kinds, NAMESPACE/NAME/GVR identity, partial-error reporting and read-only access; disposable-cluster acceptance remains open |
| Reference navigation | Enter / `Esc` in UsedBy | Implemented GET-based single-instance Goto in the referenced namespace, with filter/sort/selection and prior namespace restored on return |
| Reference sorting/refresh | `Shift-N/P/V`, mouse, `Ctrl-R` | Implemented name/namespace/GVR sorting and configured periodic/manual refresh; marking, mutation and dump actions are disabled in this view |
| ServiceAccount effective rules | `Enter` on ServiceAccount | Implemented by matching RoleBindings and ClusterRoleBindings and resolving referenced roles |
| RBAC rule view | `Enter` on Role/ClusterRole/RoleBinding/ClusterRoleBinding | Implemented as a resource, API group, binding, and verb matrix |

## Content and mutations

| Interaction | Key | Status |
| --- | --- | --- |
| Pod/container logs/previous logs | `l`, `p` | Implemented explicit default/selected container, incremental streaming, severity colors, bounded buffers, and cancellation when leaving the view; request/read errors use up to 20 attempts with backoff and bounded timestamp replay suppression. Current and previous logs follow outside head mode and finish on clean EOF; Pod UID tracking remains absent |
| Merged Pod/workload logs | `l`, `p`, then `a` where a default is available | Implemented concurrent source streams for Pods, Deployments, StatefulSets, DaemonSets, Services and Jobs, structured Pod/container prefixes and stable colors; filters/copy/export retain identity. Membership resolves on open/reload; explicit container views cannot widen their scope |
| Log ranges | `0` tail, `1` head, `2` 1m, `3` 5m, `4` 15m, `5` 30m, `6` 1h | Implemented; tail/time ranges use configured tail count (100 by default) and follow, while head reads the first 5,000 bytes without following |
| Logger defaults | `logger` in config.yaml | Implemented tail, buffer, sinceSeconds, textWrap, disableAutoscroll, columnLock, and showTime; reload applies on next open, while range changes retain current controls |
| Clear/mark logs | `Shift-C`, `m` | Implemented; clear resets the local buffer and mark adds a colored separator |
| Copy/save logs | `c`, `Ctrl-S` | Implemented for the full displayed buffer, respecting filter/timestamps; saves a new `0600` file in the context's `kux/screen-dumps` state directory |
| Resource-table export | `Ctrl-S` | Implemented streamed CSV for every filtered/sorted row and current normal/wide columns beyond the viewport; proper quoting and terminal-control removal |
| YAML/text export | `Ctrl-S` | Implemented full displayed YAML variant despite search; describe/Secret-data/other text exports respect the display filter; chooser/overlay views (`:alias`, `:ns`, `:ctx`, `:pf`, `:sd`) export filtered CSV |
| Screen-dump browser | `:screendump`, `:sd` | Implemented context-scoped file list (`NAME`, `DIR`, `SIZE`, `AGE`), filename filtering, `Shift-N`/`Shift-A` sorting, `c` path copy, and refresh; default store includes legacy log exports |
| Screen-dump preview/editor | `v`/`l`, Enter/`e` | Implemented text preview with Escape back, or external editor using KUX_EDITOR/KUBE_EDITOR/EDITOR then vi; successful editor exit reloads the list |
| Screen-dump deletion | `Ctrl-D`, then Enter | Implemented selected-file confirmation, Escape cancel, stale-file checks, reload, and read-only rejection |
| Screen-dump destination | `--screen-dump-dir`, `screenDumpDir` | Implemented CLI and `config.yaml` root override with encoded cluster/context paths, private exclusive writes, 64 MiB limits, and failed-export cleanup |
| Log autoscroll/column lock | `s`, `Shift-L` | Implemented; pausing freezes displayed arrivals while buffering, resuming catches up, and column lock preserves horizontal position as messages arrive |
| Copy YAML/toggle managed fields | `c`, `m` in YAML | Implemented; copy uses the visible variant. Lazy managed-field reads are bounded and request-scoped; stale results after toggle/reopen/edit are ignored, and failures retain the current YAML for retry |
| Search YAML without hiding context | `/query`, then `n`/`Shift-N` in YAML | Implemented; keeps the full document, highlights matches, and scrolls between matching lines |
| Live YAML/Describe refresh | `r`, `liveViewAutoRefresh` | Implemented default-off/open-time configuration, immediate enable after loading, cancel on disable/leave, fixed five-second cadence, state-preserving updates, stale-result rejection, and jittered retries within a two-minute budget. During initial loading, `r` changes the future refresh flag without canceling that read; toggle off/on after exhaustion |
| Copy decoded Secret data | `c` in Secret data view | Implemented; copies the data-only body |
| Wrap/timestamps/fullscreen | `w`, `t`, `f` in content view | Implemented; log timestamps toggle locally, wrapped tails preserve the end, and fullscreen logs use the entire terminal without borders |
| Delete marked resources, current resource, or inactive context with Tab + Enter confirmation | `Ctrl-D` | Implemented; all marked resources are deleted as one bounded batch, while active/synthetic contexts and resources without delete capability remain protected |
| Kill without confirmation | `Ctrl-K` | Implemented with zero grace period |
| Pod shell | `s` | Implemented through interactive `kubectl exec` with the active context and namespace; the host terminal is cleared and starts with a Kux Pod/container identity banner, and Kux explicitly targets the valid `kubectl.kubernetes.io/default-container` annotation or the first `spec.containers` entry; unavailable in read-only mode |
| Pod/container attach | `a` | Implemented through `kubectl attach -it` with the active context, namespace, and resolved/selected container; restores the terminal after success or failure and is disabled in read-only mode |
| Edit any Kubernetes resource | `e` | Implemented from list, details, YAML, and Describe views through interactive `kubectl edit`; content editing preserves the open view and its toggles, cancels pending reads, and refreshes once after return. Uses exact context/namespace and is unavailable in read-only mode |
| Set container images | `i` | Partial: Pod/Deployment/StatefulSet/DaemonSet init and regular forms, explicit save/cancel, blank/unchanged no-ops, ordered patches with UID/resourceVersion checks, stale-form rejection and read-only gates. Pod owner references are rejected on load and rechecked with identity before save. Fake API and demo PTY coverage exist; full RBAC/live acceptance remains pending |
| ReplicaSet rollback | `Ctrl-L` on ReplicaSets | Implemented selected-revision preview, Cancel-first confirmation, full-scope visibility gate, owner/selector/UID/version checks, raw Pod-template/annotation restore, no-op and read-only gates; disposable-cluster rollout acceptance remains pending |
| Pod file transfer | `t` on Pods | Implemented upload/download form, editable remote/local paths, container, retries and no-preserve; background `kubectl cp`, cancellation, scoped completion and read-only gates. Disposable-cluster round-trip acceptance remains pending |
| Pod sanitization | `z` on Pods | Implemented namespace/all-namespace confirmation, exact `Yes Please!` phrase, full-scope visibility gate, pinned eligible statuses, paginated listing, sequential guarded zero-grace deletes and partial-failure counts; filters and marks do not narrow the operation. Disposable-cluster acceptance remains pending |
| Port-forward list/start | `f`, `:pf`, `Shift-F` | Implemented for Pods and Services with declared TCP-port suggestions, editable local port/address, bounded `kubectl port-forward` subprocesses, a context-wide full-body list, `b` to run/stop an HTTP benchmark on the selected forward, `Enter` to open the forward's benchmark results, and `Ctrl-D` stop; starting a forward is unavailable in read-only mode |
| Owner navigation | `Shift-J` | Implemented through controller owner references and a server-side name selector; rows without hydrated owner metadata, including Nodes owned by NodeClaims, resolve the selected object on demand |
| Pod Node navigation | `o` | Implemented through the compact Pod `spec.nodeName` and a server-side Node name selector |
| Service and port-forward HTTP benchmarks | `b`, `:benchmarks` (`:be`, `:bench`) | Implemented with global and per-context `benchmarks.yaml` (`defaults`, `services`, `containers`, `http.method`/`path`/`headers`/`body`/`auth`, concurrency `c`, requests `n`), background load generation with `b` run/stop toggle, report persistence in the context benchmarks directory, and `:benchmarks` viewer (`NAMESPACE`, `NAME`, `STATUS`, `TIME`, `REQ/S`, `2XX`, `4XX/5XX`, `ERR`, `REPORT`, `VALID`, `AGE`, `Enter`/`y` report view, `Ctrl-D` delete) |
| Cordon/uncordon and drain | `u`, `r` on Nodes | Cordon and uncordon use a guarded scheduling-state patch; drain includes editable grace period and timeout plus Ignore DaemonSets, Delete EmptyDir Data, Force, and Disable Eviction toggles; preflight safety checks run before cordon and evictions remain PDB-aware unless explicitly disabled |
| Workload restart | `r` on Deployments/StatefulSets/DaemonSets | Implemented with confirmation, multi-selection, bounded concurrent Pod-template patches, and partial-failure reporting; unavailable in read-only mode |
| Scale resources | `s` | Implemented with editable replica count, explicit confirmation, marks, `/scale` requests, and partial-failure reports; discovery includes CRD scale subresources |
| Trigger CronJobs | `t` | Implemented with confirmation and marked batches; creates Jobs from current templates |
| Suspend/resume CronJob | `s` | Implemented with confirmation; reads current suspension and uses a resourceVersion precondition |
| Explicit suspend/resume Jobs and CronJobs | `:suspend`, `:resume` | Implemented for marked/current resources with confirmation |
| Namespace use and Pod drilldown | `u`, `Enter` on Namespaces | Implemented active-namespace indicator `(*)`, `u` Use in `:ns` chooser and `v1/namespaces` table, and `Enter` on `v1/namespaces` to switch active namespace and open Pods |
| Context use and rename | `u`, `r` in `:ctx` | Implemented `u` Use to switch to the selected context and `r` Rename with editable dialog (`kubectl config rename-context`), active-context and history synchronization, and read-only protection |
| Node debug shell | `s` on Nodes | Implemented when `featureGates.nodeShell: true` is configured, launching an interactive privileged host-namespace Pod via `kubectl run --rm -it` using `shellPod` settings (`image`, `namespace`, `command`, `args`, `limits`, `hostPathVolume`); disabled in read-only mode |
| Custom views and jumps | `views.yaml`, `jumps.yaml` | Implemented global and per-context `views.yaml` (custom column order, `NAME:.jsonpath`, `W`/`H`/`N`/`R`/`L`/`T` flags, `sortColumn`) and `jumps.yaml` (`Enter` resource/namespace override with `$NAME`/`$NAMESPACE` templates) |
| Helm releases and history | `:helm` (`:hm`, `:chart`), `Enter`/`r`, `v`, `y`, `d`, `Ctrl-D` | Implemented Helm release list (`NAMESPACE`, `NAME`, `REVISION`, `STATUS`, `CHART`, `APP VERSION`, `VALID`, `AGE`), `v` Values (`x` toggles user-supplied vs `--all` default values), `y` manifest YAML, `d` release notes, `Ctrl-D` `helm uninstall`, and `Enter`/`r` revision history with `Enter`/`r` confirmed `helm rollback` |
| Local manifest directory | `:dir [PATH]` (`:ls`) | Implemented local directory and YAML/JSON manifest browser with `Enter` directory navigation, `y` YAML view, `e` external editor, `a` Apply (`kubectl apply` with automatic `-k` Kustomize and `-f -R` recursive detection), and `d`/`Ctrl-D` confirmed `kubectl delete` |
| Pulses cluster/namespace health dashboard | `:pulses` (`:pu`, `:hz`), `Tab`/`Shift-Tab`, `h`/`j`/`k`/`l`, `Enter` | Implemented 4-column health gauge grid (16 GVRs in all-namespace scope, 12 in namespaced scope) with 3×3 Unicode dot-matrix OK/Fault counters and trend deltas (`↑`/`↓`), plus CPU and Memory sparkline charts and `Enter` Goto drilldown |
| XRay dependency tree | `:xray <resource> [namespace]` (`:xr`), `x`, `Enter`, `y`, `d`, `l`, `p`, `s`, `a`, `e`, `Ctrl-D` | Implemented hierarchical dependency tree for `po`, `svc`, `deploy`, `sts`, `ds`, and `rs` (grouping under Namespaces with child Pods, Containers, Volumes, ConfigMaps, Secrets, PVCs, and ServiceAccounts), `x` expand/collapse, `Enter` Goto drilldown, `l`/`p` logs, `s`/`a` container/pod shell & attach, `y`/`d`/`e` YAML/Describe/Edit, and `Ctrl-D`/`Ctrl-K` delete/kill |
| Image vulnerability scans | `imageScans` in `config.yaml`, `VS` column, `v`, `Shift-V`, `Enter` | Implemented opt-in `imageScans` (`enable`, `exclusions.namespaces`, `exclusions.labels`), 6-bit severity bitmap `VS` column and `Shift-V` sort on Pods/Deployments/StatefulSets/DaemonSets/ReplicaSets/Jobs/CronJobs/Containers, `v` drilldown into the `Scans` table (`SEVERITY`, `VULNERABILITY`, `IMAGE`, `LIBRARY`, `VERSION`, `FIXED-IN`, `TYPE` with `Shift-S`/`Shift-V`/`Shift-L`/`Shift-F` sorting), and `Enter` CVE/GHSA browser link |
| Skin chooser | `:skin`, `:skins`, `Enter` | Implemented interactive skin table (`NAME`, `ORIGIN`) listing built-in `default` (`stock`) and discovered `.yaml`/`.yml` skins from config/data/system directories, with `Enter` applying the selected skin immediately |

Mutations are enabled by default for K9s-compatible operation. `--read-only` disables edit, Pod
shell, port-forward start, resource and context deletion, kill, workload restart, node scheduling
changes, drain, attach, image updates, rollback, file transfers, Pod sanitization, scaling, CronJob actions,
screen-dump deletion, and plugins declared
`dangerous: true`, unless `--write` explicitly overrides it. Local file export and screen-dump
editor access remain available in read-only mode.

The broader remaining scope and acceptance gates are tracked in [Full K9s feature parity](k9s-full-parity.md).
