use std::time::Duration;

use anyhow::{Context, Result};
use clap::{CommandFactory, FromArgMatches, parser::ValueSource};
use crossterm::event::{Event, EventStream, KeyEventKind};
use futures::StreamExt;
use kube::Client;
use kux::{
    app::{
        ActionIntent, App, AppCommand, ContentKind, DumpIntent, DumpPayload, Overlay,
        PodShellTarget, ResourceTarget,
    },
    cli::Cli,
    config::ConfigPaths,
    config_runtime::{ConfigContext, ConfigRuntime},
    connection::ConnectionOptions,
    kubeconfig::{ContextCatalog, ContextInfo, fallback_contexts_with},
    model::ResourceSnapshot,
    plugin_process::PluginProcessManager,
    port_forward::PortForwardManager,
    source::{self, ActionRequest, ActionResult, KubernetesSettings, WatchRequest},
    terminal::TerminalGuard,
    transfer::TransferRuntime,
    ui,
};
use tokio::{
    process::Command,
    sync::{mpsc, watch},
};

mod dump_commands;
mod plugin_commands;

#[tokio::main]
async fn main() -> Result<()> {
    let matches = Cli::command().get_matches();
    let cli = Cli::from_arg_matches(&matches)?;
    if kux::cli_commands::execute(&cli, &mut std::io::stdout())? {
        return Ok(());
    }
    let (refresh_tx, refresh_rx) = watch::channel(
        cli.refresh
            .unwrap_or(Duration::from_secs(5))
            .max(Duration::from_secs(2)),
    );
    let options = RunOptions {
        frame_interval: cli.frame_interval(),
        startup_command: cli.command.clone(),
        use_config_default_view: matches.value_source("resource") != Some(ValueSource::CommandLine),
        demo: cli.demo,
        refresh: refresh_tx,
        screen_dump_dir: cli.screen_dump_dir.clone(),
    };
    let context_catalog = initial_context_catalog(&cli);
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
        let (client, default_namespace) =
            kubernetes_client(&cli.connection, cli.context.as_deref()).await?;
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
                refresh_interval: refresh_rx,
                streaming_lists: !cli.legacy_list,
                connection: cli.connection.clone(),
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
    app.connection = cli.connection.clone();
    app.configure_launch(&cli);
    request_tx
        .send(WatchRequest {
            instance: app.instance.clone(),
            generation: app.generation,
            context: app.context.clone(),
            kind: app.kind.clone(),
            namespace: app.namespace.clone(),
            field_selector: app.field_selector.clone(),
            label_selector: app.label_selector.clone(),
        })
        .await
        .context("data source stopped before startup")?;

    let result = Box::pin(run(
        app,
        request_tx,
        snapshot_rx,
        action_tx,
        action_result_rx,
        options,
    ))
    .await;
    source_task.abort();
    result
}

fn initial_context_catalog(cli: &Cli) -> ContextCatalog {
    if cli.demo {
        ContextCatalog {
            active: "demo".to_owned(),
            contexts: vec![ContextInfo::synthetic("demo")],
        }
    } else {
        fallback_contexts_with(&cli.connection, cli.context.as_deref())
    }
}

async fn kubernetes_client(
    connection: &ConnectionOptions,
    context: Option<&str>,
) -> Result<(Client, String)> {
    let config = connection
        .config(context)
        .await
        .map_err(anyhow::Error::msg)?;
    let namespace = config.default_namespace.clone();
    let client = Client::try_from(config).map_err(|error| {
        anyhow::anyhow!(
            "create Kubernetes client: {}",
            connection.redact(&error.to_string())
        )
    })?;
    client.apiserver_version().await.map_err(|error| {
        anyhow::anyhow!(
            "authenticate Kubernetes client: {}",
            connection.redact(&error.to_string())
        )
    })?;
    Ok((client, namespace))
}

struct RunOptions {
    frame_interval: Duration,
    startup_command: Option<String>,
    use_config_default_view: bool,
    demo: bool,
    refresh: watch::Sender<Duration>,
    screen_dump_dir: Option<std::path::PathBuf>,
}

struct LocalServices {
    port_forwards: PortForwardManager,
    benchmarks: kux::benchmarks::BenchmarkManager,
    plugins: PluginProcessManager,
    dumps: dump_commands::DumpRuntime,
    transfers: TransferRuntime,
}

impl LocalServices {
    fn new(
        app: &App,
        options: &RunOptions,
    ) -> (
        Self,
        mpsc::Receiver<kux::port_forward::PortForwardEvent>,
        mpsc::Receiver<kux::benchmarks::BenchmarkEvent>,
    ) {
        let (events, receiver) = mpsc::channel(64);
        let (bench_events, bench_receiver) = mpsc::channel(64);
        (
            Self {
                port_forwards: PortForwardManager::with_connection(events, app.connection.clone()),
                benchmarks: kux::benchmarks::BenchmarkManager::new(bench_events),
                plugins: PluginProcessManager::new(),
                dumps: dump_commands::DumpRuntime::spawn(options.screen_dump_dir.clone()),
                transfers: TransferRuntime::new(options.demo),
            },
            receiver,
            bench_receiver,
        )
    }
}

impl RunOptions {
    fn apply_latest_config(
        &self,
        app: &mut App,
        runtime: &mut ConfigRuntime,
        scope: &ConfigContext,
        services: &mut LocalServices,
    ) -> bool {
        let Some(update) = runtime.updates.borrow_and_update().clone() else {
            return false;
        };
        if &update.scope != scope {
            return false;
        }
        app.apply_user_config(update.config);
        services.transfers.sync(app);
        self.update_refresh(app);
        true
    }

    fn update_refresh(&self, app: &App) {
        let configured = app.refresh_interval();
        self.refresh.send_if_modified(|current| {
            if *current == configured {
                return false;
            }
            *current = configured;
            true
        });
    }

    fn initial_command(&self, app: &App) -> Option<String> {
        self.startup_command.clone().or_else(|| {
            self.use_config_default_view
                .then(|| app.user_config.settings.default_view.clone())
                .flatten()
        })
    }
}

async fn run(
    mut app: App,
    requests: mpsc::Sender<WatchRequest>,
    mut snapshots: watch::Receiver<ResourceSnapshot>,
    actions: mpsc::Sender<ActionRequest>,
    mut action_results: mpsc::Receiver<ActionResult>,
    options: RunOptions,
) -> Result<()> {
    let (mut config_runtime, mut configured_context) = start_config_runtime(&mut app).await?;
    let mut pending_startup = options.initial_command(&app);
    options.update_refresh(&app);
    let mut terminal = TerminalGuard::enter()?;
    show_startup_splash(&mut terminal, &app).await?;
    let mut events = EventStream::new();
    let mut frames = skip_interval(options.frame_interval);
    let mut heartbeat = skip_interval(Duration::from_secs(1));
    let mut dirty = true;
    let (mut services, mut port_forward_events_rx, mut benchmark_events_rx) =
        LocalServices::new(&app, &options);

    let initial = pending_startup.take_if(|_| options.demo);
    if run_initial_command(
        initial,
        &mut app,
        &mut terminal,
        &requests,
        &actions,
        &mut services,
    )
    .await?
    {
        services.transfers.shutdown().await;
        return Ok(());
    }
    sync_config_context(&mut app, &config_runtime, &mut configured_context)?;
    services.transfers.sync(&app);

    loop {
        tokio::select! {
            event = events.next() => {
                let command = command_from_event(event, &mut app, &mut pending_startup)?;
                sync_config_context(&mut app, &config_runtime, &mut configured_context)?;
                if handle_app_command(command, &mut app, &mut terminal, &requests, &actions, &mut services).await? {
                    break;
                }
                dirty = true;
            }
            changed = snapshots.changed() => {
                if changed.is_err() {
                    break;
                }
                app.apply_snapshot(snapshots.borrow_and_update().clone());
                dirty = true;
            }
            Some(result) = action_results.recv() => {
                let command = apply_result_with_startup(&mut app, result, &mut pending_startup);
                sync_config_context(&mut app, &config_runtime, &mut configured_context)?;
                if handle_app_command(command, &mut app, &mut terminal, &requests, &actions, &mut services).await? {
                    break;
                }
                dirty = true;
            }
            changed = config_runtime.updates.changed() => {
                changed.context("configuration worker stopped")?;
                dirty |= options.apply_latest_config(&mut app, &mut config_runtime, &configured_context, &mut services);
            }
            Some(event) = port_forward_events_rx.recv() => {
                if let Some(notice) = services.port_forwards.handle_event(event) {
                    app.show_transient_notice(notice);
                }
                refresh_visible_port_forwards(&mut app, &services.port_forwards);
                dirty = true;
            }
            Some(event) = benchmark_events_rx.recv() => {
                app.show_transient_notice(services.benchmarks.handle_event(event));
                if app.kind.is_benchmarks() && handle_app_command(app.reload_current_view(), &mut app, &mut terminal, &requests, &actions, &mut services).await? {
                    break;
                }
                dirty = true;
            }
            Some(completion) = services.dumps.results.recv() => {
                let command = dump_commands::apply(&mut app, completion);
                if handle_app_command(command, &mut app, &mut terminal, &requests, &actions, &mut services).await? { break; }
                dirty = true;
            }
            Some(completion) = services.transfers.results.recv() => {
                app.apply_transfer_result(completion);
                dirty = true;
            }
            _ = heartbeat.tick() => {
                app.tick();
                plugin_commands::poll(&mut app, &mut services.plugins);
                let command = app.poll_view_refresh();
                handle_app_command(command, &mut app, &mut terminal, &requests, &actions, &mut services).await?;
                dirty = true;
            }
            _ = frames.tick(), if dirty => {
                terminal.terminal_mut().draw(|frame| ui::draw(frame, &mut app))?;
                dirty = false;
            }
        }
    }
    services.transfers.shutdown().await;
    Ok(())
}

fn skip_interval(duration: Duration) -> tokio::time::Interval {
    let mut interval = tokio::time::interval(duration);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval
}

async fn run_initial_command(
    initial: Option<String>,
    app: &mut App,
    terminal: &mut TerminalGuard,
    requests: &mpsc::Sender<WatchRequest>,
    actions: &mpsc::Sender<ActionRequest>,
    services: &mut LocalServices,
) -> Result<bool> {
    let Some(initial) = initial else {
        return Ok(false);
    };
    let command = app.run_command(&initial);
    handle_app_command(command, app, terminal, requests, actions, services).await
}

async fn show_startup_splash(terminal: &mut TerminalGuard, app: &App) -> Result<()> {
    if !app.user_config.settings.ui.splashless {
        terminal
            .terminal_mut()
            .draw(|frame| ui::draw_splash(frame, app))?;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Ok(())
}

fn command_from_event(
    event: Option<std::io::Result<Event>>,
    app: &mut App,
    startup: &mut Option<String>,
) -> Result<AppCommand> {
    match event {
        Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
            *startup = None;
            Ok(app.handle_key(key))
        }
        Some(Ok(Event::Mouse(mouse))) => {
            app.handle_mouse(mouse);
            Ok(AppCommand::None)
        }
        Some(Ok(Event::Resize(_, _))) => {
            if let Some(dialog) = app.rollback.dialog.as_mut() {
                dialog.scope_visible = false;
            }
            if let Some(dialog) = app.sanitize_dialog.as_mut() {
                dialog.scope_visible = false;
            }
            Ok(AppCommand::None)
        }
        Some(Err(error)) => Err(error).context("read terminal event"),
        None => Ok(AppCommand::Quit),
        _ => Ok(AppCommand::None),
    }
}

async fn start_config_runtime(app: &mut App) -> Result<(ConfigRuntime, ConfigContext)> {
    let paths = ConfigPaths::from_env().map_err(anyhow::Error::msg)?;
    let context = config_scope(app);
    let mut runtime = ConfigRuntime::spawn(paths, context.clone())?;
    runtime
        .updates
        .changed()
        .await
        .context("configuration worker stopped during startup")?;
    if let Some(update) = runtime.updates.borrow_and_update().clone() {
        app.apply_user_config(update.config);
    }
    Ok((runtime, context))
}

fn config_scope(app: &App) -> ConfigContext {
    ConfigContext {
        cluster: app
            .context_options
            .iter()
            .find(|context| context.name == app.context)
            .and_then(|context| context.cluster.clone()),
        context: app.context.clone(),
    }
}

fn sync_config_context(
    app: &mut App,
    runtime: &ConfigRuntime,
    previous: &mut ConfigContext,
) -> Result<()> {
    let current = config_scope(app);
    if current != *previous {
        app.begin_config_load();
        runtime
            .request_context(current.clone())
            .map_err(anyhow::Error::msg)?;
        *previous = current;
    }
    Ok(())
}

async fn handle_app_command(
    command: AppCommand,
    app: &mut App,
    terminal: &mut TerminalGuard,
    requests: &mpsc::Sender<WatchRequest>,
    actions: &mpsc::Sender<ActionRequest>,
    services: &mut LocalServices,
) -> Result<bool> {
    services.transfers.sync(app);
    if let Some(intent) = cancellation_before_command(app, &command) {
        actions
            .send(ActionRequest {
                generation: app.generation,
                intent,
            })
            .await
            .context("action worker stopped")?;
    }
    let port_forwards = &mut services.port_forwards;
    let plugins = &mut services.plugins;
    match command {
        AppCommand::Quit => return Ok(true),
        AppCommand::Request(view) => {
            requests
                .send(WatchRequest {
                    instance: view.instance,
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
        AppCommand::SaveLogs { name, body } => services.dumps.submit(
            app,
            DumpIntent::Save {
                name,
                payload: DumpPayload::Text {
                    body,
                    format: kux::screen_dumps::DumpFormat::Logs,
                },
            },
        ),
        AppCommand::Dump(intent) => services.dumps.submit(app, intent),
        AppCommand::EditDump(path) => {
            if edit_dump(app, terminal, &path).await?
                && let AppCommand::Dump(intent) = app.reload_screen_dumps()
            {
                services.dumps.submit(app, intent);
            }
        }
        AppCommand::Edit(target) => handle_edit(app, terminal, &target, actions).await?,
        AppCommand::Shell(target) => {
            let result = open_pod_shell(terminal, &app.connection, &app.context, &target)
                .await
                .context("restore terminal after pod shell")?;
            report_shell_result(app, &target, result);
        }
        AppCommand::NodeShell(target) => {
            let result = open_node_shell(terminal, &app.connection, &app.context, &target)
                .await
                .context("restore terminal after node shell")?;
            report_node_shell_result(app, &target, result);
        }
        AppCommand::Plugin(invocation) => {
            plugin_commands::execute(app, terminal, plugins, invocation).await?;
        }
        AppCommand::Attach(target) => attach_pod(app, terminal, &target).await?,
        AppCommand::StartPortForward(request) => match port_forwards.start(&app.context, request) {
            Ok(forward) => {
                app.show_transient_notice(format!(
                    "Starting port forward on {}:{}",
                    forward.address, forward.local_port
                ));
                refresh_visible_port_forwards(app, port_forwards);
            }
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
        AppCommand::RunBenchmark(request) => handle_benchmark_command(app, services, request),
        AppCommand::OpenUrl(url) => open_browser_url(&url),
        AppCommand::Action(action) => dispatch_app_action(app, action, actions).await?,
        AppCommand::Transfer(intent) => services.transfers.submit(app, intent),
        AppCommand::None => {}
    }
    stop_inactive_logs(app, actions).await?;
    Ok(false)
}

fn handle_benchmark_command(
    app: &mut App,
    services: &mut LocalServices,
    request: kux::benchmarks::BenchmarkRunRequest,
) {
    match services.benchmarks.toggle_or_start(request) {
        Ok(Some(target)) => {
            app.show_transient_notice(format!("Benchmarking {target} in background…"));
        }
        Ok(None) => app.show_transient_notice("Benchmark canceled"),
        Err(error) => app.show_transient_notice(format!("Benchmark failed: {error}")),
    }
}

fn open_browser_url(url: &str) {
    let bin = if cfg!(target_os = "macos") {
        "open"
    } else {
        "sensible-browser"
    };
    let _ = std::process::Command::new(bin)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn cancellation_before_command(app: &mut App, command: &AppCommand) -> Option<ActionIntent> {
    let references = app.cancel_references_before_command(command);
    let navigation = app.cancel_navigation_before_command(command);
    let cancellation = app.cancel_inactive_content_refresh();
    // Watch requests cancel the old read themselves. An explicit cancellation
    // here would have the new generation before the source has switched to it.
    if matches!(command, AppCommand::Request(_)) {
        None
    } else {
        references.or(navigation).or(cancellation)
    }
}

async fn dispatch_app_action(
    app: &mut App,
    action: ActionIntent,
    actions: &mpsc::Sender<ActionRequest>,
) -> Result<()> {
    if app.read_only && matches!(&action, ActionIntent::Rollback { .. }) {
        app.show_transient_notice("Rollback is disabled in read-only mode");
        return Ok(());
    }
    if app.read_only && matches!(&action, ActionIntent::SanitizePods { .. }) {
        app.show_transient_notice("Pod sanitization is disabled in read-only mode");
        return Ok(());
    }
    if app.read_only && matches!(&action, ActionIntent::SetImages { .. }) {
        app.show_transient_notice("Image updates are disabled in read-only mode");
        return Ok(());
    }
    let background_refresh = matches!(
        &action,
        ActionIntent::RefreshYaml { .. }
            | ActionIntent::RefreshDescribe { .. }
            | ActionIntent::StopContentRefresh
    );
    let keeps_overlay = matches!(
        &action,
        ActionIntent::Content { .. }
            | ActionIntent::ManagedYaml { .. }
            | ActionIntent::LoadImages { .. }
            | ActionIntent::LoadTransferPod { .. }
            | ActionIntent::LoadRollback { .. }
            | ActionIntent::LoadDeploymentSelector { .. }
            | ActionIntent::LoadReferences { .. }
            | ActionIntent::LoadBatchNavigation { .. }
    );
    let log_action = matches!(
        &action,
        ActionIntent::StartLogs { .. } | ActionIntent::StopLogs { .. }
    );
    actions
        .send(ActionRequest {
            generation: app.generation,
            intent: action,
        })
        .await
        .context("action worker stopped")?;
    if keeps_overlay {
        app.show_transient_notice("Loading…");
    } else if !log_action && !background_refresh {
        app.show_notice("Working…");
    }
    Ok(())
}

async fn stop_inactive_logs(app: &mut App, actions: &mpsc::Sender<ActionRequest>) -> Result<()> {
    if !app.is_logs_view()
        && let Some(view) = app.log_view.take()
    {
        actions
            .send(ActionRequest {
                generation: app.generation,
                intent: ActionIntent::StopLogs {
                    id: view.request.id,
                },
            })
            .await
            .context("action worker stopped")?;
    }
    Ok(())
}

async fn handle_edit(
    app: &mut App,
    terminal: &mut TerminalGuard,
    target: &ResourceTarget,
    actions: &mpsc::Sender<ActionRequest>,
) -> Result<()> {
    if app.is_live_content() {
        app.pause_content_refresh();
        dispatch_app_action(app, ActionIntent::StopContentRefresh, actions).await?;
    }
    let result = edit_resource(terminal, &app.connection, &app.context, target)
        .await
        .context("restore terminal after resource edit")?;
    match result {
        Ok(status) if status.success() => app.show_transient_notice(format!(
            "Edited {} {}",
            target.kind.qualified_name(),
            target.name
        )),
        Ok(status) => {
            app.show_transient_notice(format!("Edit failed: kubectl edit exited with {status}"));
        }
        Err(error) => app.show_transient_notice(format!("Edit failed: {error}")),
    }
    if let AppCommand::Action(action) = app.refresh_content_once() {
        dispatch_app_action(app, action, actions).await?;
    }
    Ok(())
}

async fn attach_pod(
    app: &mut App,
    terminal: &mut TerminalGuard,
    target: &PodShellTarget,
) -> Result<()> {
    terminal
        .suspend()
        .context("suspend terminal for pod attach")?;
    let result = Command::new("kubectl")
        .args(app.connection.kubectl_args(None))
        .args(pod_attach_args(&app.context, target))
        .status()
        .await;
    terminal
        .resume()
        .context("restore terminal after pod attach")?;
    match result {
        Ok(status) if status.success() => app.show_transient_notice(format!(
            "Attach closed for {}/{} ({})",
            target.namespace, target.pod, target.container
        )),
        Ok(status) => app.show_transient_notice(format!(
            "Attach failed: kubectl attach exited with {status}"
        )),
        Err(error) => app.show_transient_notice(format!("Attach failed: {error}")),
    }
    Ok(())
}

async fn edit_dump(
    app: &mut App,
    terminal: &mut TerminalGuard,
    path: &std::path::Path,
) -> Result<bool> {
    let editor = ["KUX_EDITOR", "KUBE_EDITOR", "EDITOR"]
        .into_iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| "vi".to_owned());
    terminal.suspend()?;
    let result = Command::new("sh")
        .arg("-c")
        .arg(format!("exec {editor} \"$1\""))
        .arg("kux-editor")
        .arg(path)
        .kill_on_drop(true)
        .status()
        .await;
    terminal.resume()?;
    match result {
        Ok(status) if status.success() => Ok(true),
        Ok(status) => {
            app.show_transient_notice(format!("Editor exited with {status}"));
            Ok(false)
        }
        Err(error) => {
            app.show_transient_notice(format!("Unable to open editor: {error}"));
            Ok(false)
        }
    }
}

fn refresh_visible_port_forwards(app: &mut App, manager: &PortForwardManager) {
    let forwards = manager.summaries(&app.context);
    app.refresh_port_forwards(forwards);
}

fn apply_action_result(app: &mut App, result: ActionResult) -> AppCommand {
    match result {
        ActionResult::BatchNavigation { id, target, result } => {
            return app.show_batch_navigation(id, &target, result);
        }
        ActionResult::References { id, target, result } => app.show_references(id, &target, result),
        ActionResult::DeploymentSelector {
            id,
            target,
            replica_sets,
            result,
        } => {
            return app.show_deployment_selector(id, &target, replica_sets, result);
        }
        ActionResult::RollbackLoaded { id, target, result } => {
            app.show_rollback(id, &target, result);
        }
        ActionResult::TransferPod { id, target, result } => {
            app.show_transfer_pod(id, &target, result);
        }
        ActionResult::Sanitized { namespace, result } => {
            app.show_sanitized(namespace.as_deref(), result);
        }
        ActionResult::TransientNotice(notice)
        | ActionResult::Notice(notice)
        | ActionResult::Error(notice) => {
            app.show_transient_notice(notice);
        }
        ActionResult::OpenedContent { id, target, result } => {
            app.apply_opened_content(id, &target, *result);
        }
        ActionResult::ImagesUpdated { result, .. } | ActionResult::RolledBack { result } => {
            app.show_transient_notice(result.unwrap_or_else(|error| error));
        }
        ActionResult::Images { id, target, result } => app.show_images(id, &target, result),
        ActionResult::RefreshedDescribe { id, target, result } => {
            app.apply_describe_refresh(id, &target, result);
        }
        ActionResult::RefreshedYaml { id, target, result } => {
            app.apply_yaml_refresh(id, &target, result);
        }
        ActionResult::Scoped { generation, result } => {
            if app.generation == generation {
                return apply_action_result(app, *result);
            }
        }
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
        ActionResult::ManagedYaml { id, target, result } => {
            app.show_managed_yaml(id, &target, result);
        }
        ActionResult::LogResolved { id, metadata } => app.show_log_resolution(id, metadata),
        ActionResult::LogBatch {
            id,
            lines,
            finished,
            error,
        } => {
            app.append_log_batch(id, lines, finished, error);
        }
        result @ (ActionResult::NamespaceOptions { .. }
        | ActionResult::ContextOptions { .. }
        | ActionResult::ResourceOptions { .. }
        | ActionResult::ServerVersion(_)
        | ActionResult::ClusterMetrics { .. }) => apply_discovery_result(app, result),
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
    }
    AppCommand::None
}

fn apply_discovery_result(app: &mut App, result: ActionResult) {
    match result {
        ActionResult::NamespaceOptions {
            context,
            namespaces,
        } => app.set_namespace_options(&context, namespaces),
        ActionResult::ContextOptions {
            context,
            contexts,
            notice,
        } => app.apply_context_options(&context, contexts, notice),
        ActionResult::ResourceOptions { context, resources } if app.context == context => {
            app.set_resource_options(resources);
        }
        ActionResult::ServerVersion(version) => app.set_server_version(version),
        ActionResult::ClusterMetrics { cpu, mem } => app.set_cluster_metrics(cpu, mem),
        _ => {}
    }
}

fn apply_result_with_startup(
    app: &mut App,
    result: ActionResult,
    startup: &mut Option<String>,
) -> AppCommand {
    let discovered =
        matches!(&result, ActionResult::ResourceOptions { context, .. } if context == &app.context);
    let command = apply_action_result(app, result);
    if discovered && let Some(startup) = startup.take() {
        return app.run_command(&startup);
    }
    command
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
    connection: &ConnectionOptions,
    context: &str,
    target: &ResourceTarget,
) -> Result<Result<std::process::ExitStatus, String>> {
    terminal
        .suspend()
        .context("suspend terminal for resource edit")?;
    let status = Command::new("kubectl")
        .args(connection.kubectl_args(None))
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

fn pod_attach_args(context: &str, target: &PodShellTarget) -> Vec<String> {
    vec![
        "attach".to_owned(),
        "-it".to_owned(),
        "--context".to_owned(),
        context.to_owned(),
        "--namespace".to_owned(),
        target.namespace.clone(),
        "--container".to_owned(),
        target.container.clone(),
        target.pod.clone(),
    ]
}

fn pod_shell_banner(target: &PodShellTarget) -> String {
    format!(
        "<<Kux-Shell>> Pod: {}/{} | Container: {}",
        target.namespace, target.pod, target.container
    )
}

async fn open_pod_shell(
    terminal: &mut TerminalGuard,
    connection: &ConnectionOptions,
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
        .args(connection.kubectl_args(None))
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

fn node_shell_pod_name(node: &str) -> String {
    let sanitized: String = node
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches('-');
    let suffix = if trimmed.is_empty() { "node" } else { trimmed };
    let name = format!("kux-shell-{suffix}");
    if name.len() > 63 {
        name[..63].trim_end_matches('-').to_owned()
    } else {
        name
    }
}

fn node_shell_volumes_and_mounts(
    settings: &kux::config::ShellPodSettings,
) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let mut mounts = vec![serde_json::json!({
        "name": "root-vol",
        "mountPath": "/host",
        "readOnly": true,
    })];
    let mut volumes = vec![serde_json::json!({
        "name": "root-vol",
        "hostPath": { "path": "/" }
    })];
    for extra in &settings.host_path_volume {
        if !extra.name.is_empty() && !extra.mount_path.is_empty() && !extra.host_path.is_empty() {
            mounts.push(serde_json::json!({
                "name": extra.name,
                "mountPath": extra.mount_path,
                "subPath": extra.sub_path,
                "readOnly": extra.read_only,
            }));
            volumes.push(serde_json::json!({
                "name": extra.name,
                "hostPath": { "path": extra.host_path }
            }));
        }
    }
    (volumes, mounts)
}

fn node_shell_overrides(target: &kux::app::NodeShellTarget) -> serde_json::Value {
    let command = if target.settings.command.is_empty() {
        vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "nsenter -t 1 -m -u -i -n -- /bin/sh || /bin/sh".to_owned(),
        ]
    } else {
        target.settings.command.clone()
    };
    let (volumes, mounts) = node_shell_volumes_and_mounts(&target.settings);
    serde_json::json!({
        "apiVersion": "v1",
        "spec": {
            "nodeName": target.node,
            "restartPolicy": "Never",
            "hostPID": true,
            "hostNetwork": true,
            "hostIPC": true,
            "tolerations": [{ "operator": "Exists" }],
            "volumes": volumes,
            "containers": [{
                "name": "kux-shell",
                "image": target.settings.image,
                "stdin": true,
                "tty": true,
                "command": command,
                "args": target.settings.args,
                "resources": { "limits": target.settings.limits },
                "volumeMounts": mounts,
                "securityContext": { "privileged": true }
            }]
        }
    })
}

fn node_shell_args(context: &str, target: &kux::app::NodeShellTarget) -> Vec<String> {
    let pod_name = node_shell_pod_name(&target.node);
    let overrides = node_shell_overrides(target).to_string();
    vec![
        "run".to_owned(),
        pod_name,
        "--rm".to_owned(),
        "-it".to_owned(),
        "--restart=Never".to_owned(),
        "--context".to_owned(),
        context.to_owned(),
        "--namespace".to_owned(),
        target.settings.namespace.clone(),
        "--image".to_owned(),
        target.settings.image.clone(),
        "--overrides".to_owned(),
        overrides,
    ]
}

fn node_shell_banner(target: &kux::app::NodeShellTarget) -> String {
    format!(
        "<<Kux-Node-Shell>> Node: {} | Image: {}",
        target.node, target.settings.image
    )
}

async fn open_node_shell(
    terminal: &mut TerminalGuard,
    connection: &ConnectionOptions,
    context: &str,
    target: &kux::app::NodeShellTarget,
) -> Result<Result<std::process::ExitStatus, String>> {
    terminal
        .suspend()
        .context("suspend terminal for node shell")?;
    terminal
        .show_interactive_banner(&node_shell_banner(target))
        .context("render node shell banner")?;
    let status = Command::new("kubectl")
        .args(connection.kubectl_args(None))
        .args(node_shell_args(context, target))
        .status()
        .await
        .map_err(|error| format!("Unable to run kubectl run for node shell: {error}"));
    terminal.resume()?;
    Ok(status)
}

fn report_node_shell_result(
    app: &mut App,
    target: &kux::app::NodeShellTarget,
    result: std::result::Result<std::process::ExitStatus, String>,
) {
    match result {
        Ok(status) if status.success() => {
            app.show_transient_notice(format!("Node shell closed for {}", target.node));
        }
        Ok(status) => {
            app.show_transient_notice(format!(
                "Node shell failed: kubectl run exited with {status}"
            ));
        }
        Err(error) => {
            app.show_transient_notice(format!("Node shell failed: {error}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Instant};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use kux::{
        app::{
            ActionIntent, App, AppCommand, ContentKind, Overlay, PodShellTarget, ResourceTarget,
        },
        model::{
            ResourceDescriptor, ResourceKind, ResourceOperations, ResourceRow, ResourceSnapshot,
            SourceState,
        },
        source::{ActionResult, ContainerImage, ImageSnapshot},
    };

    use super::{
        apply_action_result, copy_success_message, edit_args, pod_attach_args, pod_shell_args,
        pod_shell_banner,
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
    fn startup_command_waits_for_current_context_discovery_then_resolves_custom_resource() {
        let mut app = App::new(
            "prod",
            None,
            ResourceKind::Pods,
            false,
            vec!["prod".to_owned()],
        );
        let mut startup = Some("example.io/v1/widgets team-a".to_owned());
        let resource = target("example.io", "Widget", "widgets", Some("team-a")).kind;
        let old = ActionResult::ResourceOptions {
            context: "old".to_owned(),
            resources: vec![resource.clone()],
        };
        super::apply_result_with_startup(&mut app, old, &mut startup);
        assert!(startup.is_some());
        let current = ActionResult::ResourceOptions {
            context: "prod".to_owned(),
            resources: vec![resource.clone()],
        };
        let kux::app::AppCommand::Request(view) =
            super::apply_result_with_startup(&mut app, current, &mut startup)
        else {
            panic!("startup resource request")
        };
        assert_eq!(view.kind, resource);
        assert_eq!(view.namespace.as_deref(), Some("team-a"));
        assert!(startup.is_none());
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

    #[test]
    fn attach_args_preserve_context_namespace_and_container_without_a_shell() {
        let target = PodShellTarget {
            namespace: "prod".to_owned(),
            pod: "api-0".to_owned(),
            container: "sidecar".to_owned(),
        };
        assert_eq!(
            pod_attach_args("production", &target),
            [
                "attach",
                "-it",
                "--context",
                "production",
                "--namespace",
                "prod",
                "--container",
                "sidecar",
                "api-0"
            ]
        );
    }

    fn app_with_selected_pod() -> App {
        let mut app = App::new(
            "production",
            None,
            ResourceKind::Pods,
            false,
            vec!["production".to_owned()],
        );
        app.apply_snapshot(ResourceSnapshot {
            generation: app.generation,
            kind: app.kind.clone(),
            rows: Arc::new(vec![Arc::new(ResourceRow::new(
                "team-a", "selected", "Running", "1/1", "0", 0,
            ))]),
            table_columns: Arc::from([]),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });
        app
    }

    fn press(app: &mut App, character: char) -> AppCommand {
        app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
    }

    fn app_with_edited_image_form() -> App {
        let mut app = app_with_selected_pod();
        let AppCommand::Action(ActionIntent::LoadImages { id, target }) = press(&mut app, 'i')
        else {
            panic!("image form must load selected Pod");
        };
        let snapshot = ImageSnapshot {
            target: target.clone(),
            uid: "current-pod".to_owned(),
            resource_version: "1050".to_owned(),
            images: vec![ContainerImage {
                name: "api".to_owned(),
                image: "api:v1".to_owned(),
                init: false,
            }],
        };
        app.show_images(id, &target, Ok(snapshot));
        press(&mut app, '2');
        app
    }

    fn image_completion(generation: u64, result: Result<String, String>) -> ActionResult {
        let mut previous = target("", "Pod", "pods", Some("team-a"));
        previous.name = "previously-edited".to_owned();
        ActionResult::Scoped {
            generation,
            result: Box::new(ActionResult::ImagesUpdated {
                target: previous,
                result,
            }),
        }
    }

    #[test]
    fn old_generation_image_completions_leave_new_form_and_notice_untouched() {
        for result in [Ok("old success".to_owned()), Err("old failure".to_owned())] {
            let mut app = app_with_edited_image_form();
            app.generation += 1;
            app.show_transient_notice("current notice");
            let generation = app.generation;
            assert_eq!(
                apply_action_result(&mut app, image_completion(generation - 1, result)),
                AppCommand::None
            );
            assert_eq!(app.overlay, Overlay::ImageDialog);
            assert_eq!(app.notice.as_deref(), Some("current notice"));
            let dialog = app.image_editor.dialog.as_ref().unwrap();
            assert_eq!(dialog.values, ["api:v12"]);
            assert_eq!(dialog.snapshot.uid, "current-pod");
            assert_eq!(dialog.cursor, "api:v12".len());
        }
    }

    #[test]
    fn current_image_completions_preserve_a_newer_edited_form() {
        for result in [
            Ok("updated previous Pod".to_owned()),
            Err("previous Pod conflict".to_owned()),
        ] {
            let mut app = app_with_edited_image_form();
            let before = app.image_editor.dialog.as_ref().unwrap();
            let expected = (
                before.snapshot.clone(),
                before.values.clone(),
                before.focus,
                before.cursor,
            );
            let notice = result.clone().unwrap_or_else(|error| error);
            let generation = app.generation;
            assert_eq!(
                apply_action_result(&mut app, image_completion(generation, result)),
                AppCommand::None
            );
            assert_eq!(app.overlay, Overlay::ImageDialog);
            assert_eq!(app.notice.as_deref(), Some(notice.as_str()));
            let after = app.image_editor.dialog.as_ref().unwrap();
            assert_eq!(
                (
                    after.snapshot.clone(),
                    after.values.clone(),
                    after.focus,
                    after.cursor
                ),
                expected
            );
            assert!(app.image_editor.pending.is_none());
        }
    }

    fn app_with_pending_yaml_refresh() -> App {
        let mut app = app_with_selected_pod();
        assert!(matches!(
            press(&mut app, 'y'),
            AppCommand::Action(ActionIntent::Content {
                content: ContentKind::Yaml,
                ..
            })
        ));
        app.show_content(
            "Selected Pod",
            "original",
            None,
            None,
            None,
            ContentKind::Yaml,
        );
        assert!(matches!(
            press(&mut app, 'r'),
            AppCommand::Action(ActionIntent::RefreshYaml { .. })
        ));
        assert_eq!(app.poll_yaml_refresh(), AppCommand::None);
        app
    }

    #[test]
    fn navigation_clears_pending_refresh_without_sending_a_new_generation_cancellation() {
        let mut app = app_with_pending_yaml_refresh();
        let previous_generation = app.generation;
        let command = app.run_command("deployments");
        assert!(matches!(command, AppCommand::Request(_)));
        assert_ne!(app.generation, previous_generation);
        assert_eq!(super::cancellation_before_command(&mut app, &command), None);
        assert_eq!(app.cancel_inactive_content_refresh(), None);
    }

    #[test]
    fn leaving_content_without_navigation_still_cancels_once() {
        let mut app = app_with_pending_yaml_refresh();
        app.overlay = Overlay::None;
        assert_eq!(
            super::cancellation_before_command(&mut app, &AppCommand::None),
            Some(ActionIntent::StopContentRefresh)
        );
        assert_eq!(
            super::cancellation_before_command(&mut app, &AppCommand::None),
            None
        );
    }

    #[tokio::test]
    async fn readonly_image_submission_is_rejected_before_queueing() {
        let mut app = app_with_edited_image_form();
        let dialog = app.image_editor.dialog.as_ref().unwrap();
        let action = ActionIntent::SetImages {
            snapshot: dialog.snapshot.clone(),
            images: dialog.snapshot.images.clone(),
        };
        let (sender, mut requests) = tokio::sync::mpsc::channel(1);
        app.read_only = true;
        super::dispatch_app_action(&mut app, action, &sender)
            .await
            .unwrap();
        assert_eq!(
            requests.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        );
        assert!(app.notice.as_deref().unwrap().contains("read-only"));
    }

    #[tokio::test]
    async fn readonly_sanitize_submission_is_rejected_before_queueing() {
        for namespace in [None, Some("team".to_owned())] {
            let mut app = app_with_selected_pod();
            app.read_only = true;
            let (sender, mut requests) = tokio::sync::mpsc::channel(1);
            super::dispatch_app_action(&mut app, ActionIntent::SanitizePods { namespace }, &sender)
                .await
                .unwrap();
            assert_eq!(
                requests.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            );
            assert!(app.notice.as_deref().unwrap().contains("read-only"));
        }
    }

    fn rollback_snapshot() -> kux::source::RollbackSnapshot {
        kux::source::RollbackSnapshot {
            target: target("apps", "ReplicaSet", "replicasets", Some("team-a")),
            uid: "replica-set".to_owned(),
            resource_version: "4".to_owned(),
            deployment: "api".to_owned(),
            deployment_uid: "deployment".to_owned(),
            deployment_resource_version: "9".to_owned(),
            revision: 2,
            template: serde_json::json!({}),
            annotations: std::collections::BTreeMap::new(),
            already_matches: false,
        }
    }

    #[tokio::test]
    async fn readonly_rollback_submission_is_rejected_before_queueing() {
        let mut app = app_with_selected_pod();
        app.read_only = true;
        let action = ActionIntent::Rollback {
            snapshot: Box::new(rollback_snapshot()),
        };
        let (sender, mut requests) = tokio::sync::mpsc::channel(1);
        super::dispatch_app_action(&mut app, action, &sender)
            .await
            .unwrap();
        assert_eq!(
            requests.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        );
        assert!(app.notice.as_deref().unwrap().contains("read-only"));
    }

    #[test]
    fn resize_invalidates_destructive_scope_before_the_next_draw() {
        for rollback in [false, true] {
            let mut app = app_with_selected_pod();
            if rollback {
                let snapshot = rollback_snapshot();
                app.kind = snapshot.target.kind.clone();
                let AppCommand::Action(ActionIntent::LoadRollback { id, target }) =
                    app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL))
                else {
                    panic!("rollback preview");
                };
                app.show_rollback(id, &target, Ok(snapshot));
                app.rollback.dialog.as_mut().unwrap().confirm = true;
            } else {
                press(&mut app, 'z');
                let dialog = app.sanitize_dialog.as_mut().unwrap();
                dialog.confirmation = kux::app::SANITIZE_CONFIRMATION.to_owned();
                dialog.focus = 2;
            }
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
            terminal
                .draw(|frame| kux::ui::draw(frame, &mut app))
                .unwrap();
            assert!(if rollback {
                app.rollback.dialog.as_ref().unwrap().scope_visible
            } else {
                app.sanitize_dialog.as_ref().unwrap().scope_visible
            });
            super::command_from_event(
                Some(Ok(crossterm::event::Event::Resize(4, 2))),
                &mut app,
                &mut None,
            )
            .unwrap();
            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                AppCommand::None
            );
            assert!(app.notice.as_deref().unwrap().contains("Enlarge"));
            terminal
                .draw(|frame| kux::ui::draw(frame, &mut app))
                .unwrap();
            let command = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            assert!(if rollback {
                matches!(command, AppCommand::Action(ActionIntent::Rollback { .. }))
            } else {
                matches!(
                    command,
                    AppCommand::Action(ActionIntent::SanitizePods { .. })
                )
            });
        }
    }

    #[test]
    fn rollback_completion_is_scoped_and_preserves_a_newer_dialog() {
        let mut app = app_with_edited_image_form();
        let result = |generation| ActionResult::Scoped {
            generation,
            result: Box::new(ActionResult::RolledBack {
                result: Ok("Rollback completed".to_owned()),
            }),
        };
        let generation = app.generation;
        let notice = app.notice.clone();
        apply_action_result(&mut app, result(generation.wrapping_sub(1)));
        assert_eq!(app.notice, notice);
        apply_action_result(&mut app, result(generation));
        assert_eq!(app.notice.as_deref(), Some("Rollback completed"));
        assert_eq!(app.overlay, Overlay::ImageDialog);
        assert!(app.image_editor.dialog.is_some());
    }

    #[test]
    fn sanitize_completion_is_generation_scoped_and_preserves_new_confirmation() {
        let mut app = app_with_selected_pod();
        press(&mut app, 'z');
        let dialog = app.sanitize_dialog.clone();
        let result = |generation| ActionResult::Scoped {
            generation,
            result: Box::new(ActionResult::Sanitized {
                namespace: Some("team".to_owned()),
                result: Ok(kux::source::SanitizeReport {
                    deleted: 1,
                    eligible: 2,
                    failure: Some("team/second changed".to_owned()),
                }),
            }),
        };
        let generation = app.generation;
        apply_action_result(&mut app, result(generation - 1));
        assert_eq!(app.notice, None);
        assert_eq!(app.sanitize_dialog, dialog);
        apply_action_result(&mut app, result(generation));
        assert_eq!(app.sanitize_dialog, dialog);
        assert_eq!(app.overlay, Overlay::SanitizeDialog);
        assert!(app.notice.as_deref().unwrap().contains("Deleted 1 of 2"));
    }

    #[test]
    fn older_mutation_notices_and_errors_preserve_new_modal_work() {
        for result in [ActionResult::Notice, ActionResult::Error] {
            let mut app = app_with_selected_pod();
            let AppCommand::Action(ActionIntent::LoadTransferPod { id, target }) =
                press(&mut app, 't')
            else {
                panic!("load transfer Pod");
            };
            app.show_transfer_pod(
                id,
                &target,
                Ok(kux::transfer::TransferPod {
                    containers: vec!["api".to_owned()],
                }),
            );
            app.transfers.dialog.as_mut().unwrap().running = true;
            apply_action_result(&mut app, result("Earlier action finished".to_owned()));
            assert_eq!(app.overlay, Overlay::TransferDialog);
            assert!(app.transfers.dialog.as_ref().unwrap().running);
            assert_eq!(app.notice.as_deref(), Some("Earlier action finished"));

            let mut app = app_with_selected_pod();
            press(&mut app, 'z');
            let confirmation = app.sanitize_dialog.clone();
            apply_action_result(&mut app, result("Earlier action finished".to_owned()));
            assert_eq!(app.overlay, Overlay::SanitizeDialog);
            assert_eq!(app.sanitize_dialog, confirmation);

            let mut app = app_with_edited_image_form();
            apply_action_result(&mut app, result("Earlier action finished".to_owned()));
            assert_eq!(app.overlay, Overlay::ImageDialog);
            assert!(app.image_editor.dialog.is_some());
        }
    }

    #[test]
    fn node_shell_args_and_banner_build_privileged_host_pod_override() {
        let settings = kux::config::ShellPodSettings {
            image: "alpine:3.20".to_owned(),
            namespace: "kube-system".to_owned(),
            host_path_volume: vec![kux::config::HostPathMount {
                name: "var-log".to_owned(),
                mount_path: "/host/var/log".to_owned(),
                sub_path: String::new(),
                host_path: "/var/log".to_owned(),
                read_only: true,
            }],
            ..kux::config::ShellPodSettings::default()
        };
        let target = kux::app::NodeShellTarget {
            node: "Node.Example_01".to_owned(),
            settings,
        };
        assert_eq!(
            super::node_shell_pod_name(&target.node),
            "kux-shell-node-example-01"
        );
        assert_eq!(
            super::node_shell_banner(&target),
            "<<Kux-Node-Shell>> Node: Node.Example_01 | Image: alpine:3.20"
        );
        let args = super::node_shell_args("prod", &target);
        assert!(args.contains(&"--rm".to_owned()));
        assert!(args.contains(&"kube-system".to_owned()));
        assert!(args.contains(&"alpine:3.20".to_owned()));
        let overrides = super::node_shell_overrides(&target);
        assert_eq!(overrides["spec"]["nodeName"], "Node.Example_01");
        assert_eq!(overrides["spec"]["hostPID"], true);
        assert_eq!(overrides["spec"]["hostNetwork"], true);
        assert_eq!(overrides["spec"]["hostIPC"], true);
        assert_eq!(
            overrides["spec"]["containers"][0]["securityContext"]["privileged"],
            true
        );
        assert_eq!(overrides["spec"]["volumes"].as_array().unwrap().len(), 2);
    }
}
