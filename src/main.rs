use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{Event, EventStream, KeyEventKind};
use futures::StreamExt;
use kube::{Client, Config, config::KubeConfigOptions};
use kux::{
    app::{ActionIntent, App, AppCommand, ContentKind, Overlay, PodShellTarget, ResourceTarget},
    cli::Cli,
    kubeconfig::{ContextCatalog, ContextInfo, fallback_contexts},
    model::ResourceSnapshot,
    port_forward::PortForwardManager,
    source::{self, ActionRequest, ActionResult, KubernetesSettings, WatchRequest},
    terminal::TerminalGuard,
    ui,
};
use tokio::{
    process::Command,
    sync::{mpsc, watch},
};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let context_catalog = if cli.demo {
        ContextCatalog {
            active: "demo".to_owned(),
            contexts: vec![ContextInfo::synthetic("demo")],
        }
    } else {
        fallback_contexts(cli.context.as_deref())
    };
    let context_label = context_catalog.active;
    let context_options = context_catalog.contexts;
    let (request_tx, request_rx) = mpsc::channel(8);
    let (snapshot_tx, snapshot_rx) = watch::channel(ResourceSnapshot::loading(1, cli.resource));
    let (action_tx, action_rx) = mpsc::channel(16);
    let (action_result_tx, action_result_rx) = mpsc::channel(16);

    let (namespace, source_task) = if cli.demo {
        let namespace = if cli.all_namespaces || !cli.resource.is_namespaced() {
            None
        } else {
            cli.namespace.clone()
        };
        let source_task = source::spawn_demo(
            cli.demo_items,
            request_rx,
            snapshot_tx,
            action_rx,
            action_result_tx,
        );
        (namespace, source_task)
    } else {
        let (client, default_namespace) = kubernetes_client(cli.context.as_deref()).await?;
        let namespace = if cli.all_namespaces || !cli.resource.is_namespaced() {
            None
        } else {
            Some(cli.namespace.clone().unwrap_or(default_namespace))
        };
        let source_task = source::spawn_kubernetes(
            client,
            KubernetesSettings {
                initial_context: context_label.clone(),
                publish_interval: cli.publish_interval(),
                streaming_lists: !cli.legacy_list,
            },
            request_rx,
            snapshot_tx,
            action_rx,
            action_result_tx,
        );
        (namespace, source_task)
    };

    let mut app = App::new(
        context_label,
        namespace,
        cli.resource,
        cli.read_only,
        context_options,
    );
    if cli.no_logo {
        app.set_logo_visible(false);
    }
    request_tx
        .send(WatchRequest {
            generation: app.generation,
            context: app.context.clone(),
            kind: app.kind.clone(),
            namespace: app.namespace.clone(),
            field_selector: app.field_selector.clone(),
            label_selector: app.label_selector.clone(),
        })
        .await
        .context("data source stopped before startup")?;

    let result = run(
        app,
        request_tx,
        snapshot_rx,
        action_tx,
        action_result_rx,
        cli.frame_interval(),
    )
    .await;
    source_task.abort();
    result
}

async fn kubernetes_client(context: Option<&str>) -> Result<(Client, String)> {
    let config = if let Some(context) = context {
        Config::from_kubeconfig(&KubeConfigOptions {
            context: Some(context.to_owned()),
            ..KubeConfigOptions::default()
        })
        .await
        .with_context(|| format!("load kubeconfig context {context:?}"))?
    } else {
        Config::infer()
            .await
            .context("infer Kubernetes configuration")?
    };
    let namespace = config.default_namespace.clone();
    let client = Client::try_from(config).context("create Kubernetes client")?;
    client
        .apiserver_version()
        .await
        .context("authenticate Kubernetes client")?;
    Ok((client, namespace))
}

async fn run(
    mut app: App,
    requests: mpsc::Sender<WatchRequest>,
    mut snapshots: watch::Receiver<ResourceSnapshot>,
    actions: mpsc::Sender<ActionRequest>,
    mut action_results: mpsc::Receiver<ActionResult>,
    frame_interval: Duration,
) -> Result<()> {
    let mut terminal = TerminalGuard::enter()?;
    let mut events = EventStream::new();
    let mut frames = tokio::time::interval(frame_interval);
    frames.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(1));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dirty = true;
    let (port_forward_events_tx, mut port_forward_events_rx) = mpsc::channel(64);
    let mut port_forwards = PortForwardManager::new(port_forward_events_tx);

    loop {
        tokio::select! {
            event = events.next() => {
                match event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        let command = app.handle_key(key);
                        if handle_app_command(
                            command,
                            &mut app,
                            &mut terminal,
                            &requests,
                            &actions,
                            &mut port_forwards,
                        )
                        .await?
                        {
                            break;
                        }
                        dirty = true;
                    }
                    Some(Ok(Event::Resize(_, _))) => dirty = true,
                    Some(Ok(Event::Mouse(mouse))) => {
                        if app.handle_mouse(mouse) {
                            dirty = true;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(error).context("read terminal event"),
                    None => break,
                }
            }
            changed = snapshots.changed() => {
                if changed.is_err() {
                    break;
                }
                app.apply_snapshot(snapshots.borrow_and_update().clone());
                dirty = true;
            }
            Some(result) = action_results.recv() => {
                let command = apply_action_result(&mut app, result);
                if handle_app_command(
                    command,
                    &mut app,
                    &mut terminal,
                    &requests,
                    &actions,
                    &mut port_forwards,
                )
                .await?
                {
                    break;
                }
                dirty = true;
            }
            Some(event) = port_forward_events_rx.recv() => {
                if let Some(notice) = port_forwards.handle_event(event) {
                    app.show_transient_notice(notice);
                }
                refresh_visible_port_forwards(&mut app, &port_forwards);
                dirty = true;
            }
            _ = heartbeat.tick() => {
                app.tick();
                dirty = true;
            }
            _ = frames.tick(), if dirty => {
                terminal.terminal_mut().draw(|frame| ui::draw(frame, &mut app))?;
                dirty = false;
            }
        }
    }
    Ok(())
}

async fn handle_app_command(
    command: AppCommand,
    app: &mut App,
    terminal: &mut TerminalGuard,
    requests: &mpsc::Sender<WatchRequest>,
    actions: &mpsc::Sender<ActionRequest>,
    port_forwards: &mut PortForwardManager,
) -> Result<bool> {
    match command {
        AppCommand::Quit => return Ok(true),
        AppCommand::Request(view) => {
            requests
                .send(WatchRequest {
                    generation: app.generation,
                    context: view.context,
                    kind: view.kind,
                    namespace: view.namespace,
                    field_selector: view.field_selector,
                    label_selector: view.label_selector,
                })
                .await
                .context("data source stopped")?;
        }
        AppCommand::Copy(value) => handle_copy(app, terminal, &value),
        AppCommand::Edit(target) => {
            let result = edit_resource(terminal, &app.context, &target)
                .await
                .context("restore terminal after resource edit")?;
            match result {
                Ok(status) if status.success() => app.show_transient_notice(format!(
                    "Edited {} {}",
                    target.kind.qualified_name(),
                    target.name
                )),
                Ok(status) => app.show_transient_notice(format!(
                    "Edit failed: kubectl edit exited with {status}"
                )),
                Err(error) => app.show_transient_notice(format!("Edit failed: {error}")),
            }
        }
        AppCommand::Shell(target) => {
            let result = open_pod_shell(terminal, &app.context, &target)
                .await
                .context("restore terminal after pod shell")?;
            report_shell_result(app, &target, result);
        }
        AppCommand::StartPortForward(request) => match port_forwards.start(&app.context, request) {
            Ok(forward) => app.show_transient_notice(format!(
                "Starting port forward on {}:{}",
                forward.address, forward.local_port
            )),
            Err(error) => {
                app.show_transient_notice(format!("Port forward failed: {error}"));
            }
        },
        AppCommand::ShowPortForwards => {
            let forwards = port_forwards.summaries(&app.context);
            app.show_port_forwards(forwards);
        }
        AppCommand::StopPortForward(id) => {
            if let Some(forward) = port_forwards.stop(id) {
                app.show_transient_notice(format!(
                    "Stopping port forward on {}:{}",
                    forward.address, forward.local_port
                ));
            }
            refresh_visible_port_forwards(app, port_forwards);
        }
        AppCommand::Action(action) => {
            let keeps_overlay = matches!(&action, ActionIntent::ManagedYaml { .. });
            actions
                .send(ActionRequest {
                    generation: app.generation,
                    intent: action,
                })
                .await
                .context("action worker stopped")?;
            if keeps_overlay {
                app.show_transient_notice("Loading managed fields…");
            } else {
                app.show_notice("Working…");
            }
        }
        AppCommand::None => {}
    }
    Ok(false)
}

fn refresh_visible_port_forwards(app: &mut App, manager: &PortForwardManager) {
    let forwards = manager.summaries(&app.context);
    app.refresh_port_forwards(forwards);
}

fn apply_action_result(app: &mut App, result: ActionResult) -> AppCommand {
    match result {
        ActionResult::Scoped { generation, result } => {
            if app.generation == generation {
                return apply_action_result(app, *result);
            }
        }
        ActionResult::Notice(notice) | ActionResult::Error(notice) => app.show_notice(notice),
        ActionResult::Content {
            title,
            body,
            managed_fields,
            decoded_body,
            decoded_managed_fields,
            content,
        } => app.show_content(
            title,
            body,
            managed_fields,
            decoded_body,
            decoded_managed_fields,
            content,
        ),
        ActionResult::ManagedYaml {
            target,
            body,
            decoded_body,
        } => app.show_managed_yaml(&target, body, decoded_body),
        ActionResult::NamespaceOptions {
            context,
            namespaces,
        } => app.set_namespace_options(&context, namespaces),
        ActionResult::ContextOptions {
            context,
            contexts,
            notice,
        } => app.apply_context_options(&context, contexts, notice),
        ActionResult::ResourceOptions { context, resources } => {
            if app.context == context {
                app.set_resource_options(resources);
            }
        }
        ActionResult::ChildResource {
            parent,
            kind,
            owner_uid,
        } => return app.show_discovered_child(&parent, kind, &owner_uid),
        ActionResult::NoChildResource { parent } => app.show_no_discovered_child(&parent),
        ActionResult::Owner { target, owner } => {
            return app.show_resolved_owner(&target, owner);
        }
        ActionResult::Containers { target, rows } => app.show_containers(target, rows),
        ActionResult::Policy {
            target,
            title,
            rows,
        } => app.show_policy(&target, title, rows),
        ActionResult::ServerVersion(version) => app.set_server_version(version),
    }
    AppCommand::None
}

fn handle_copy(app: &mut App, terminal: &mut TerminalGuard, value: &str) {
    let message = copy_success_message(app);
    match terminal.copy_to_clipboard(value) {
        Ok(()) => app.show_transient_notice(message),
        Err(error) => app.show_transient_notice(format!("Copy failed: {error}")),
    }
}

fn copy_success_message(app: &App) -> &'static str {
    match (app.overlay, app.content_kind) {
        (Overlay::Content, Some(ContentKind::Yaml | ContentKind::SecretData)) => {
            "YAML copied to clipboard"
        }
        (Overlay::Content, Some(ContentKind::Logs | ContentKind::PreviousLogs)) => {
            "Logs copied to clipboard"
        }
        _ => "Copied to clipboard",
    }
}

fn edit_args(context: &str, target: &ResourceTarget) -> Vec<String> {
    let mut args = vec![
        "edit".to_owned(),
        target.kind.qualified_name(),
        target.name.clone(),
        "--context".to_owned(),
        context.to_owned(),
    ];
    if let Some(namespace) = &target.namespace {
        args.extend(["--namespace".to_owned(), namespace.clone()]);
    }
    args
}

async fn edit_resource(
    terminal: &mut TerminalGuard,
    context: &str,
    target: &ResourceTarget,
) -> Result<Result<std::process::ExitStatus, String>> {
    terminal
        .suspend()
        .context("suspend terminal for resource edit")?;
    let status = Command::new("kubectl")
        .args(edit_args(context, target))
        .status()
        .await
        .map_err(|error| format!("Unable to run kubectl edit: {error}"));
    terminal.resume()?;
    Ok(status)
}

fn pod_shell_args(context: &str, target: &PodShellTarget) -> Vec<String> {
    let mut args = vec![
        "exec".to_owned(),
        "-it".to_owned(),
        "--context".to_owned(),
        context.to_owned(),
        "--namespace".to_owned(),
        target.namespace.clone(),
        target.pod.clone(),
    ];
    args.extend(["--container".to_owned(), target.container.clone()]);
    args.extend([
        "--".to_owned(),
        "sh".to_owned(),
        "-c".to_owned(),
        "command -v bash >/dev/null && exec bash || exec sh".to_owned(),
    ]);
    args
}

fn pod_shell_banner(target: &PodShellTarget) -> String {
    format!(
        "<<Kux-Shell>> Pod: {}/{} | Container: {}",
        target.namespace, target.pod, target.container
    )
}

async fn open_pod_shell(
    terminal: &mut TerminalGuard,
    context: &str,
    target: &PodShellTarget,
) -> Result<Result<std::process::ExitStatus, String>> {
    terminal
        .suspend()
        .context("suspend terminal for pod shell")?;
    terminal
        .show_interactive_banner(&pod_shell_banner(target))
        .context("render pod shell banner")?;
    let status = Command::new("kubectl")
        .args(pod_shell_args(context, target))
        .status()
        .await
        .map_err(|error| format!("Unable to run kubectl exec: {error}"));
    terminal.resume()?;
    Ok(status)
}

fn report_shell_result(
    app: &mut App,
    target: &PodShellTarget,
    result: std::result::Result<std::process::ExitStatus, String>,
) {
    match result {
        Ok(status) if status.success() => {
            app.show_transient_notice(format!(
                "Shell closed for {}/{}",
                target.namespace, target.pod
            ));
        }
        Ok(status) => {
            app.show_transient_notice(format!("Shell failed: kubectl exec exited with {status}"));
        }
        Err(error) => {
            app.show_transient_notice(format!("Shell failed: {error}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use kux::{
        app::{App, ContentKind, Overlay, PodShellTarget, ResourceTarget},
        model::{ResourceDescriptor, ResourceKind, ResourceOperations},
        source::ActionResult,
    };

    use super::{
        apply_action_result, copy_success_message, edit_args, pod_shell_args, pod_shell_banner,
    };

    fn target(group: &str, kind: &str, plural: &str, namespace: Option<&str>) -> ResourceTarget {
        ResourceTarget {
            kind: ResourceDescriptor::new(
                group,
                "v1",
                if group.is_empty() {
                    "v1".to_owned()
                } else {
                    format!("{group}/v1")
                },
                kind,
                plural,
                namespace.is_some(),
                ResourceOperations::all(),
            ),
            namespace: namespace.map(str::to_owned),
            name: "selected".to_owned(),
        }
    }

    fn yaml_result(generation: u64) -> ActionResult {
        ActionResult::Scoped {
            generation,
            result: Box::new(ActionResult::Content {
                title: "YAML(argo/example)".to_owned(),
                body: "kind: Application".to_owned(),
                managed_fields: None,
                decoded_body: None,
                decoded_managed_fields: None,
                content: ContentKind::Yaml,
            }),
        }
    }

    #[test]
    fn stale_content_results_cannot_open_after_the_view_generation_changes() {
        let mut app = App::new(
            "production",
            None,
            ResourceKind::Pods,
            false,
            vec!["production".to_owned()],
        );
        app.generation = 2;

        apply_action_result(&mut app, yaml_result(1));
        assert_eq!(app.overlay, Overlay::None);

        apply_action_result(&mut app, yaml_result(2));
        assert_eq!(app.overlay, Overlay::Content);
        assert_eq!(app.content_body, "kind: Application");
    }

    #[test]
    fn log_copy_uses_a_content_specific_confirmation() {
        let mut app = App::new(
            "production",
            None,
            ResourceKind::Pods,
            false,
            vec!["production".to_owned()],
        );
        app.show_content(
            "Logs(default/api-0)",
            "INFO started",
            None,
            None,
            None,
            ContentKind::Logs,
        );

        assert_eq!(copy_success_message(&app), "Logs copied to clipboard");
    }

    #[test]
    fn edit_args_qualify_dynamic_resources_and_preserve_scope() {
        assert_eq!(
            edit_args(
                "prod",
                &target("example.io", "Widget", "widgets", Some("team-a"))
            ),
            [
                "edit",
                "widgets.example.io",
                "selected",
                "--context",
                "prod",
                "--namespace",
                "team-a",
            ]
        );
        assert_eq!(
            edit_args("prod", &target("", "Namespace", "namespaces", None)),
            ["edit", "namespaces", "selected", "--context", "prod"]
        );
    }

    #[test]
    fn pod_shell_args_preserve_context_namespace_and_container() {
        let target = PodShellTarget {
            namespace: "team-a".to_owned(),
            pod: "api-0".to_owned(),
            container: "api".to_owned(),
        };
        assert_eq!(
            pod_shell_args("prod", &target),
            [
                "exec",
                "-it",
                "--context",
                "prod",
                "--namespace",
                "team-a",
                "api-0",
                "--container",
                "api",
                "--",
                "sh",
                "-c",
                "command -v bash >/dev/null && exec bash || exec sh",
            ]
        );

        assert_eq!(
            pod_shell_banner(&target),
            "<<Kux-Shell>> Pod: team-a/api-0 | Container: api"
        );
    }

    #[test]
    fn pod_shell_banner_names_an_explicit_container() {
        let target = PodShellTarget {
            namespace: "team-a".to_owned(),
            pod: "api-0".to_owned(),
            container: "sidecar".to_owned(),
        };
        assert_eq!(
            pod_shell_banner(&target),
            "<<Kux-Shell>> Pod: team-a/api-0 | Container: sidecar"
        );
    }
}
