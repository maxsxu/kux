# Full K9s feature parity

The goal is to replicate all K9s features in Kux. This is the implementation and acceptance
inventory for that goal, not a claim that it is complete. The narrower
[interaction matrix](k9s-parity.md) remains useful for keys already exposed, but does not cover
configuration, integrations, specialized views, or the complete resource action surface.

## Baseline and evidence

The upstream GitHub latest-release API reported **K9s v0.51.0**, published
`2026-06-06T14:13:12Z`. The annotated tag resolves to commit
`558caafe7ba067467de46b320cc22ef11fef9c34`. The inventory was checked against that source tree,
including command registration, viewer registration, action bindings, renderers, configuration
types/schemas, and integration implementations. [Release][release] · [Pinned source][source]

This version is a reproducible measurement baseline, not a reduction of the objective. Before
declaring completion, check the current stable release again, inspect changes since this commit,
and add newly introduced features. Keep advertised or historical features with unresolved status
in the inventory until there is evidence resolving them.

The initial Kux source comparison began at `69e5d7dd4266c4f142e0e7a8eb1b7d861aab0c5d`, with
the first implementation batch covering attach, scaling, CronJob actions, aliases, hotkeys, startup
commands, and background configuration. A second batch adds scoped plugins, input forms/process
execution, named skins, and shared connection overrides. A third adds table/content exports and
the screen-dump browser, logger/refresh preferences, display/inversion controls, and offline CLI
commands. A fourth adds YAML/Describe refresh controls, log retry/reconnection, and container-image
forms. A fifth adds Pod file transfers and namespace-scoped sanitization. A sixth corrects
Deployment navigation and adds ReplicaSet revision rollback. A seventh adds UsedBy scans and
reference navigation. An eighth adds CronJob-to-Job and Job-to-Pod navigation. A ninth adds merged
Pod and workload logs with all-container controls and structured source identities. A tenth adds
aggregate Workloads and corrects UsedBy Goto to exact instance reads. Evidence for each is recorded
below; remaining behavioral, PTY, and disposable-cluster acceptance gaps stay open.

Status meanings:

- **Present**: a real Kux implementation and relevant existing tests were located. This is source
  evidence, not a new end-to-end verification or a claim of exact upstream compatibility.
- **Partial**: an implementation exists, but specific upstream behavior is absent or unverified.
- **In progress**: implementation is underway; it is not accepted yet.
- **Missing**: no reachable implementation was located in the audited Kux source.
- **Unresolved**: upstream documentation and executable source disagree, or coverage requires a
  focused comparison. This remains open work rather than silently disappearing from scope.

Every acceptance item also requires the shared verification gates at the end of this document.
“Present” rows still need those gates before the overall goal can be marked complete.

## Launch, connection, and configuration

Upstream evidence: [CLI flags and commands][cli], [configuration implementation][config],
[schemas][schemas], and [context configuration][context-config]. Kux evidence starts in
`src/cli.rs`, `src/connection.rs`, `src/main.rs`, `src/kubeconfig.rs`, and
`src/source/kubernetes.rs`. Configuration layers live in `src/config.rs`,
`src/config_runtime.rs`, and `src/config_layers.rs`.

| ID | Feature | Kux status and remaining requirement | Acceptance evidence |
| --- | --- | --- | --- |
| C01 | Context, namespace, all namespaces, read-only launch | Present: `--context`, `-n`, `-A`, `--read-only`/`--readonly`, and explicit `--write` precedence are implemented; write overrides CLI/config read-only across reloads. | CLI/App fixtures verify precedence and startup; continue the shared every-write-path/read-only acceptance gate as new actions land. |
| C02 | Startup command | Partial: `--command/-c` and configured `defaultView` accept command grammar, including discovered GVRs and aliases. `--resource/-r` retains its fixed built-in enum. K9s uses `-r` for refresh, so the existing short flag conflicts. | Start directly into discovered CRDs, contexts, namespaces, Helm, and special views with filters; preserve documented Kux flags without silently changing meaning. |
| C03 | Kubernetes connection overrides | Partial: explicit kubeconfig/cluster/user/server, impersonation/repeated groups, CA/client certificate/key/token, insecure TLS, and request-timeout options share client and built-in kubectl paths across context changes. Config/API/argv/redaction tests exist. Rust timeout applies per connect/read/write stage, not an end-to-end deadline. Real TLS/auth/impersonation acceptance and future Helm propagation remain. | `src/connection.rs` fixtures cover selection, override precedence, requests and redacted diagnostics; source/main/port-forward tests cover built-in argv. Complete HTTPS and disposable-cluster tests before claiming exact connection parity. |
| C04 | CLI display and logging options | Present: headless, logoless/no-logo, crumbsless, splashless, invert, fractional refresh, screen-dump-dir, `-l`/`--logLevel`, and `--logFile` are implemented. Kux intentionally retains `-r` for resource selection. | CLI parsing/precedence and App/renderer tests cover display/refresh and logging flags. |
| C05 | `info`, `version`, shell completion | Partial: offline info (listing Config, Aliases, Hotkeys, Plugins, Views, Jumps, Benchmarks, Skins, Context Configs, Screen Dumps, and Logs paths), version/short-version/build fields, and completion scripts for supported shells implemented before client/TUI initialization. Completion is static; dynamic kubeconfig context/cluster/user and namespace values remain optional. | `src/cli_commands.rs` and offline binary checks cover path output, no directory creation, all supported shell scripts, version fields, and output errors. |
| C06 | Global YAML configuration and precedence | Present: global/context files, Kux XDG/environment paths, `readOnly`, `noExitOnCtrlC`, `defaultView`, `refreshRate`, `liveViewAutoRefresh`, `screenDumpDir`, `portForwardAddress`, `maxConnRetry`, `disablePodCounting`, `thresholds`, `logger`, `ui`, `shellPod`, `imageScans`, `featureGates`, `skin` settings, last-valid retention, and background reload are implemented. | Temporary-directory fixtures cover missing/malformed/merged/reloaded files, normalized logger/refresh/threshold values, live-view next-open defaults and CLI overrides; context-tagged updates preserve the loading gate. |
| C07 | Per-context persistence | Partial: per-context settings, aliases, hotkeys, plugins, skin selection, `featureGates`, `shellPod`, and read-only overrides load from disk. Kux history, active view, recent namespaces, and display toggles remain in memory. | Restart twice with two contexts; each retains its own settings, override priority, and correctly sanitized paths. |
| C08 | Configurable refresh and resilience | Partial: coalesced watches/reconnects, request timeout, reloadable Metrics/list-only cadence, default-off `liveViewAutoRefresh` and `r` for YAML/Describe, bounded live-view retries, log retry/reconnection, and `maxConnRetry`/`disablePodCounting` configuration exist. | `src/source/refresh.rs`, `src/app_live_tests.rs`, and fake HTTP log tests cover cadence, toggle/cancel, stale results, retry bounds and recovery. |
| C09 | UI preferences | Present: header/logo/wide toggles, breadcrumbs, splash, content fullscreen defaults, `reactive`, `enableMouse`, `noIcons`, and Oklch inversion implemented with CLI/config precedence. | `src/app_preferences.rs`, `src/ui_preferences.rs`, renderer fixtures, and pinned inversion tests. |
| C10 | Logger preferences | Present: tail, buffer, sinceSeconds, textWrap, disableAutoscroll, columnLock and showTime defaults are parsed/normalized and applied to current/previous logs. Range changes retain active toggles; reopening applies updated configuration. | `src/config.rs`, `src/app_logger_tests.rs`, `src/log_view.rs`, and `src/source/logs.rs` test parsing, API params, bounds, pause/resume/marks, range changes and reload/reopen behavior; shared live/PTY gates remain. |
| C11 | Resource thresholds and GPU settings | Partial: configurable CPU/memory warning (`warn`, default `70`) and critical (`critical`, default `90`) percentage thresholds (`thresholds.cpu`, `thresholds.memory` in `config.yaml`) colorize `%CPU/R`, `%CPU/L`, `%CPU`, `%MEM/R`, `%MEM/L`, and `%MEM` cells. GPU vendor resource counts remain optional. | Unit and renderer tests verify threshold parsing, normalization, and warning/critical cell styling. |
| C12 | Runtime integration environment | Present: standard kubeconfig/kubectl editor behavior and Kux/K9s XDG/config/data/skins/logs (`K9S_LOGS_DIR`, `KUX_LOGS_DIR`) paths work; local dump editor resolves `KUX_EDITOR`, `KUBE_EDITOR`, `EDITOR`, then `vi`; port-forward address resolves `portForwardAddress` and `K9S_DEFAULT_PF_ADDRESS`; node-shell honors `featureGates.nodeShell`. | Unit tests verify each override and documented precedence. |

## Commands, navigation, and tables

Upstream evidence: [command parser][commands], [shared table behavior][table],
[browser behavior][browser], and [global application bindings][app]. Kux implements these mainly
in `src/app.rs`, `src/model.rs`, `src/ui.rs`, and `src/source/discovery.rs`.

| ID | Feature | Kux status and remaining requirement | Acceptance evidence |
| --- | --- | --- | --- |
| N01 | Discovered resource aliases and qualified resources | Present: singular/plural, built-in/server short names, group-qualified plural, CRD and aggregated API discovery. GVR command lookup and completion are implemented; alternate served-version and preferred-version behavior need comparison. | Fixtures with colliding short names, multiple served versions, core resources, CRDs, list-only APIs, and partial discovery failures select the exact intended GVR. |
| N02 | Resource command arguments | Partial: resource plus positional or `-n` namespace, `/filter` or `-f` filter, positional or `-l` inline server label selector, and `@context` work (including alias expansion). Full upstream directory-path arguments and edge-case comparisons remain. | Table-driven command fixtures compare resulting context, resource, namespace, selector, filter, history, and error behavior with upstream. |
| N03 | Command/filter history and completion | Present: completion, previous filter restoration, command history and hidden view history. Partial: fish-style history and back/forward command semantics versus nested view breadcrumbs require comparison. | Replay mixed commands, drilldowns, invalid commands, edited completions, `[`, `]`, `-`, and Escape without stale selection/filter. |
| N04 | Global commands and help | Present: `?`, `:help`, `:q`/`:quit`, `:alias`, `:cow [msg]` (centered red ASCII cow bubble), `:skin`/`:skins`, and all synthetic-view commands (`:pulses`, `:xray`, `:wk`, `:dir`, `:helm`, `:be`, `:scans`, `:pf`, `:sd`, `:usr`, `:grp`, `:can`) work and appear in command completion. | Command completion and execution tests verify every supported command reaches its view/action. |
| N05 | Filtering | Present: regex, inverse, fuzzy, and label modes. Partial: K9s uses regexp2 features; Kux uses Rust regex, and Kux label matching must be checked against Kubernetes selector semantics. | Cases include whitespace, anchors, lookaround/backreferences where upstream supports them, invalid patterns, `in`/`notin`, equality/inequality, existence/non-existence, and combined selectors. |
| N06 | Navigation, selection, and sorting | Present: vim keys, paging, ends, column selection, sort shortcuts, mouse row/column selection, marking/range/clear. Partial: exact view-specific sort defaults, typed values, stable selection through updates, and dynamic-column behavior need comparisons. | Input replays and row fixtures cover filtered/reordered/deleted selected and marked rows, duplicate names across namespaces, missing values, and numeric versus lexical sorting. |
| N07 | Table renderers and change feedback | Partial: typed Pod/Node/workload columns and server Table columns exist. Need all specialized column formats, wide flags, fault classification, colors/icons, field-change indicators and resource-specific sort shortcuts. | Compare each registered upstream renderer with fixture output, including state transitions; general API listability alone cannot satisfy this row. |
| N08 | Header, breadcrumbs, cluster summary | Present: context/cluster/user/server version, cluster CPU/MEM utilization percentages (`n/a` fallback), extension hotkeys, namespaces, responsive header and themed hideable breadcrumbs implemented. | Resize and context-change replays preserve readable metadata/actions and display unknown/unavailable metrics correctly. |
| N09 | Copy and export | Present: clipboard plus `Ctrl-S` log/YAML/text and full filtered/sorted resource-table CSV exports implemented; current normal/wide columns extend beyond the viewport. YAML preserves the full displayed document during search. Both CLI `--screen-dump-dir` and `config.yaml` `screenDumpDir` are supported. | `src/ui_export.rs` tests immutable ordered snapshots, off-screen rows, quoting and controls; `src/app_dumps.rs` tests YAML versus text filtering; filesystem tests prove exclusive private writes and streaming limits. |
| N10 | Screen dump browser | Present: `:screendump`/`:sd` list/filter/refresh (`NAME`, `DIR`, `SIZE`, `AGE`), `Shift-N`/`Shift-A` sorting, `c` path copy, `v`/`l` preview, `Enter`/`e` external editor, confirmed `Ctrl-D` deletion, stale results/files, read-only deletion gate, encoded cluster/context scope, and legacy default logs implemented. | `src/screen_dumps.rs`, `src/app_dumps.rs`, `src/dump_commands.rs`, `src/ui_screen_dumps.rs`; temp-directory, stale-selection/result, symlink, bounded I/O, private-file and failed-export tests. |
| N11 | Capability and RBAC-aware actions | Partial: discovery tracks watch/get/delete and Kux blocks read-only mutations. Upstream also performs access checks for operations. | Role-limited fixtures prove inaccessible actions are unavailable or explained, errors are surfaced, and action dispatch independently enforces read-only/capability constraints. |

## Resource views and actions

Upstream's authoritative inventory is the combination of [view registration][view-registry],
[model/render registration][model-registry], and the generic browser, not a fixed list of API
names. Kux's general discovery is an important foundation but does not implement each viewer's
special behavior. For every resource below, also apply N05–N11 and content/action gates.

| ID | Resource or feature | Kux status and remaining requirement | Upstream source and acceptance evidence |
| --- | --- | --- | --- |
| R01 | Pods and container drilldown | Present: typed Pod table, regular/init/ephemeral container rows, default-container resolution, node and owner jumps. Partial: complete container metrics/image/restart/status columns and upstream selection behavior need comparison. | `internal/view/{pod,container}.go`, `internal/render/{pod,container}.go`; fixtures cover sidecars, init, ephemeral, waiting, terminated, deleting, and unscheduled Pods. |
| R02 | Pod/container shell | Present: interactive kubectl exec with explicit identity and terminal restoration. Partial: configurable shell choice/defaults and terminated/unavailable containers need exact comparison. | `internal/view/pod.go`, `internal/view/exec.go`; fake subprocess and PTY tests validate argv, container choice, failure, resize, exit and restored input. |
| R03 | Pod/container attach (`a`) | Partial: implemented for chosen/default container, explicit context/namespace, and terminal restoration. Argument tests and fake-kubectl PTY smoke pass; real-container behavior and failure/resize cases remain to verify. | `internal/view/{pod,container}.go`; fake kubectl and PTY tests plus disposable-cluster attach test. |
| R04 | Pod transfer (`t`) | Partial: upload/download form, editable remote/local paths, regular/restartable-init/ephemeral container choice, default 999 retries, no-preserve, background kubectl execution, cancellation and read-only gates are implemented. Real Kubernetes transfer acceptance remains unverified. | `src/app_transfers{,_tests}.rs`, `src/ui_transfers.rs`, `src/transfer{,_tests}.rs`; 23 focused tests cover a successful fake-kubectl round trip with spaces and exact bytes, HTTP Pod loading/errors, literal connection argv, cancellation/reaping, scope and form identity. Compare disposable-Pod tar/exec behavior before completion. |
| R05 | Pod sanitize (`z`) | Partial: namespace/all-namespace confirmation, exact phrase, full-scope visibility gate, pinned computed-status eligibility, paginated listing, sequential guarded zero-grace deletion, timeout and partial-failure reports are implemented. This is separate from Popeye; disposable-cluster acceptance remains unverified. | `src/app_sanitize{,_tests}.rs`, `src/ui_sanitize.rs`, `src/source/pod_actions{,_tests}.rs`; source comparison and fixtures cover exact eligible states, scope, absence of selector narrowing, UID/resourceVersion preconditions and first-failure stop. |
| R06 | Set image (`i`) | Partial: Pod, Deployment, StatefulSet and DaemonSet regular/init forms, explicit save/cancel, blank/unchanged no-ops, ordered image-only patches, UID/resourceVersion guards, stale-form rejection and read-only gates implemented. Any Pod owner reference is rejected at load and rechecked with identity before save; workload owner references remain allowed. Full RBAC preflight (N11) and disposable-cluster acceptance remain. | `src/app_images{,_tests}.rs`, `src/ui_images.rs`, and twelve passing `src/source/image_actions_tests.rs` tests cover form state, real-client mock paths/payloads, owner restrictions and conflicts; isolated demo PTY covers editing/cancel/save/read-only. Compare `internal/view/image_extender.go` and Pod.SetImages through live acceptance. |
| R07 | Deployment/ReplicaSet navigation | Present: Deployment Enter opens Pods, `z` opens ReplicaSets and ReplicaSet Enter opens Pods, retaining selected namespace/selector and history. Missing compact Deployment selectors load asynchronously with cancellation and response identity. Shared live/API acceptance gates remain. | `src/app_navigation.rs`, `src/source/deployment_navigation.rs`, typed/progressive/demo fixtures and isolated PTY cover routes, parent server-selector history, cleared text filters, selected-versus-marked identity, missing metadata, loading/error/cancel/stale-response and read-only navigation. Like upstream, matching labels select children without an extra owner-UID filter. |
| R08 | Scaling (`s`) | Present: implemented for Deployment, StatefulSet, ReplicaSet, ReplicationController and CRDs with a scale subresource, single-target current desired-replica prefill from `READY` (`X/Y`), marked batches, scale-to-zero, and partial failures. Both discovery formats and real-client mock HTTP routing tested. | `internal/view/scale_extender.go`, `internal/dao/{scalable,registry}.go`; discover scale in both discovery formats, read current replicas, validate nonnegative counts, update `/scale`, honor marked resources and report partial failures. |
| R09 | Workload restart (`r`) | Present for Deployment, StatefulSet, DaemonSet with confirmation, marks and template patch. | `internal/view/restart_extender.go`; mock exact requests and isolated-cluster rollout evidence for all three kinds. |
| R10 | ReplicaSet rollback (`Ctrl-L`) | Partial: view-specific binding, selected-revision preview, Cancel-first/full-scope confirmation, inactive/owner/revision/selector checks, identity/version guards, raw template/annotation patch, no-op and read-only handling are implemented. Disposable-cluster rollout acceptance remains. | `src/app_rollback{,_tests}.rs`, `src/ui_rollback.rs`, `src/source/rollback{,_tests}.rs` and isolated demo PTY cover selected identity, scoped loads, patch requests, conflicts, cancellation, resize and read-only gates; compare `internal/view/rs.go` and `internal/dao/rs.go` on real Deployment rollout. |
| R11 | StatefulSet, DaemonSet and replication controllers | Partial: first two have typed rows and Pod drilldown; replication controllers use generic discovery. | `internal/view/{sts,ds}.go`, registries; fixture comparison covers readiness, images, selectors, metrics and all inherited actions. |
| R12 | CronJobs (`t`, `s`, Enter) | Present: confirmed batch trigger and current-object suspension toggle implemented with state-aware confirmation dialog (`Confirm Suspend` vs `Confirm Resume`); real-client mock HTTP tests cover template creation and resourceVersion conflicts. CronJob Enter asynchronously opens Jobs by owner-reference name in the selected namespace, matching pinned upstream. | `src/app_batch_navigation_tests.rs`, `src/source/batch_navigation{,_tests}.rs` and isolated demo PTY cover child navigation; compare `internal/view/cronjob.go`, `internal/dao/{cronjob,job}.go`. Trigger creates a Job from the template, preserving labels/annotations and proper owner, handles names and multiple selections; suspend reflects current object and confirms intended state. |
| R13 | Jobs | Partial: asynchronous Enter opens Pods through the current Job selector, including matchExpressions and nil-versus-empty selector semantics; current/previous workload logs are implemented; specialized state/age/completion rendering remains incomplete. | `src/app_batch_navigation_tests.rs`, `src/source/batch_navigation{,_tests}.rs`, publication fixtures and isolated demo PTY cover selected scope, selectors, empty results and navigation. Compare `internal/view/job.go`, `internal/render/job.go`; workload log fixtures cover parallel/completed/failed sources, while disposable-cluster acceptance remains open. Job suspend is not bound in v0.51.0 and is an optional extra, not proof of CronJob parity. |
| R14 | Nodes | Present: status, capacity/usage, Pod counts, Pod drilldown, guarded scheduling patch (`c` Cordon and `u` Uncordon/toggle) and drain (`r`) with options. | `internal/view/node.go`, `internal/render/node.go`; include already-cordoned, no metrics, taints, deleting Pods, PDB denial, timeout and partial drain failures. |
| R15 | Node shell (`s`) | Partial: feature-gated via `featureGates.nodeShell` and configured via `shellPod` (`image`, `namespace`, `command`, `args`, `limits`, `hostPathVolume`), launching a privileged ephemeral host-namespace Pod on the selected Node via `kubectl run` with read-only protection. Live disposable-cluster acceptance remains unverified. | `internal/view/node.go`, `internal/dao/node.go`, `internal/config/shell_pod.go`; unit tests verify `nodeShell` gate, Pod name sanitization, host-path mounts, privileged security context, and `kubectl run` override spec. |
| R16 | Services and endpoints | Partial: typed Services, Pod selector drilldown, port forwarding, and `b` HTTP benchmarking (for reachable `NodePort`/`LoadBalancer` Services) exist. Endpoint/EndpointSlice tables use generic server columns. | `internal/view/svc.go`, `internal/render/{svc,ep,eps}.go`; verify selectorless/ExternalName/headless, named ports, endpoint readiness/address formatting and namespace scope. |
| R17 | ConfigMap/Secret/PVC/ServiceAccount/PriorityClass UsedBy (`u`) | Present: asynchronous five-workload scans, full GVR/namespace/name identity, NAMESPACE/NAME/GVR table, partial-error reporting, sorting/filtering and preserved return state. Goto now opens a GET-based single-instance view, correcting the earlier namespace-list behavior. Shared live acceptance gates remain. | `src/app_references{,_tests}.rs`, `src/source/references{,_tests}.rs`, `src/source/resource_views.rs` and UI fixtures cover exact source scope, reference paths, pagination/cancellation, same-name identities, forbidden/unserved APIs, refresh, exact instance reads and history. Compare `internal/view/reference.go`, `command.go`, `internal/model/table.go`, `internal/render/reference.go`, `internal/dao/{reference,cluster,dp,sts,ds,job,cronjob}.go`. Owner traversal remains separate. |
| R18 | Secrets | Present: encoded YAML, decoded YAML toggle and decoded-data view without caching list values. Partial: specialized table parity remains; UsedBy is tracked in R17. | `internal/view/secret.go`, `internal/render/secret.go`; binary/non-UTF8/empty/malformed values, no data leakage into list cache/dumps without explicit export. |
| R19 | Namespaces | Present: chooser/direct switch, all namespaces, recent shortcuts, YAML/edit, `u` Use, active-namespace `(*)` indicator in both `:ns` chooser and `v1/namespaces` table, and `Enter` on `v1/namespaces` to use the selected namespace and open Pods. | `internal/view/ns.go`; switching updates the intended view/context without stale resource data and survives restart. |
| R20 | Contexts | Partial: list/switch (`Enter`/`u`), metadata, copy (`c`), rename (`r` via `kubectl config rename-context` with active-context and history synchronization), and guarded inactive-context delete (`Ctrl-D`) exist. Per-context view restoration and favorites remain. | `internal/view/context.go`, `internal/dao/context.go`; unit tests verify rename dialog, read-only/synthetic guards, active-context rename handling, and delete protection. |
| R21 | Events | Partial: discovered server table with `Type`/`Reason`/`Message` status/readiness/details projection, `Shift-S`/`Shift-R` sorting, and `Ctrl-Z` fault filtering (`Type != Normal`). | `internal/view/event.go`, `internal/render/ev.go`; repeated events, warnings, involved/regarding object identity, missing times and event API versions. |
| R22 | Storage and policy tables | Partial: dynamic PV/PVC/StorageClass/PDB/NetworkPolicy/HPA tables with `Phase`/`Claim`/`Volume` projection, `Shift-S` status sorting, and `Ctrl-Z` fault filtering (`Phase` not `Bound`/`Available`). | `internal/render/{pv,pvc,sc,pdb,np,hpa}.go`; golden fixtures cover phase, claim, capacity, access modes, binding/reclaim, disruptions, selectors and target values. |
| R23 | CRDs and all other listable resources | Present: generic Table/typed fallback, CRD→instances, qualified YAML/describe/edit/delete. Partial: custom views/jumps and scalable CRD subresources tracked separately. | `internal/view/crd.go`, generic browser/model; namespaced/cluster CRDs, multiple versions, printer columns, no Table support, list-only and unknown resource fixtures. |
| R24 | Aggregate workloads (`:workloads`, `:wk`) | Present: six-kind table, per-row exact GVR identity, KIND/READY/AGE sorting, wide VALID, filters/marks/export, mixed-target YAML/Describe/Edit/delete and GET-based instance Goto with parent-state restoration. Live/scale acceptance remains open. | `src/source/resource_views.rs`, `src/app_workloads_tests.rs`, `src/ui_workloads.rs` and portable PTY cover mixed kinds/namespaces, Table-derived status/readiness, atomic refresh, canceled reads, read-only actions and exact singleton navigation. Compare `internal/view/workload.go`, `internal/dao/workload.go`, `internal/render/workload.go`. |

## Logs, content, and port forwarding

Upstream evidence: [log viewer][logs], [log parameters][log-data], [log retries][log-retries],
[live content][live-view], [live-view configuration defaults][live-config], [YAML polling][yaml-model],
[Describe polling][describe-model], [port-forward support][ports], and
`internal/view/{pf,pf_dialog,pf_extender}.go`.
Kux evidence: `src/source/logs.rs`, `src/log_view.rs`, `src/port_forward.rs`, content handlers in
`src/app.rs` and `src/source/kubernetes.rs`.

| ID | Feature | Kux status and remaining requirement | Acceptance evidence |
| --- | --- | --- | --- |
| L01 | Single-container current/previous logs | Present: incremental logs, configurable bounded buffer/defaults, ranges, timestamps, clear/mark/pause/column-lock, fullscreen/wrap/filter/copy/save, and up to 20 attempts for request/read failures with upstream-style backoff. Reconnect uses a bounded timestamp replay window; unsafe cursors stop with an explicit error. Current and previous logs follow outside Head and finish on clean EOF; same-name Pod replacement is not UID-tracked. | Focused tests cover API parameters, chunk bounds, partial lines, real-client HTTP/chunked failure recovery, same-timestamp occurrence counts, terminal/missing Pods, retry exhaustion, cancellation during HTTP/backoff/backpressure, and replay bounds/clock regression. Live-cluster recovery remains unverified. |
| L02 | Multi-container and workload logs | Present: Pod all-container toggle `a`, concurrent streams, structured colored Pod/container prefixes, filter/export identity and Service/Deployment/StatefulSet/DaemonSet/Job routes. Pod membership resolves on open/range change/toggle; it is not discovered continuously during a rollout. Shared live and scale gates remain. | `src/source/log_scope{,_tests}.rs`, merged-session HTTP fixtures, App/display state tests and `src/ui_logs.rs` cover exact scope, annotations, regular/init/ephemeral sources, concurrent delivery, partial failure, previous logs, cancellation and bounded buffers. Re-resolve changed Pod membership on restart; exercise rolling replacements and real-cluster limits before closing shared gates. |
| L03 | Live YAML and describe | Present: `liveViewAutoRefresh` defaults false; after loading, `r` immediately enables or cancels refresh with a fixed five-second interval and bounded two-minute exponential retry budget. Initial Loading reads have per-open identity and timeout; `r` during loading changes the future flag without canceling that read. Search/scroll/wrap/fullscreen and YAML variants survive updates; edit preserves the view and performs a one-shot refresh. Lazy managed fields, copy/edit and Ctrl-S export remain available. | Config/App tests cover open-time settings, initial/managed/refresh request cancellation and stale success/errors, retained content on refresh failure, backoff/reset/exhaustion, and state-preserving edit refresh. Isolated demo/fake-kubectl PTY confirms toggle, editor success/failure, config reload and read-only flows; disposable-cluster acceptance remains pending. |
| L04 | Port-forward targets and lifecycle | Present: Pods, Containers (`Shift-F` / `f`), and Services with declared TCP port suggestions, configurable default `portForwardAddress` (`config.yaml` and `K9S_DEFAULT_PF_ADDRESS`), editable local address/port, list/stop, and child process lifecycle. | Mock process plus unit tests verify Pod/Container/Service target identity, address defaults, bind conflict, startup failure, and cleanup. |
| L05 | Fast-forward annotations and transport | Partial: `k9scli.io/port-forwards` (dialog default port mapping) and `k9scli.io/auto-port-forwards` (immediate port-forward activation without dialog) are implemented. | `internal/port/{ann,co_portspec,pf}.go`; unit tests verify container/named-port matching, dialog prefill, and auto-forward activation. |
| L06 | Port-forward view controls | Present: `f`/`:pf`, `Ctrl-D` stop, `b` benchmark run/stop on selected forward, `Shift-P` sort by port, `Shift-U` sort by URL/address, and `Enter` drilldown into filtered `:benchmarks` results. | Concurrent sessions, failed/exited sessions, selected deletion, sorting, and benchmarking do not affect another context/forward. |

## RBAC, health, graphs, and integrations

| ID | Feature | Kux status and remaining requirement | Upstream evidence and acceptance evidence |
| --- | --- | --- | --- |
| I01 | Role/ClusterRole/binding policy matrix | Present: Kux resolves role references and renders verbs, resource/group, binding source. Partial: exact matrix columns and non-resource URL/resourceNames/scope handling need comparison. | [RBAC DAO][rbac], `internal/render/{rbac,policy}.go`; wildcard verbs/resources/groups and namespace versus cluster scope fixtures. |
| I02 | Users, groups, ServiceAccount authorization | Present: direct ServiceAccount, User, and Group subject bindings, implicit ServiceAccount group grants (`system:serviceaccounts`, `system:serviceaccounts:<ns>`, `system:authenticated`), `:users`/`:usr` and `:groups`/`:grp` subject enumeration (`NAME`, `KIND`, `FIRST LOCATION`), `Enter` policy drilldown, and `:can u:NAME` / `:can g:NAME` / `:can s:[NAMESPACE/]NAME` query grammar are implemented. | `internal/dao/{rbac_subject,rbac_policy}.go`, `internal/view/{user,group,policy}.go`, `internal/view/cmd/types.go` and `command.go`; direct and group grants, same name in different namespaces, dangling refs and denied reads are distinguished. |
| I03 | XRay (`:xray RESOURCE [NAMESPACE]`) | Partial: `:xray <resource> [namespace]` (`:xr`) builds and renders the hierarchical dependency tree for `po`, `svc`, `deploy`, `sts`, `ds`, and `rs` (Namespace → Workload/Service → Pod → Container/Volume/ServiceAccount → ConfigMap/Secret/PVC with `ok`/`toast`/`noref`/`completed` health states), supporting `x` expand/collapse, `Enter` Goto drilldown, `l`/`p` logs, `s`/`a` shell/attach, `y`/`d`/`e` YAML/Describe/Edit, and `Ctrl-D`/`Ctrl-K` delete/kill. Live disposable-cluster acceptance remains open. | [XRay viewer][xray] and `internal/xray/`; unit and renderer tests verify tree hierarchy projection, expand/collapse state, shell/log/Goto routing, and tree glyph rendering. |
| I04 | Pulses (`:pulses`, `:pu`, `:hz`) | Partial: `:pulses` (`:pu`, `:hz`) renders a 4-column health gauge grid (16 resource GVRs in all-namespace scope, 12 in namespaced scope) with 3×3 Unicode dot-matrix OK/Fault counters and trend deltas (`↑`/`↓`), plus CPU and Memory time-series sparklines (`▁`–`█`), `Tab`/`Shift-Tab`/`h`/`j`/`k`/`l`/arrow focus navigation, and `Enter` Goto drilldown to the selected resource view (`Pods` for CPU/Memory). Live disposable-cluster acceptance remains open. | [Pulse viewer][pulse], `internal/model/{pulse,pulse_health}.go`, `internal/health/`, `internal/tchart/`; unit and renderer tests verify 16/12 GVR scope, dot-matrix gauges, sparklines, grid focus navigation, and `Enter` drilldown. |
| I05 | Helm release list, manifests, notes and values | Partial: `:helm` (`:hm`, `:charts`) lists releases across namespace/all-namespace scope with `NAMESPACE`, `NAME`, `REVISION`, `STATUS`, `CHART`, `APP VERSION`, wide `VALID`, and `AGE`, plus `v` Values (`x` toggles user-supplied vs `--all` default values), `y` manifest YAML, `d` release notes, and shared `ConnectionOptions::helm_args` propagation. Disposable-cluster acceptance remains open. | [Helm DAO][helm], `internal/view/{helm_chart,value_extender}.go`; unit and App/source tests verify column projection, values toggling, and command routing. |
| I06 | Helm history, rollback and uninstall | Partial: `Enter`/`r` on a Helm release opens revision history (`REVISION`, `STATUS`, `CHART`, `APP VERSION`, `DESCRIPTION`, wide `VALID`) with revision-scoped `v` Values, `y` manifest, `d` notes, confirmed `Enter`/`r` `helm rollback`, and `Ctrl-D` `helm uninstall` on releases with read-only gates. Disposable-cluster acceptance remains open. | `internal/view/helm_history.go`, `internal/dao/helm_history.go`; unit tests verify revision history rows, rollback confirmation/intent, and read-only enforcement. |
| I07 | HTTP benchmarks | Partial: `b` on reachable Services (`NodePort`/`LoadBalancer`) and PortForwards (`:pf`) starts or cancels background HTTP load generation configured via `benchmarks.yaml` (`defaults`, `services`, `containers`, `c`, `n`, `http.method`/`path`/`host`/`headers`/`body`/`auth`), persists reports under `<data_dir>/benchmarks/<cluster>-<context>/`, and `:benchmarks` (`:be`, `:bench`) displays `NAMESPACE`, `NAME`, `STATUS`, `TIME`, `REQ/S`, `2XX`, `4XX/5XX`, `ERR`, `REPORT`, wide `VALID`, and `AGE` with `Enter`/`y` report view and `d`/`Ctrl-D` deletion. | [Benchmark configuration][bench-config], `internal/perf/benchmark.go`, `internal/view/{svc,pf,benchmark}.go`; local HTTP server and App/parser tests verify exact request count, status/error aggregation, report persistence/parsing, and command routing. |
| I08 | Image vulnerability scans | Partial: opt-in `imageScans` (`enable`, `exclusions.namespaces`, `exclusions.labels`) in `config.yaml`, 6-bit severity bitmap `VS` column (`SEV-1`..`SEV-5`/`SEV-U`) and `Shift-V` sort on Pods/Deployments/StatefulSets/DaemonSets/ReplicaSets/Jobs/CronJobs/Containers, `v` drilldown into the `Scans` table (`SEVERITY`, `VULNERABILITY`, `IMAGE`, `LIBRARY`, `VERSION`, `FIXED-IN`, `TYPE` with `Shift-S`/`Shift-V`/`Shift-L`/`Shift-F` sorting and `Enter` NVD/GitHub Advisory browser link), and background `grype -o json` execution with deduplication and caching. | [Vulnerability scanner][vulnerability], `internal/view/{vul_extender,img_scan}.go`, `internal/config/scans.go`; unit and renderer tests cover config exclusions, deduplication, 6-bit score calculation, `VS` column injection, and `Scans` table sorting. |
| I09 | Local manifest directory (`:dir PATH`) | Partial: `:dir [PATH]` (`:ls`) browses local directories and `.yaml`/`.yml`/`.json` manifests (`📁`/`🦄`), supporting `Enter` directory drilldown, `y` YAML view, `e` external editor, `a` Apply (`kubectl apply` with automatic `-k` Kustomize and `-f -R` subdirectory detection), and `d`/`Ctrl-D` confirmed `kubectl delete` with read-only protection. Live-cluster acceptance remains open. | [Directory viewer][directory], `internal/dao/dir.go`; temporary manifest tree and App tests verify directory filtering, Kustomize/recursive flag selection, apply/delete/edit commands, and read-only gates. |
| I10 | Popeye reports | Unresolved/historical: README still advertises `:popeye`/`:pop` and spinach config, but v0.51.0 has no registered Popeye viewer/command. The v0.31.9 changelog says the bridge was removed. Preserve the requested broad feature in this inventory; resolve intended historical compatibility before an “all features” claim. | [Removal note][popeye-removal] and pinned README; if included, implement actual scan/report/drilldown/config behavior, not a notice or a generic shell shortcut. |

## Customization and extension compatibility

Upstream evidence: [configuration schemas][schemas], [aliases][aliases], [hotkeys][hotkeys],
[plugins][plugins], [custom views][views], [custom jumps][jumps], [skins][skins].
Kux extension evidence: `src/plugins.rs`, `src/app_plugins.rs`, `src/plugin_process.rs`,
`src/plugin_commands.rs`, `src/ui_plugins.rs`, `src/theme.rs`, and the configuration worker above.

| ID | Feature | Kux status and remaining requirement | Acceptance evidence |
| --- | --- | --- | --- |
| E01 | Custom aliases | Present: YAML aliases, global/context overrides, command expansion, cycle detection, precedence, GVR lookup, argument completion, and `:alias`/`Ctrl-A` chooser display and filtering are implemented and tested. | Upstream example fixtures and nested/cyclic/conflicting aliases; reload/context switch removes obsolete entries; alias target can contain namespace/filter/context. |
| E02 | Custom hotkeys | Partial: shortcut parsing, command dispatch, global/context override, input/dialog protection, reload, and header/help entries implemented. Complete collision/precedence comparison remains. | Modifier/function-key, collision, invalid-entry, input/dialog precedence, context override, reload, and visible-header/help tests. |
| E03 | Plugin loading and scoping | Present: global/context files, bounded sorted system/user directories, single/multi/full YAML forms, `all`/alias/GVR/view scopes (including `contexts`, `namespaces`, `containers`, `references`, `policy`, `portforwards`, `screendumps`, `aliases`, `workloads`, `helm`, `dir`, `benchmarks`, `pulses`, `xray`, `scans`, `skins`, `users`, `groups`), overrides, and header/help entries implemented. Last-valid file retention and context updates are tested. | `src/plugins.rs` loader/merge/scoping tests plus `src/app_plugins.rs` dispatch and `src/ui_plugins.rs`/`src/ui.rs` rendering tests. |
| E04 | Plugin execution | Partial: direct foreground/background argv pipelines, confirmation, dangerous/read-only checks, failures, and bounded background output implemented. Background overwriteOutput uses the first nonempty stdout line; foreground uses inherited terminal I/O. Direct spawned stages are managed, not daemonized descendants. Plugin PTY/resize/restoration and exact upstream output behavior remain unverified. | Fake subprocess tests exercise argv/env, piping, intermediate failures, capture bounds and cancellation; UI tests bind confirmed invocation to execution. Add PTY success/failure/resize and descendant-lifecycle compatibility evidence. |
| E05 | Plugin variable environment | Partial: resource GVR/name/namespace, context/cluster/user/groups, filter, kubeconfig, Pod/container, input and hidden-column values are wired; braced punctuation names and boolean inversion expand without implicit shell splitting. Context/namespace/synthetic-view environments and all upstream columns still need exact comparison. Arbitrary plugins must supply their own connection flags. | `src/app_plugins.rs` selection tests, `src/ui.rs::plugin_column_values`, connection environment and interpolation/fake-process tests; compare upstream environment fixtures across all viewers and missing values. |
| E06 | Plugin input forms | Present: up to five string/number/bool/dropdown fields with name/label/required/default/options, field navigation, cancel, validation, `$INPUT_<NAME>` substitution and expanded confirmation. Exact upstream interaction still requires the shared PTY gate. | Parser/validation tests in `src/plugins.rs`, interaction tests in `src/app_plugins.rs`, and rendered input/confirmation frames in `src/ui_plugins.rs`. |
| E07 | Custom views | Partial: global and per-context `views.yaml` (`views:` or `k9s.views:`) with per-GVR/alias/namespace keys, custom column selection/order, JSONPath extraction (`NAME:.path`), `W` wide, `H` hidden, `N`/`R`/`L` alignment, `T` time/age, and `sortColumn` (`COL:asc\|desc`) with hot-reload. | Upstream `views.yaml` fixtures across built-in and JSONPath columns produce configured headers, cells, wide/hidden visibility, plugin column variables, and default sort order. |
| E08 | Custom resource jumps | Partial: global and per-context `jumps.yaml` (`jumps:` or `k9s.jumps:`) mapping source GVR/alias to target `resource` and optional `namespace`, overriding `Enter` on configured resources with `$NAME`/`$NAMESPACE` template expansion and label selector propagation. | Unit tests cover config layering, template expansion, `Enter` override, and header hint updates. |
| E09 | Skins and theme reload | Present: named selection/lookup, interactive `:skin`/`:skins` chooser (`NAME`, `ORIGIN`, `Enter` to apply live), YAML groups/colors/anchors, component/state precedence, configurable `thresholds`, reload/retention and Oklch inversion implemented. Existing tables/header/help/prompt/dialog/YAML/log/breadcrumb/pulse/xray colors render. | `src/theme.rs` stock/Dracula parsing, `src/theme_invert.rs` exact pinned color fixtures, `:skin` chooser tests, actual renderer-cell assertions and config-runtime reload/context tests. |

## Exact resource coverage audit

Before final acceptance, enumerate the following upstream files rather than inferring completeness
from the six typed Kux resource kinds or successful Kubernetes discovery:

1. `internal/view/registrar.go`: all core/misc/apps/RBAC/batch/CRD/Helm viewer registrations.
2. `internal/model/registry.go`: every DAO, renderer and tree renderer, including generic fallback.
3. `internal/dao/registry.go`: synthetic resources, aliases, standard and scalable resource rules.
4. `internal/view/*go` and shared `internal/ui/*go`: every registered key, inherited extender and
   action availability rule. Constructors matter: an extender file existing does not prove it is
   reachable from every viewer.
5. `internal/config/json/schemas/*json` and corresponding types/loaders: every exposed configuration
   field and its priority, default, validation and reload behavior.
6. `cmd/root.go`, `cmd/info.go`, `cmd/version.go`: every flag, command, completion and environment
   integration, including the `-r` compatibility conflict.

Specialized Kubernetes rendering inventory includes Pod, Node, Container, Namespace, Context,
Deployment, ReplicaSet, StatefulSet, DaemonSet, Service, Endpoints, EndpointSlice, Event, ConfigMap,
Secret, ServiceAccount, Role, RoleBinding, ClusterRole, ClusterRoleBinding, PV, PVC, StorageClass,
NetworkPolicy, PDB, HPA, CronJob, Job and CRD. Some renderers are selected through the generic
registry rather than a dedicated viewer. Ingress and every other discovered API still require
generic browser acceptance. Synthetic resources include workloads, references, pulses, dirs,
xrays, aliases, contexts, screendumps, benchmarks, portforwards, containers, scans, Helm/history,
RBAC/policy/users/groups, help and quit. CPU/memory identifiers are internal Pulse chart identities,
not evidence of standalone top-level commands.

## First implementation batch: verification evidence

- `cargo test --all-targets`: 281 passed, 11 ignored. The ignored existing live-cluster tests
  do not count as passing acceptance evidence.
- `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, and `git diff --check`: clean.
- Workload tests exercise real `kube::Client` requests against a local mock HTTP server, including
  scale-to-zero, partial Forbidden failures, CronJob Job creation, and suspension conflicts.
- Discovery fixtures cover aggregated and legacy scale subresources, including cluster-scoped CRDs.
- Configuration fixtures cover file layering, invalid edits, nonregular/oversized files, shortcut
  parsing, alias cycles, argument completion, context changes, and bounded update delivery. A
  same-context/coalesced roundtrip regression proves the UI receives its configuration acknowledgment.
- PTY smoke with the built binary and temporary config passed: configured GVR startup, explicit
  command precedence, read-only scaling guard, noExitOnCtrlC with `:q`, custom hotkey/alias,
  scale-to-zero form, and attach via fake kubectl followed by restored TUI keyboard input.
- No live cluster mutations were performed. Disposable-cluster acceptance is still open; this
  batch does not establish full K9s feature parity.

## Second implementation batch: evidence and limits

This batch adds shared connection options, plugin loading/dispatch/forms/processes, and skin
loading/rendering. The coverage below includes **unit, renderer, mock API, fake subprocess, and
the specifically recorded offline PTY evidence**. Live-cluster acceptance remains open. Record
final integrated test totals after the complete verification run; intermediate subsystem passes
do not establish all-target acceptance.

- `src/connection.rs` tests cover explicit kubeconfig/context/cluster/user selection, server and
  authentication precedence, repeated impersonation groups, timeout parsing/configuration,
  token redaction, and real-client requests against a local fixture. Built-in source/main and
  port-forward tests check connection argument propagation. Real TLS handshakes/credential plugins
  and end-to-end operation deadlines remain outside this evidence.
- `src/plugins.rs` tests cover YAML forms, input schemas, interpolation, collision/scoping policy,
  file precedence, removal/reload, invalid-file retention, and bounded directory traversal.
  `src/app_plugins.rs` exercises selected identities, key precedence, dangerous-plugin read-only
  gates, input submission, cancellation, and confirmed invocation dispatch. Header/help and form
  frames are covered in `src/ui.rs` and `src/ui_plugins.rs`.
- `src/plugin_process.rs` uses fake processes for direct argv/environment, connected pipes,
  foreground exit status, background output bounds, failure and cancellation. The runner owns
  directly spawned pipeline stages; it does not guarantee cleanup of daemonized descendants.
  Offline PTY checks verified plugin forms, numeric inputs, confirmation, scoped identity variables,
  read-only safe/dangerous behavior, background output, named skin rendering, and foreground Ctrl-C
  cancellation with restored keyboard input. Fake kubectl PTY checks also verified shell, attach,
  and edit interruption and normal Ctrl-C exit after resume. Resize and live-cluster acceptance
  remain open.
  `src/plugin_commands.rs` tests first-line output notices and their bounded/sanitized presentation.
- `src/theme.rs` tests include upstream Dracula YAML with anchors, named/hex/default/indexed
  colors, group validation, lookup precedence, regular-file/size checks, and component styles.
  Renderer tests assert colors on real table/header/help/prompt/dialog/YAML/log cells and preserve
  selection visibility when a skin only overrides body/table colors. This batch did not establish
  inversion, graph rendering, picker behavior, or full upstream skin compatibility; the third batch
  adds inversion evidence below.
- `src/config_runtime.rs` exercises combined extension reloads, malformed/repaired files,
  context changes, unchanged acknowledgments, and latest-value delivery. Settings and extensions
  remain on the worker thread; the terminal receives a complete context-tagged configuration.
- Remaining work is tracked in C03/C06–C09 and E02–E09. Plugin support does not satisfy missing
  built-in workflows such as Helm, benchmarks, custom views/jumps, or multi-container logs.

## Third implementation batch: evidence and limits

The third batch adds exports/browser, logger defaults, refresh scheduling, offline commands, and
display controls. Integrated validation passes: `cargo test --all-targets --offline` reports
376 passed and 11 ignored live-cluster tests; Clippy with `-D warnings`, formatting, diff checks,
and the development build pass. Renderer tests were rerun after the final title-helper refactor.
The evidence below includes unit, renderer, filesystem, mock-process, and PTY coverage. No live
cluster mutations were performed, and full feature parity remains incomplete.

- `src/ui_export.rs` captures shared rows and current ordered indices, then streams all filtered
  rows as CSV on the filesystem worker. Tests cover off-screen rows, sort order, snapshot stability,
  quoting/newlines, and terminal-control removal. `src/app_dumps.rs` distinguishes full YAML export
  from filtered text, and tests browser selection, preview return, delete confirmation/read-only,
  and leaving before a delayed response.
- `src/screen_dumps.rs` tests scoped private/non-overwriting writes, encoded paths, legacy log
  visibility, stale/replaced selections, cross-context rejection, file/context symlinks, oversized
  and binary reads, streaming bounds, and removal of incomplete exports. `src/dump_commands.rs`
  runs those operations off the UI loop with bounded channels. Its four passing runtime tests
  cover stale context/generation completions, current-request/open-browser editor dispatch,
  read-only rejection before delete queueing, and full-queue backpressure with pending-state
  cleanup. The browser's NAME/SIZE/AGE layout intentionally differs from upstream NAME/DIR/VALID/AGE;
  exact sort/copy/export coverage for all synthetic screens remains incomplete.
- A normal-size PTY flow passed table export after filtering/sorting, dump preview, a fake editor
  with spaces in its path, cancelled and confirmed deletion, and read-only behavior. This checks
  terminal/process integration without using a live Kubernetes cluster.
- A PTY run exported all 100,000 demo rows into a 10,309,629-byte CSV while accepting navigation
  and filtering. The final-row navigation rendered in 58.8 ms while the file was still being
  written; the filter rendered in 2.184 s, and export completion was observed 2.6083 s after
  export started. These are observations from one local development-build run, not comparative
  performance guarantees. The run exposed and fixed the table's header-margin viewport error;
  a renderer regression now verifies the final row remains visible and clickable after resize.
  Streaming writes use a bounded buffer and preserve the original snapshot through UI changes.
- `src/source/kubernetes.rs` has a passing action-dispatch regression for older, newer, and absent
  active generations: all are rejected before using the current context's client. A matching
  generation reaches the fake API, which observes only the intended current scale request.
- Logger tests in `src/config.rs`, `src/source/logs.rs`, `src/log_view.rs`, and
  `src/app_logger_tests.rs` cover normalized defaults, generated API log parameters, line/byte
  bounds, paused/resumed output, range changes, marks, and next-open application of reloaded
  settings. At this stage log retry/reconnect and multi-container/workload aggregation were
  missing; retry/reconnection is added in the fourth batch below.
- `src/source/refresh.rs` tests live cadence changes without watcher restart and correct handling
  of a closed configuration channel. In this batch Metrics/list-only polling and YAML refresh
  used that cadence, and YAML interval changes rescheduled the due time. The fourth batch replaces
  this YAML behavior with upstream default-off live-view controls, separate fixed cadence,
  Describe refresh and bounded retries. Watch publication remains independently coalesced.
- `src/cli.rs`, `src/cli_commands.rs`, `src/app_preferences.rs`, and display tests cover static
  shell scripts, offline path/version output, fractional refresh parsing, persistent CLI override
  precedence including --write, header/logo/breadcrumb/splash visibility, and content fullscreen
  defaults. `src/theme_invert.rs` compares exact K9s v0.51.0 color fixtures; frame rendering applies
  inversion to final colors while preserving explicit terminal-default skin values.
- Remaining gaps include diagnostic logging flags, dynamic Kubernetes completion, YAML
  screenDumpDir, chooser exports, full screen-dump controls, the skin picker, and the other open
  inventory items. An implemented subsystem or green unit tests do not establish full K9s parity.

## Fourth implementation batch: evidence and limits

The fourth batch adds live YAML/Describe refresh controls, log retry/reconnection, and selected
resource image forms. This section records code, unit/renderer, fake-HTTP, and isolated PTY
coverage. The all-target test run reports 444 passed and 11 ignored live-cluster tests across two
suites. Clippy with `-D warnings`, formatting, diff checks, and the development build pass. The
final-binary PTY rerun also passes. Disposable-cluster acceptance remains unverified; earlier
batch results above remain historical evidence for those earlier trees.

- `src/config.rs` accepts global/context `liveViewAutoRefresh`, defaults it to false, and tests
  inheritance, explicit false overrides, invalid-layer retention, reload and removed overrides.
  `src/app_live{,_tests}.rs` tests next-open defaults, immediate `r` enable, cancellation on
  disable/leave, fixed five-second cadence, retained content on failure, exponential jitter,
  two-minute exhaustion/reset, and stale target/kind/session rejection. YAML and Describe share
  this state machine; metrics/list-only `refreshRate` no longer changes their cadence.
- Initial YAML/Describe/decoded Secret-data opens now use `Content { id, target, content }` and
  `OpenedContent { id, target, result }`. App immediately presents Loading and accepts a response
  only for the matching opening kind, request ID, target, pending state, and overlay. Eight initial
  request tests cover this lifecycle: seven in `src/app_initial_tests.rs` cover Escape,
  replacement/same-target reopen, stale success/error,
  wrong target/kind, defaults sampled before dispatch, loading-time changes and config reload,
  `r` preserving the first read, forced-refresh invalidation, and failure/retry notice cleanup.
  Initial source fetches have a ten-second timeout and retain request identity on redacted errors;
  the eighth test, the source wrapper regression, covers success and redacted-failure identity.
  Loading races are verified through unit/mock coverage; the normal-size PTY checks below do not
  simulate delayed API replies.
- Lazy managed-field reads now carry a request ID and target for both success and failure, with
  a ten-second timeout. `src/app_managed_tests.rs` exercises reopening the same target, refresh
  toggles, editor pause, replacement by a newer refresh, wrong kind/target, and preservation of
  the displayed YAML on failure. The fake HTTP error path preserves the tagged result instead of
  closing content through a generic notice. This evidence, together with the initial-request tests
  above, covers initial resource content, managed-field and live-refresh requests; it does not
  establish equivalent request guards for every application action.
- The content editor cancels pending reads before invoking `kubectl edit`, retains the open
  YAML/Describe view and toggles, then performs one refresh after return even when automatic
  refresh is disabled. App tests exercise successful/failed one-shot refresh and preserved state;
  the isolated PTY flow also checks successful/failed fake editor return to the same content view.
- Fourteen focused `src/source/logs.rs` tests passed. Local real-client HTTP/chunked fixtures
  verify open/transport/read-error recovery, original Pod/container/range parameters, current and
  previous streams, head byte-window preservation, partial-line refetch, same-timestamp duplicate
  counts, clean EOF, terminal/deleting/missing Pod stops, exhaustion, and cancellation while
  opening/reading/backing off. Existing backpressure/line-size tests remain passing. Retry timing
  matches the pinned 20-attempt policy, fixed first delay and jittered exponential nominal cap;
  `maxConnRetry` remains a separate, unimplemented cluster-health setting.
- Log resume deliberately improves on the pinned loop's reused initial parameters: inclusive
  `sinceTime` plus local nanosecond/occurrence filtering avoids a one-second skip and ordinary
  replay duplication. The replay window is bounded to 5,000 distinct lines/1 MiB plus its copy.
  Interrupted output with an unavailable or ambiguous timestamp cursor stops with an actionable
  error. Observed backward timestamps keep displaying but prevent unsafe resumption until time
  advances. Pod UID replacement is not detected, and unobserved clock regressions remain outside
  the chronological-stream assumption. Previous-instance requests intentionally do not follow.
- Twelve passing `src/source/image_actions_tests.rs` tests use fake HTTP APIs to verify all four exact routes,
  init/regular container snapshots, ordered image-only strategic patches, UID/resourceVersion
  checks, forbidden/conflict errors, and no request for blank/unchanged edits. App tests cover
  form identity through table updates, explicit Save/Cancel, stale loads, unsupported resources,
  and read-only/context transitions. Any Pod owner reference is rejected, regardless of its
  controller flag; a fresh Pod GET rechecks ownership, UID and resourceVersion before patching.
  Workload owner references do not block template edits. A narrow renderer fixture keeps the
  focused field and buttons visible. Update results are generation-scoped transient notices,
  preserving newer forms. These tests do not establish live Kubernetes image-update acceptance.
- The final-binary normal-size PTY rerun recorded in
  `/private/tmp/kux-live-image-smoke-r4novstz/report.json` passed with no failures using demo data
  and a fake kubectl. It verifies default-off YAML/Describe,
  `r` toggles, successful/failed editor return with the same view and toggle, configured defaults
  on new views, reload preserving the current toggle, init/regular image Save and Escape cancel,
  and read-only rejection of image/external edits while Describe refresh remains available.
  The captured editor argv has the explicit demo context and selected Pod namespace.
- Remaining broad gaps still include multi-container/workload logs, global connection-health
  retry configuration, custom views/jumps, Helm, XRay/Pulses, benchmarks, transfers, node shell,
  and the open inventory items. This batch does not establish full feature parity.

## Fifth implementation batch: evidence and limits

The fifth batch adds Pod file transfer and namespace-scoped Pod sanitization. The implementation
is connected to resource keys, help/header hints, the main dispatcher and scoped source results.
An interim all-target run reported 503 passed and 11 ignored live-cluster tests before subsequent
source-response and sixth-batch changes. This is historical evidence, not a separate final fifth-batch
acceptance claim; the final combined sixth-batch run is recorded below. Disposable-cluster
acceptance remains unverified.

- `src/app_transfers.rs` captures the selected Pod independently of marked rows. Its form defaults
  to download, prefills `namespace/pod:`, swaps From/To when direction changes, orders the valid
  annotated regular container first, and lists regular, restartable-init and ephemeral containers.
  Cancelled/replaced loads cannot reopen the form. Tests cover Unicode editing, input validation,
  explicit OK, no edits or duplicate submission while running, scoped completions and read-only
  changes. The narrow renderer keeps the focused cursor and buttons visible.
- Twenty-three focused transfer tests passed. `src/transfer_tests.rs` launches an isolated fake
  kubectl for upload and download, verifying exact round-trip bytes with local paths containing
  spaces. Additional fixtures cover literal argv/shared connection options, editable remote
  targets, local path validation, Pod GET/403, failed subprocess status and credential redaction,
  bounded output, cancellation/reaping, captured context/generation, context/read-only changes,
  and shutdown. Demo mode validates input but launches no process. These are process/API fixtures,
  not evidence that a real cluster's tar/exec transport or permissions work.
- `src/app_sanitize.rs` captures context/generation/namespace before requiring the exact
  `Yes Please!` phrase and explicit OK. Filters, server selectors and marks do not narrow the
  confirmed scope. Cancel, wrong phrase, readonly reload, context loading and stale generation
  reject dispatch. Ctrl-C follows the configured exit preference. `src/ui_sanitize.rs` measures
  its complete scope and eligibility notice and disables confirmation until it fits after resize;
  a small terminal cannot silently hide the destructive scope while leaving OK active.
- `src/source/pod_actions.rs` computes K9s-compatible status with init/sidecar/container/deletion
  precedence and selects exactly Completed, Pending, CrashLoopBackOff, Error, ImagePullBackOff,
  ContainerStatusUnknown, Evicted and OOMKilled. It completes paginated listing before deleting
  anything, uses the listed namespace/UID/resourceVersion for each zero-grace DELETE, and stops
  at the first failure without retries. The call timeout defaults to 120 seconds, with a total
  budget five times the call timeout; CLI overrides propagate and zero disables both deadlines.
  Eighteen focused backend tests pass, covering status precedence, selector-free pagination,
  namespace/identity checks, forbidden/conflict/list failures, first-error stop and partial counts.
  Timeout fixtures independently cover per-request and overall bounds, invalid/overflow values
  before any request, and explicitly disabled deadlines. Partial counts and errors remain scoped
  to the originating generation and preserve newer forms.
- The final-binary isolated demo PTY replay passed all seven checks in
  `/private/tmp/kux-pod-workflow-smoke-_yvaezaw/report.json`: sanitize wrong/exact confirmation,
  all-namespace scope under a filter, explicit namespace, upload direction/path swap with
  no-preserve/container/retries, download to a new local filename with spaces, Escape and read-only
  gates. A real resize from 140×24 to 80×10 prevented sanitize submission while its warning was
  clipped; resizing back revealed the full scope and enabled explicit confirmation. The harness
  installed a failing fake kubectl in PATH and verified that demo mode invoked no kubectl process.
- Remaining broad gaps include multi-container/workload logs, global connection-health retry
  configuration, custom views/jumps, Helm, XRay/Pulses, benchmarks, node shell, rollback and the
  other open inventory items. These additions do not establish full K9s feature parity.

## Sixth implementation batch: evidence and limits

The sixth batch adds Deployment/ReplicaSet navigation and guarded ReplicaSet revision rollback.
The final integrated run reports **552 passed and 11 ignored live-cluster tests** across all targets.
All-target Clippy with `-D warnings`, formatting, diff checks and the development build pass. The
final binary passes both the fifth-batch and sixth-batch PTY replays recorded below. This combines
all six batches; it is not a claim that the full goal or disposable-cluster acceptance is complete.

- `src/app_navigation.rs` routes Deployment Enter to Pods and `z` to ReplicaSets using the selected
  row's namespace and selector, independently of marks. Existing back/forward history preserves
  the parent context and server selectors; parent text filters clear on resource changes. Missing
  selection is a no-op and missing namespaces report a safe retry notice. Missing compact selector
  metadata now starts an asynchronous Deployment fetch while preserving the current list. Typed,
  progressive Table and demo Deployment rows retain the Pod child default; ReplicaSet Enter retains
  its own Pod selector. Twelve dedicated App tests passed within a 25-test navigation run after the
  fallback was added. The earlier Deployment-filtered run passed nine tests and left one live-cluster
  test ignored.
- `src/source/deployment_navigation.rs` validates the returned Deployment's exact GVK/name/namespace,
  rejects empty selectors, preserves matchLabels/matchExpressions through Kubernetes selector
  conversion, bounds each read to ten seconds and uses a real-client local HTTP fixture. App
  responses require matching request ID, target, desired child kind, view generation/context,
  selection, overlay and input state. Escape, new commands, navigation and repeated requests
  invalidate earlier responses; errors settle loading and leave the current list intact. Label
  selectors intentionally do not add an owner-UID filter, matching upstream navigation.
- `src/source/rollback_tests.rs` has sixteen passing focused fake-HTTP tests for the selected
  inactive ReplicaSet and controlling Deployment, positive revision, owner UID, selector and
  pause checks, guarded revalidation, exact JSON Patch, unknown Pod-template fields, annotation
  replacement/preservation, no-op equivalence, missing/forbidden/conflict responses and ambiguous
  timeout handling. Additional cases reject invalid timeout values before I/O and verify that a
  dropped PATCH response reports an unknown outcome without retrying. Raw unknown template fields,
  including empty values, are preserved in the patch. This verifies requests and local behavior,
  not a real controller rollout.
- The modal captures target/context/generation, defaults to Cancel, rejects stale loads and
  read-only submission, and requires the full revision/Deployment notice to fit before confirming.
  Completion notices preserve newer dialogs. Generic late Notice/Error completions now also retain
  active modals; source-side rejection produces matching request-identified failures for content,
  image, transfer, refresh and log reads so loading state can settle without closing a newer view.
  A saturated-result-queue regression confirms that a terminal rejection is delivered once queue
  capacity is released. A main-loop regression queues Resize then Enter without a redraw and
  confirms both rollback and sanitize remain blocked until their full scope is rendered again.
- The final-binary isolated demo PTY replay passed in
  `/private/tmp/kux-navigation-rollback-smoke-kdoosbi1/report.json`: Deployment Enter and `z`,
  ReplicaSet Enter, back history, cleared parent text filters, rollback preview, default Cancel,
  Escape, explicit confirmation and read-only behavior. At 80×8, rollback remained unavailable
  while its scope was clipped; resizing to 140×24 displayed the complete scope and permitted
  confirmation. The harness verified no kubectl process executed.
- Real Deployment rollout and shared live-cluster acceptance remain open. Broader gaps
  include multi-container/workload logs, global connection-health retry configuration, custom
  views/jumps, Helm, XRay/Pulses, benchmarks, node shell and the other inventory requirements.

## Implementation sequence

The sequence organizes delivery; no phase replaces the full objective.

1. Finish disposable-cluster/PTY acceptance for attach, scale, CronJob, aliases/hotkeys, shared
   connection options, plugins, skins, exports/browser, and new preferences. Continue capability
   metadata and configuration coverage.
   Remove pending-action notices only when an action works.
2. Close common workflow gaps: full command grammar/CLI, persistent context settings, remaining
   chooser export/screen-dump controls, UsedBy, workload/all-container logs, Deployment navigation,
   image-update, transfer and rollback acceptance, node shell and exact resource bindings.
3. Add configurable views and jumps, then close the remaining skin/plugin compatibility gaps with
   realistic fixtures. Extension support supplements built-in features; a user-written plugin is
   not acceptance for a missing built-in K9s workflow.
4. Aggregate Workloads is implemented in batch ten. Next implement RBAC users/groups and `:can`
   subject queries, then XRay and Pulses, Helm, benchmarks,
   image scans and local manifests. Resolve historical Popeye scope with the upstream evidence.
5. Complete every renderer/setting/shortcut comparison, run the end-to-end matrix, audit current
   upstream releases and close only requirements with direct verification evidence.

## Seventh implementation batch: evidence and limits

The seventh batch adds `u` UsedBy on exact core/v1 ConfigMaps, Secrets, PVCs and ServiceAccounts,
and scheduling.k8s.io/v1 PriorityClasses. It scans Deployment, StatefulSet, DaemonSet, Job and
CronJob Pod templates, matching the five pinned scanner kinds. It does not scan Pods or
ReplicaSets. Namespaced sources scan their namespace; PriorityClass users retain their actual
workload namespaces across the cluster. The workflow is available in read-only mode.

- Eighteen focused backend tests passed against local HTTP APIs and pure reference fixtures.
  They cover normal/init/ephemeral environment references, direct/projected volumes, pull secrets,
  explicit/default ServiceAccount names, PriorityClasses, direct PVCs and generated StatefulSet
  claims. They verify exact list routes and response identities, full GVR/namespace/name dedup,
  pagination, repeated tokens and page caps, oversized response truncation, partial permission and
  unserved-API failures, shared ServiceAccount lookup caching, request timeouts and cancellation
  without a later continuation request. Invalid source/timeout inputs cause no HTTP request.
- Deliberate correctness extensions include projected ConfigMap/Secret volumes, Job/CronJob PVCs,
  ServiceAccount image-pull secrets and namespace-local Secret references with omitted namespaces.
  Generated StatefulSet claim names require the complete template/workload prefix and a numeric
  ordinal. A denied ServiceAccount lookup cannot hide an already matched direct Secret volume.
  Secret scans retain the pinned explicit-ServiceAccount lookup policy; they do not add implicit
  default-account reads. No Secret payload is fetched.
- `src/app_references.rs` scopes initial and refresh responses by request ID, source, generation,
  context and current view. Empty complete scans retain the source list with a no-references
  notice; incomplete scans report partial results rather than claiming completeness. Refresh is
  periodic/manual and nonoverlapping. Typed source rejections terminate matching requests and
  redact credentials before diagnostic length limits, including long-token fixtures.
- References use exact NAMESPACE, NAME and GVR columns and GVR/namespace/name row keys. Same-named
  workloads in different kinds or namespaces remain distinct. `Shift-V` sorts GVR; filters,
  name/namespace sorting and mouse sorting reuse the table controls. Four focused renderer tests
  verify column order at 48/80/140 columns, no inherited server columns or marks, rendered identity,
  mouse sort reversal/filter counts, dedicated hints and breadcrumbs.
- Enter opens the selected resource kind's full namespace list and focuses the exact name as
  progressive rows arrive. Escape restores the references' filter, sort and selected identity,
  then the live-updated source list. Eleven focused App tests cover all five source kinds in
  read-only mode, duplicate identities, Goto/return, empty versus partial results, repeated/stale loads,
  cancellation after input/overlay/selection changes, nonoverlapping refresh and updated parent
  snapshots. A source faults-only filter does not hide synthetic references or healthy destination
  workloads and is restored when returning. Filtered Escape and the `h` back shortcut cancel an
  in-flight refresh so a late result cannot reopen the closed list. Mutation, marking and removed display/export keys are suppressed.
- The isolated demo PTY replay is saved in [scripts/smoke_usedby.py](../scripts/smoke_usedby.py).
  Run `rtk proxy python3 scripts/smoke_usedby.py` after a development build. The portable replay
  passed all three grouped flows with no failures in
  `/var/folders/0t/_tpl8f9d1xvbvy1x6gbzdlmr0000gn/T/kux-references-smoke-3w0xfe7s/report.json`, against a successful development build.
  It enters UsedBy from all five source kinds, verifies the exact columns and both GVR sort
  directions, narrows a filter, opens the selected Deployment namespace, and returns to the saved
  references before the source list. It also checks manual refresh, all five actual PriorityClass
  workload namespaces, read-only navigation, suppressed marking/mutation/export/display keys, and
  source faults-filter visibility across Goto and return. A failing fake kubectl was installed in PATH; the report confirms no kubectl execution.
- This is read-only mock/demo acceptance. Disposable-cluster UsedBy permission combinations,
  busy workload changes and scale measurements remain part of the shared acceptance gates.
  Full feature parity remains open, including multi-container/workload logs, aggregate workloads,
  specialized resource tables, custom views/jumps, Helm, XRay/Pulses, benchmarks and node shell.

Subsequent audit correction: the passing batch-seven fixtures above establish Kux's namespace-list
navigation and focused selection, not exact upstream Goto behavior. Pinned
`internal/view/reference.go` passes the selected fully qualified name to `gotoResource`;
`internal/view/command.go::componentFor` sets the instance, and
`internal/model/table.go::reconcile` GETs that one object. R17 therefore remains Partial until
Kux gains the equivalent instance view. The same shared mode is required for aggregate-workload
Goto in R24. The historical test and PTY outcomes remain unchanged.
The tenth implementation batch below supplies that instance mode and updates current R17 status;
this correction remains here to preserve the audit history.

## Eighth implementation batch: evidence and limits

CronJob Enter now opens Jobs in the selected CronJob's namespace; Job Enter opens Pods using the
current Job selector. Both routes read the selected parent asynchronously, including when compact
list metadata already contains a selector, and remain available in read-only mode. Matching Job
owner-reference names reproduce the pinned K9s behavior: the relationship does not require the
same owner UID, API kind, or controller flag. The Job list starts with the youngest rows first.

- Fourteen App tests in `src/app_batch_navigation_tests.rs` cover selected versus marked identity,
  exact batch/v1 resource scope, discovered Job descriptors, namespace/history, cleared child text
  filters, default Job age order, nil versus empty selectors, denied/deleted/malformed parents,
  request cancellation/reopening and stale context/generation/view/selection/overlay/input.
  Deployment and batch reads cannot satisfy one another's pending requests.
- Eleven backend tests in `src/source/batch_navigation_tests.rs` cover exact GET paths and returned
  identity, full matchLabels/matchExpressions conversion, Kubernetes selector validation boundaries,
  nil/null selectors matching nothing, explicitly empty selectors matching everything, permission
  failures, configured deadlines and cancellation of an unbounded parent read. Invalid targets or
  deadlines produce no request.
- Four integration fixtures cover owner-name filtering for dynamic objects and full/metadata-only
  progressive Table rows, removal when watched ownership changes, nil-selector empty snapshots and
  scoped redacted failures. Local owner-name and match-none predicates never become server field
  selectors. The identified-read rejection matrix now covers 36 cases, so rejected and superseded
  batch reads terminate their matching loading state even when the delivery queue is full.
- Enter hints advertise Jobs or Pods without depending on hydrated child metadata, including in
  read-only mode. A focused renderer test also excludes same-named custom-group resources and keeps
  CronJob Trigger hidden in read-only mode. Help and the interaction matrix list both routes.
- The portable [batch navigation replay](../scripts/smoke_batch_navigation.py) passed all three
  grouped flows with no failures in
  `/var/folders/0t/_tpl8f9d1xvbvy1x6gbzdlmr0000gn/T/kux-references-smoke-_81202fl/batch-report.json`.
  Run `rtk proxy python3 scripts/smoke_batch_navigation.py` after a development build. It follows CronJob to owner-filtered
  Jobs to Pods, restores both scopes through Escape, repeats the path in read-only mode and opens
  Pods from a Job selected directly from the all-namespace list. Captured rows prove that unrelated
  same-namespace Jobs are excluded and Pod views retain the selected Job's namespace. The failing
  kubectl trap recorded no execution.
- Final combined verification passed **619 tests**, with **11 live-cluster tests ignored**.
  All-target Clippy with warnings denied, formatting checks and the development build passed.
  Both fresh UsedBy and batch-navigation PTY reports used binary SHA-256
  `4b337bc1243e3d517e3c70e5d6d90cc32183789a41af9c2b17dfd3b53542f740`, built at
  `2026-10-13T06:10:33Z`; no source file was newer at replay. The inventory retains 72 unique IDs,
  and its documentation's local Markdown links resolve.
- This acceptance uses local HTTP fixtures and generated demo resources. Disposable-cluster
  navigation, workload changes during busy watches, exact specialized Job/CronJob tables, workload
  logs and the CronJob suspension-state preview remain open. Full K9s parity remains unproven.

## Ninth implementation batch: evidence and limits

Current and previous logs now merge sources for Pods, Deployments, StatefulSets, DaemonSets,
Services and Jobs. The selected object's valid default-container annotation starts a default
scope, and `a` switches to all regular/init/ephemeral containers. A sole template container stays
selected even if matching Pods have injected sidecars; Services include all containers and ignore
Pod default annotations. Explicit container selection cannot expand through `a`. Read-only mode
retains these observation routes.

- Resolver HTTP fixtures in `src/source/log_scope_tests.rs` verify exact selected identities,
  namespace-local selectors, pagination beyond one server page, deduplicated sources, empty scopes,
  invalid or expression-only selectors, missing/deleted/denied resources, deadlines and cancellation.
  Workload log selectors follow the pinned `matchLabels` behavior; Job Enter navigation separately
  supports full selectors. Repeated or oversized continuation tokens reject an incomplete scope.
- Collection fixtures now use realistic `v1/PodList` envelopes with members that omit `apiVersion`
  and `kind`. Review against the pinned `kube-core` 4.0.0 deserializer caught the earlier fixtures'
  assumption that every member repeats these fields. The resolver accepts omitted member types
  while rejecting conflicting explicit types, invalid envelopes and wrong namespace/name identity.
  This follows the typed collection semantics and omitted item fields shown in the
  [Kubernetes API collections reference](https://kubernetes.io/docs/reference/using-api/api-concepts/#collections).
- Merged-session real-client HTTP fixtures in `src/source/log_session_tests.rs` verify concurrent
  opening, mixed one/many-container Pods, previous logs, completion after the last source, redacted
  partial failures, independent replay cursors and cancellation while the shared channel is full.
  Current and previous logs both follow outside Head, matching pinned upstream. Sources append in
  arrival order; they are not globally sorted by timestamp.
- App and renderer fixtures cover exact supported GVRs, selected versus marked rows, plugin `p`
  precedence, stale session results, default/all scope titles, range reloads, explicit-container
  restrictions and narrow action hints. Source identity stays aligned through buffer eviction,
  pause/resume, local markers, filtering, timestamp hiding and copy/save. Identity strings count
  toward the display/backlog byte limits, and payload severity does not come from source names.
- Prefixes follow `internal/dao/log_item.go` and `log_items.go`: gray bold API timestamp first,
  padded to 30 columns, Pod for workload scopes, then bold container unless that source is single.
  The source-color hash preserves upstream's byte-index-weighted Unicode calculation and palette,
  with one deliberate correction: the upstream `fushia` spelling becomes fuchsia (`#ff00ff`).
- The portable [merged log replay](../scripts/smoke_merged_logs.py) passed all four grouped flows
  with 19 saved log exports and no failures in
  `/var/folders/0t/_tpl8f9d1xvbvy1x6gbzdlmr0000gn/T/kux-references-smoke-m0pziu_j/merged-logs-report.json`.
  Run `rtk proxy python3 scripts/smoke_merged_logs.py` after a development build. It exercises Pod
  default/all toggles, timestamps, filtering, range reloads, markers and complete-buffer export;
  current/previous logs from every supported workload; explicit container scope; and read-only
  merged logs. The failing kubectl trap recorded no invocation. This uses generated demo data,
  isolated configuration and a nonexistent kubeconfig.
- Final combined verification passed **667 tests**, with **11 live-cluster tests ignored**.
  All-target Clippy with warnings denied, formatting checks and the development build passed.
  The PTY report used binary SHA-256
  `20ce56362a34a3a0c7dc811381dd9424ef079fe09befc156188a969a97a74c8d`, built at
  `2026-10-13T06:33:33Z`; no Rust source was newer at replay. The inventory retains 72 unique IDs;
  all 19 local Markdown links across these documents resolve. Portable replay scripts parse
  successfully and leave no Python bytecode in the checkout.
- The shared channel and display buffers are bounded. Replay tracking divides a 5,000-line/1-MiB
  payload budget among sources, plus a separate replay copy of up to 1 MiB per session. Batch limits
  are divided by source count with a minimum of one line; each stream still owns a partial-line
  buffer of up to 64 KiB and an active connection. Total transport memory therefore grows with the
  source count, and high-fanout workload performance remains an acceptance gate.
- Membership resolves on opening, range changes or container toggles. Running sessions do not
  continuously discover rollout replacements, and same-name Pod replacements are not UID-bound.
  Disposable-cluster recovery, scale testing and the remaining inventory gaps stay open; this
  batch does not establish full K9s feature parity.

## Tenth implementation batch: evidence and limits

`:workloads` and `:wk` combine the pinned six resource kinds: Pods, Services, DaemonSets,
StatefulSets, Deployments and ReplicaSets. Rows retain exact GVR/namespace/name identity for
YAML, Describe, Edit and marked deletion. Enter opens a concrete GET-based instance view and
preserves the aggregate namespace; Escape restores the filter, sorting, selected identity and
marks. UsedBy Goto now uses the same exact instance read, with the referenced namespace becoming
active as in upstream. This resolves the earlier R17 namespace-list discrepancy without changing
the historical batch-seven evidence.

- `src/source/resource_views.rs` concurrently requests each real kind's Table, pages all results
  with the requested selectors, and publishes only a complete combined snapshot. A failed refresh
  reports its error while retaining the previous complete snapshot; an initial failure publishes
  no partial rows. There is no synthetic Kubernetes `/workloads` endpoint. GVR/namespace/name keys
  keep equal names in different kinds distinct.
- Real-client HTTP fixtures in `src/source/resource_views_tests.rs` exercise GET when LIST is
  forbidden, mismatched namespace/name/type rejection, cluster-scoped targets, metadata-only
  CRD Tables and raw-object fallback. Instance failures clear old rows. Aggregate fixtures cover
  all-namespace scope, field/label selectors, multi-page results, inconsistent columns, repeated
  continuation tokens, denied kinds, deadlines and cancellation of all outstanding requests.
  Projection fixtures compare pinned readiness/status/validity rules, including completed Pods
  and Services. Polling follows the configured refresh interval and remains generation-scoped.
- App fixtures cover aliases/completion through catalog resets, initial Kind order and sorting,
  same-name marked identities, selected versus marked action targets, exact singleton scope,
  namespace changes, canceled/stale snapshots and parent restoration across initial Loading.
  Aggregate views expose YAML/Describe/Edit/delete but suppress logs, scaling, restart, image
  changes and immediate Kill. Read-only mode permits observation and blocks mutation.
- Eight renderer/export fixtures in `src/ui_workloads.rs` cover exact
  KIND/NAMESPACE/NAME/STATUS/READY/AGE order in namespaced and all-namespace views, wide-only VALID,
  full CSV identity, live age and `<unknown>` formatting, plugin columns, Kind mouse sorting, horizontal scrolling
  at narrow widths, pending-colored degraded rows, visible marks, exact singleton titles and
  mixed-kind confirmation wording and visible-column keyboard/VALID mouse sorting. Sort shortcuts
  are `Shift-K`, `Shift-R` and `Shift-A`.
- Two deliberate differences remain visible in this implementation: the selected-instance worker
  can operate with GET permission even when LIST for that resource is forbidden, while pinned
  upstream's browser initialization also performs a LIST-access preflight and initializes its
  informer. Kux's existing background Pod cache still lists/watches Pods; a GET-based instance
  worker is not a claim that the entire application requires only GET. Kux also titles an instance
  with its namespace/name to make the exact target visible; upstream's instance table retains its
  active-scope title. These choices do not establish exact RBAC or title parity.
- Local fixture and generated-demo acceptance do not close disposable-cluster RBAC, busy workload
  churn, server Table variation or large-scale performance gates. Full K9s feature parity and
  remaining inventory items stay open.

## Verification and completion gates

- **Traceability:** each ID has links to its Kux implementation, a behavior test/fixture and the
  relevant upstream source. Record “not run” distinctly from pass; replace this initial status
  only after integration, not after an isolated helper or key placeholder is added.
- **API/process correctness:** fake API servers and subprocesses assert method/path/body, GVR,
  namespace, context, selected/marked identity, request cancellation and errors. Pure helper tests
  do not prove an action is reachable from the UI or uses the live configuration.
- **Interaction:** PTY replays exercise the same user journeys against the pinned K9s baseline and
  Kux, with view content/action results captured. Include narrow terminal, resizing, Unicode,
  empty lists, busy streams, command/filter modes, modals, history and process exit restoration.
- **Isolated integration:** verify real CRUD, scale, trigger, rollback, evictions, exec/attach,
  transfer, port forwarding, Helm and manifests against disposable fixtures/clusters. Never use
  an existing production context merely to validate a feature; keep external mutation tests
  explicitly selected and separate from the default test suite.
- **Read-only and failures:** every mutating route, including plugins and nested views, rejects
  dispatch in read-only mode. Permission denials, missing binaries/APIs, unavailable metrics,
  stale object UID/resourceVersion and partial batch failures remain actionable and do not freeze
  input or silently operate on a replacement object/context.
- **Kux performance contract:** preserve async watches, bounded channels/logs, compact rows,
  generation-safe snapshots, atomic relists and viewport rendering. Exercise at least the existing
  100,000-row demo scenario with filter/sort/navigation while streams run, measuring regressions
  against the current Kux baseline rather than treating a passing functional test as scale proof.
- **Build and docs:** run appropriate formatting, tests, clippy and release build for the final
  integrated tree; validate help/key hints and README against actual behavior. Publish the
  remaining-gap list until it is empty, including unresolved advertised features.
- **Final audit:** re-enumerate upstream registrations/config/CLI at the current stable release;
  account for every feature with direct acceptance evidence. All rows being merely “Present,”
  green unit tests, or successful generic discovery are insufficient to declare full parity.

[release]: https://github.com/derailed/k9s/releases/tag/v0.51.0
[source]: https://github.com/derailed/k9s/tree/558caafe7ba067467de46b320cc22ef11fef9c34
[cli]: https://github.com/derailed/k9s/blob/v0.51.0/cmd/root.go
[config]: https://github.com/derailed/k9s/tree/v0.51.0/internal/config
[schemas]: https://github.com/derailed/k9s/tree/v0.51.0/internal/config/json/schemas
[context-config]: https://github.com/derailed/k9s/blob/v0.51.0/internal/config/data/context.go
[commands]: https://github.com/derailed/k9s/tree/v0.51.0/internal/view/cmd
[table]: https://github.com/derailed/k9s/blob/v0.51.0/internal/view/table.go
[browser]: https://github.com/derailed/k9s/blob/v0.51.0/internal/view/browser.go
[app]: https://github.com/derailed/k9s/blob/v0.51.0/internal/view/app.go
[view-registry]: https://github.com/derailed/k9s/blob/v0.51.0/internal/view/registrar.go
[model-registry]: https://github.com/derailed/k9s/blob/v0.51.0/internal/model/registry.go
[logs]: https://github.com/derailed/k9s/blob/v0.51.0/internal/view/log.go
[log-data]: https://github.com/derailed/k9s/blob/v0.51.0/internal/dao/log_options.go
[log-retries]: https://github.com/derailed/k9s/blob/v0.51.0/internal/dao/pod.go#L357
[live-view]: https://github.com/derailed/k9s/blob/v0.51.0/internal/view/live_view.go
[live-config]: https://github.com/derailed/k9s/blob/v0.51.0/internal/config/k9s.go#L68
[yaml-model]: https://github.com/derailed/k9s/blob/v0.51.0/internal/model/yaml.go#L116
[describe-model]: https://github.com/derailed/k9s/blob/v0.51.0/internal/model/describe.go#L106
[ports]: https://github.com/derailed/k9s/tree/v0.51.0/internal/port
[rbac]: https://github.com/derailed/k9s/blob/v0.51.0/internal/dao/rbac.go
[xray]: https://github.com/derailed/k9s/blob/v0.51.0/internal/view/xray.go
[pulse]: https://github.com/derailed/k9s/blob/v0.51.0/internal/view/pulse.go
[helm]: https://github.com/derailed/k9s/blob/v0.51.0/internal/dao/helm_chart.go
[bench-config]: https://github.com/derailed/k9s/blob/v0.51.0/internal/config/benchmark.go
[vulnerability]: https://github.com/derailed/k9s/blob/v0.51.0/internal/vul/scanner.go
[directory]: https://github.com/derailed/k9s/blob/v0.51.0/internal/view/dir.go
[popeye-removal]: https://github.com/derailed/k9s/blob/v0.51.0/change_logs/release_v0.31.9.md
[aliases]: https://github.com/derailed/k9s/blob/v0.51.0/internal/config/alias.go
[hotkeys]: https://github.com/derailed/k9s/blob/v0.51.0/internal/config/hotkey.go
[plugins]: https://github.com/derailed/k9s/blob/v0.51.0/internal/config/plugin.go
[views]: https://github.com/derailed/k9s/blob/v0.51.0/internal/config/views.go
[jumps]: https://github.com/derailed/k9s/blob/v0.51.0/internal/config/jumps.go
[skins]: https://github.com/derailed/k9s/blob/v0.51.0/internal/config/styles.go
