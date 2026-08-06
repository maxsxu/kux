# K9s interaction parity

Kux follows K9s's command grammar and key placement without importing K9s code. This matrix makes
the current boundary explicit: **implemented** performs the action, **interaction** recognizes the
K9s key and reports that its backend is pending, and **planned** is not yet exposed.

## Global and navigation

| Interaction | Key or command | Status |
| --- | --- | --- |
| Active help | `?` | Implemented |
| Resource aliases | `Ctrl-A` | Implemented for every discovered listable resource; chooser replaces the body |
| Quit | `:q`, `:quit`, `Ctrl-C` | Implemented |
| Previous/next view | `Esc`, `[`, `-`, `]` | Implemented with hidden view history |
| Resource command | `:pod`, `:secret`, `:plural.group` | Implemented; discovery includes CRDs and aggregated APIs |
| CustomResourceDefinition command | `:crd`, `:crds` | Implemented as exact aliases for the CRD resource |
| Resource in namespace | `:pod ns-x` | Implemented |
| Regex, inverse, fuzzy, label filter | `/x`, `/!x`, `/-f x`, `/-l k=v` | Implemented |
| Highlight and navigate filter matches | `n`, `Shift-N` | Implemented across resource lists, choosers, and content; navigation wraps at both ends |
| Command/filter in in-place views | `:`, `/` | Implemented for resource, resource chooser, namespace chooser, context, details, YAML, describe, logs, container, and policy bodies |
| Restore previous/clear filter input | `Tab`, `Ctrl-U` | Implemented with muted history hint |
| Cancel mode | `Esc` | Implemented |
| Context chooser/direct context | `:ctx`, `:ctx name` | Implemented; body columns are Name/Cluster/User/Namespace, `c` copies the selected name, switching rebuilds the client, and `Ctrl-D` confirms deletion of inactive persisted contexts |
| Namespace chooser/direct namespace | `:ns`, `:ns name` | Implemented; body columns are Name/Status/Age, `y` opens the selected Namespace YAML, and direct selection switches immediately |
| Recent namespace shortcuts | `0`…`9` | Implemented; `0` is always all namespaces and `1`…`9` are named recents |
| K9s-style header menu | automatic | Implemented as a lossless responsive header: cluster information is measured and wrapped first, the upright logo is shown only when it fits, and only complete namespace/action hotkey columns are admitted in priority order |
| Categorized help body | `?` | Implemented in place with Navigation, General, Resources, Display, and Mutation categories |
| Command completion | `Tab`, `Shift-Tab` | Implemented for discovered resource names and short names, built-in aliases, contexts, and namespaces |
| Refresh | `Ctrl-R` | Implemented |

## Table interaction

| Interaction | Key | Status |
| --- | --- | --- |
| Vim navigation | `h/j/k/l`, `g/G`, `Ctrl-B/F` | Implemented contextually across resource lists, choosers, text bodies, help, details, and confirmation actions |
| Mark, range mark, clear | `Space`, `Ctrl-Space`, `Ctrl-\` | Implemented |
| Faults, wide, header, logo | `Ctrl-Z/W/E/L` | Implemented |
| Select column | `Shift-Left`, `Shift-Right` | Implemented |
| Sort selected/name/age/namespace/status | `Shift-O/N/A/P/S` | Implemented |
| Sort by mouse | Left-click a column header | Implemented; repeated clicks reverse direction |
| Copy name/namespace | `c`, `n` | Implemented with native clipboard commands and OSC 52 fallback |
| Decode Secret data | `x` | Implemented for exact core/v1 Secrets; list rows open decoded data only, while full YAML toggles encoded/decoded values |
| Details/YAML/object description | `Enter`, `y`, `d` | Implemented in place; `d` runs qualified `kubectl describe` |
| Warp to namespace | `w` | Implemented in all-namespace views |
| Pod and node metrics columns | automatic | Implemented through `metrics.k8s.io` |
| Node → pods on node | `Enter` on Node | Implemented with server-side field selector |
| Workload → owned children | `Enter` on Deployment/ReplicaSet/StatefulSet/DaemonSet | Implemented with server-side label selectors; selector-backed Services also open Pods |
| Pod → containers | `Enter` on Pod | Implemented for regular, init, and ephemeral containers |
| Container → logs | `Enter` on container | Implemented with explicit container selection |
| APIService status table | `:apiservice` | Implemented with `NAME`, `SERVICE`, `AVAILABLE`, and `AGE` |
| ServiceAccount effective rules | `Enter` on ServiceAccount | Implemented by matching RoleBindings and ClusterRoleBindings and resolving referenced roles |
| RBAC rule view | `Enter` on Role/ClusterRole/RoleBinding/ClusterRoleBinding | Implemented as a resource, API group, binding, and verb matrix |

## Content and mutations

| Interaction | Key | Status |
| --- | --- | --- |
| Pod/container logs/previous logs | `l`, `p` | Implemented, last 500 lines; Pod actions resolve and pass the selected default container for both current and previous requests, and timestamps/severity levels are colorized without changing the raw content |
| Copy YAML/toggle managed fields | `c`, `m` in YAML | Implemented; copy uses the visible variant |
| Search YAML without hiding context | `/query`, then `n`/`Shift-N` in YAML | Implemented; keeps the full document, highlights matches, and scrolls between matching lines |
| Copy decoded Secret data | `c` in Secret data view | Implemented; copies the data-only body |
| Wrap/timestamps/fullscreen | `w`, `t`, `f` in content view | Implemented UI toggles |
| Delete marked resources, current resource, or inactive context with Tab + Enter confirmation | `Ctrl-D` | Implemented; all marked resources are deleted as one bounded batch, while active/synthetic contexts and resources without delete capability remain protected |
| Kill without confirmation | `Ctrl-K` | Implemented with zero grace period |
| Pod shell | `s` | Implemented through interactive `kubectl exec` with the active context and namespace; the host terminal is cleared and starts with a Kux Pod/container identity banner, and Kux explicitly targets the valid `kubectl.kubernetes.io/default-container` annotation or the first `spec.containers` entry; unavailable in read-only mode |
| Pod attach | `a` | Interaction; backend pending |
| Edit any Kubernetes resource | `e` | Implemented from list, details, and YAML views through interactive `kubectl edit` with the active context and selected resource namespace; unavailable in read-only mode |
| Port-forward list/start | `f`, `:pf`, `Shift-F` | Implemented for Pods and Services with declared TCP-port suggestions, editable local port/address, bounded `kubectl port-forward` subprocesses, a context-wide full-body list, and `Ctrl-D` stop; starting a forward is unavailable in read-only mode |
| Owner navigation | `Shift-J` | Implemented through controller owner references and a server-side name selector |
| Pod Node navigation | `o` | Implemented through the compact Pod `spec.nodeName` and a server-side Node name selector |
| Service benchmark | `b` | Interaction; backend pending |
| Cordon/uncordon and drain | `u`, `r` on Nodes | Cordon and uncordon use a guarded scheduling-state patch; drain includes editable grace period and timeout plus Ignore DaemonSets, Delete EmptyDir Data, Force, and Disable Eviction toggles; preflight safety checks run before cordon and evictions remain PDB-aware unless explicitly disabled |
| Workload restart | `r` on Deployments/StatefulSets/DaemonSets | Implemented with confirmation, multi-selection, bounded concurrent Pod-template patches, and partial-failure reporting; unavailable in read-only mode |
| Namespace use, UsedBy, CronJob trigger | `u`, `t` | Planned with those resource views |

Mutations are enabled by default for K9s-compatible operation. `--read-only` disables edit, Pod
shell, port-forward start, resource and context deletion, kill, workload restart, node scheduling
changes, drain, and every future mutation path.
