use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{Event, EventStream, KeyEventKind};
use futures::StreamExt;
use kube::{Client, Config, config::KubeConfigOptions};
use kux::{
    app::{App, AppCommand, Overlay},
    cli::Cli,
    kubeconfig::{ContextCatalog, ContextInfo, fallback_contexts},
    model::ResourceSnapshot,
    source::{self, ActionResult, KubernetesSettings, WatchRequest},
    terminal::TerminalGuard,
    ui,
};
use tokio::sync::{mpsc, watch};

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

    let namespace;
    let source_task;
    if cli.demo {
        namespace = if cli.all_namespaces || !cli.resource.is_namespaced() {
            None
        } else {
            cli.namespace.clone()
        };
        source_task = source::spawn_demo(
            cli.demo_items,
            request_rx,
            snapshot_tx,
            action_rx,
            action_result_tx,
        );
    } else {
        let (client, default_namespace) = kubernetes_client(cli.context.as_deref()).await?;
        namespace = if cli.all_namespaces || !cli.resource.is_namespaced() {
            None
        } else {
            Some(cli.namespace.clone().unwrap_or(default_namespace))
        };
        source_task = source::spawn_kubernetes(
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
    }

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
    Ok((client, namespace))
}

async fn run(
    mut app: App,
    requests: mpsc::Sender<WatchRequest>,
    mut snapshots: watch::Receiver<ResourceSnapshot>,
    actions: mpsc::Sender<kux::app::ActionIntent>,
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

    loop {
        tokio::select! {
            event = events.next() => {
                match event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        match app.handle_key(key) {
                            AppCommand::Quit => break,
                            AppCommand::Request(view) => {
                                requests.send(WatchRequest {
                                    generation: app.generation,
                                    context: view.context,
                                    kind: view.kind,
                                    namespace: view.namespace,
                                    field_selector: view.field_selector,
                                }).await.context("data source stopped")?;
                            }
                            AppCommand::Copy(value) => {
                                handle_copy(&mut app, &mut terminal, &value);
                            }
                            AppCommand::Action(action) => {
                                actions.send(action).await.context("action worker stopped")?;
                                app.show_notice("Working…");
                            }
                            AppCommand::None => {}
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
                apply_action_result(&mut app, result);
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

fn apply_action_result(app: &mut App, result: ActionResult) {
    match result {
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
        ActionResult::Containers { target, rows } => app.show_containers(target, rows),
        ActionResult::Policy {
            target,
            title,
            rows,
        } => app.show_policy(&target, title, rows),
        ActionResult::ServerVersion(version) => app.set_server_version(version),
    }
}

fn handle_copy(app: &mut App, terminal: &mut TerminalGuard, value: &str) {
    let message = if app.overlay == Overlay::Content {
        "YAML copied to clipboard"
    } else {
        "Copied to clipboard"
    };
    match terminal.copy_to_clipboard(value) {
        Ok(()) => app.show_transient_notice(message),
        Err(error) => app.show_transient_notice(format!("Copy failed: {error}")),
    }
}
