//! Configuration I/O runs on a dedicated thread, with only the latest update
//! retained for the terminal loop.

use std::{
    io,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration,
};

use tokio::sync::watch;

use crate::config::{ConfigPaths, UserConfig};

#[path = "config_layers.rs"]
mod layers;
use layers::ConfigurationLayers;

const POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigContext {
    pub cluster: Option<String>,
    pub context: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigUpdate {
    pub scope: ConfigContext,
    pub config: UserConfig,
}

/// Owns a detached worker that stops when this runtime is dropped. Context
/// requests and results each occupy one slot, regardless of consumer speed.
#[derive(Debug)]
pub struct ConfigRuntime {
    requests: mpsc::SyncSender<()>,
    latest_scope: Arc<Mutex<ConfigContext>>,
    pub updates: watch::Receiver<Option<ConfigUpdate>>,
    // Dropping a JoinHandle detaches; filesystem I/O cannot block owner teardown.
    worker: thread::JoinHandle<()>,
}

impl ConfigRuntime {
    /// Start loading configuration without reading files on the calling thread.
    /// Await the first non-`None` update before using configuration-dependent
    /// behavior. Missing files still produce a complete default configuration.
    ///
    /// # Errors
    /// Returns the operating system error if the worker thread cannot start.
    pub fn spawn(paths: ConfigPaths, initial: ConfigContext) -> io::Result<Self> {
        let (requests, receiver) = mpsc::sync_channel(1);
        let (updates, update_receiver) = watch::channel(None);
        let latest_scope = Arc::new(Mutex::new(initial.clone()));
        let worker_scope = Arc::clone(&latest_scope);
        let worker = thread::Builder::new()
            .name("kux-config".to_owned())
            .spawn(move || run_worker(paths, initial, &worker_scope, &receiver, &updates))?;
        Ok(Self {
            requests,
            latest_scope,
            updates: update_receiver,
            worker,
        })
    }

    /// Request a new context without waiting for filesystem I/O. Rapid requests
    /// replace the pending scope; the worker publishes its scope with the result
    /// so the caller can reject stale updates after another context switch.
    ///
    /// # Errors
    /// Returns an error if the worker has stopped or its context lock is poisoned.
    pub fn request_context(&self, scope: ConfigContext) -> Result<(), String> {
        if self.worker.is_finished() {
            return Err("Configuration worker has stopped".to_owned());
        }
        *self
            .latest_scope
            .lock()
            .map_err(|_| "Configuration worker context lock is poisoned".to_owned())? = scope;
        match self.requests.try_send(()) {
            Ok(()) | Err(mpsc::TrySendError::Full(())) => Ok(()),
            Err(mpsc::TrySendError::Disconnected(())) => {
                Err("Configuration worker has stopped".to_owned())
            }
        }
    }
}

fn run_worker(
    paths: ConfigPaths,
    mut scope: ConfigContext,
    latest_scope: &Mutex<ConfigContext>,
    requests: &mpsc::Receiver<()>,
    updates: &watch::Sender<Option<ConfigUpdate>>,
) {
    let mut layers = ConfigurationLayers::new(paths, &scope);
    let config = layers.refresh(&scope, true).unwrap_or_default();
    if updates
        .send(Some(ConfigUpdate {
            scope: scope.clone(),
            config,
        }))
        .is_err()
    {
        return;
    }
    while !updates.is_closed() {
        let requested = match requests.recv_timeout(POLL_INTERVAL) {
            Ok(()) => {
                // The channel can hold at most one more wakeup. Consume it before
                // reading the latest scope without letting producers starve I/O.
                let _ = requests.try_recv();
                true
            }
            Err(mpsc::RecvTimeoutError::Timeout) => false,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        if updates.is_closed() {
            return;
        }
        let Ok(next_scope) = latest_scope.lock().map(|scope| scope.clone()) else {
            return;
        };
        let scope_changed = next_scope != scope;
        scope = next_scope;
        let config = layers.refresh(&scope, requested || scope_changed);
        if let Some(config) = config
            && updates
                .send(Some(ConfigUpdate {
                    scope: scope.clone(),
                    config,
                }))
                .is_err()
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "kux-config-runtime-{}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn paths(&self) -> ConfigPaths {
            ConfigPaths::new(self.0.join("config"), self.0.join("data"))
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scope(context: &str) -> ConfigContext {
        ConfigContext {
            cluster: Some("cluster".to_owned()),
            context: context.to_owned(),
        }
    }

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    async fn next_update(updates: &mut watch::Receiver<Option<ConfigUpdate>>) -> ConfigUpdate {
        tokio::time::timeout(Duration::from_secs(5), updates.changed())
            .await
            .expect("config update before timeout")
            .expect("worker alive");
        updates
            .borrow_and_update()
            .clone()
            .expect("loaded configuration")
    }

    async fn stop(runtime: ConfigRuntime) {
        let mut updates = runtime.updates.clone();
        updates.borrow_and_update();
        drop(runtime);
        tokio::time::timeout(Duration::from_secs(2), async {
            while updates.changed().await.is_ok() {
                updates.borrow_and_update();
            }
        })
        .await
        .expect("worker exits after owner drop");
    }

    #[tokio::test]
    async fn extension_files_reload_together_and_keep_last_valid_versions() {
        use ratatui::style::Color;

        let directory = TestDirectory::new();
        let paths = directory.paths();
        write(
            &paths.config_dir.join("config.yaml"),
            "kux:\n  ui:\n    skin: ocean\n",
        );
        let skin = paths.config_dir.join("skins/ocean.yaml");
        write(&skin, "k9s:\n  body:\n    fgColor: blue\n");
        let plugins = paths.config_dir.join("plugins.yaml");
        write(
            &plugins,
            "plugins:\n  inspect:\n    shortCut: v\n    description: Inspect\n    scopes: [all]\n    command: echo\n",
        );
        let mut runtime = ConfigRuntime::spawn(paths.clone(), scope("initial")).unwrap();
        let initial = next_update(&mut runtime.updates).await;
        assert_eq!(
            initial.config.theme.color_override("body.fgColor"),
            Some(Color::Rgb(0, 0, 255))
        );
        assert_eq!(initial.config.plugins.plugins["inspect"].command, "echo");
        write(&skin, "k9s:\n  body:\n    fgColor: unknown-color\n");
        write(&plugins, "plugins: [invalid]");
        let invalid = next_update(&mut runtime.updates).await;
        assert_eq!(invalid.config.theme, initial.config.theme);
        assert_eq!(
            invalid.config.plugins.plugins,
            initial.config.plugins.plugins
        );
        assert_eq!(invalid.config.diagnostics.len(), 2);
        write(&skin, "k9s:\n  body:\n    fgColor: '#123456'\n");
        write(
            &plugins,
            "plugins:\n  inspect:\n    shortCut: v\n    description: Inspect\n    scopes: [all]\n    command: printf\n",
        );
        let recovered = next_update(&mut runtime.updates).await;
        assert_eq!(
            recovered.config.theme.color_override("body.fgColor"),
            Some(Color::Rgb(0x12, 0x34, 0x56))
        );
        assert_eq!(
            recovered.config.plugins.plugins["inspect"].command,
            "printf"
        );
        assert_eq!(recovered.config.diagnostics.len(), 0);
        write(
            &paths.context_dir("cluster", "next").join("config.yaml"),
            "kux:\n  skin: default\n",
        );
        write(
            &paths.context_dir("cluster", "next").join("plugins.yaml"),
            "plugins:\n  inspect:\n    shortCut: v\n    description: Inspect\n    scopes: [all]\n    command: cat\n",
        );
        runtime.request_context(scope("next")).unwrap();
        let scoped = next_update(&mut runtime.updates).await;
        assert_eq!(scoped.scope, scope("next"));
        assert_eq!(scoped.config.plugins.plugins["inspect"].command, "cat");
        assert_eq!(scoped.config.theme, crate::theme::Skin::default());
        stop(runtime).await;
    }

    #[tokio::test]
    async fn absent_files_still_publish_initial_configuration_without_creating_files() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        let mut runtime = ConfigRuntime::spawn(paths.clone(), scope("initial")).unwrap();
        let update = next_update(&mut runtime.updates).await;
        assert_eq!(
            update,
            ConfigUpdate {
                scope: scope("initial"),
                config: UserConfig::default()
            }
        );
        assert!(!paths.config_dir.exists());
        assert!(!paths.data_dir.exists());
        assert!(!runtime.updates.has_changed().unwrap());
        stop(runtime).await;
    }

    #[tokio::test]
    async fn context_switch_loads_scoped_settings_and_tags_each_update() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        write(
            &paths.config_dir.join("aliases.yaml"),
            "aliases:\n  pp: pods\n",
        );
        write(
            &paths
                .context_dir("cluster", "production")
                .join("config.yaml"),
            "kux:\n  readOnly: true\n",
        );
        write(
            &paths
                .context_dir("other-cluster", "production")
                .join("aliases.yaml"),
            "aliases:\n  pp: pods isolated\n",
        );
        let mut runtime = ConfigRuntime::spawn(paths, scope("development")).unwrap();
        let initial = next_update(&mut runtime.updates).await;
        assert!(!initial.config.settings.read_only);
        runtime.request_context(scope("production")).unwrap();
        let production = next_update(&mut runtime.updates).await;
        assert_eq!(production.scope, scope("production"));
        assert!(production.config.settings.read_only);
        let other = ConfigContext {
            cluster: Some("other-cluster".to_owned()),
            context: "production".to_owned(),
        };
        runtime.request_context(other.clone()).unwrap();
        let isolated = next_update(&mut runtime.updates).await;
        assert_eq!(isolated.scope, other);
        assert!(!isolated.config.settings.read_only);
        assert_eq!(
            isolated.config.expand_command("pp").unwrap(),
            "pods isolated"
        );
        stop(runtime).await;
    }

    #[tokio::test]
    async fn polling_publishes_changes_and_retains_last_valid_malformed_file() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        let config_file = paths.config_dir.join("config.yaml");
        write(&config_file, "kux:\n  readOnly: true\n");
        let mut runtime = ConfigRuntime::spawn(paths, scope("production")).unwrap();
        assert!(
            next_update(&mut runtime.updates)
                .await
                .config
                .settings
                .read_only
        );
        write(&config_file, "kux:\n  readOnly: [broken\n");
        let malformed = next_update(&mut runtime.updates).await;
        assert!(malformed.config.settings.read_only);
        assert_eq!(malformed.config.diagnostics.len(), 1);
        assert_eq!(malformed.config.diagnostics[0].path, config_file);
        write(&config_file, "kux:\n  readOnly: false\n");
        let repaired = next_update(&mut runtime.updates).await;
        assert!(!repaired.config.settings.read_only);
        assert_eq!(repaired.config.diagnostics.len(), 0);
        stop(runtime).await;
    }

    #[tokio::test]
    async fn rapid_context_requests_and_updates_keep_only_latest_values() {
        let directory = TestDirectory::new();
        let mut runtime = ConfigRuntime::spawn(directory.paths(), scope("initial")).unwrap();
        next_update(&mut runtime.updates).await;
        for index in 0..1_000 {
            runtime
                .request_context(scope(&format!("context-{index}")))
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let update = next_update(&mut runtime.updates).await;
                if update.scope.context == "context-999" {
                    break;
                }
            }
        })
        .await
        .expect("latest scope is delivered");
        assert!(!runtime.updates.has_changed().unwrap());
        stop(runtime).await;
    }

    #[tokio::test]
    async fn explicit_requests_acknowledge_unchanged_and_coalesced_roundtrip_scopes() {
        let directory = TestDirectory::new();
        let mut runtime = ConfigRuntime::spawn(directory.paths(), scope("initial")).unwrap();
        next_update(&mut runtime.updates).await;
        runtime.request_context(scope("initial")).unwrap();
        assert_eq!(
            next_update(&mut runtime.updates).await.scope,
            scope("initial")
        );

        // Keep the worker from reading the pending slot until a complete context
        // roundtrip has coalesced back to its original scope.
        {
            let mut latest = runtime.latest_scope.lock().unwrap();
            *latest = scope("other");
            runtime.requests.try_send(()).unwrap();
            *latest = scope("initial");
            let _ = runtime.requests.try_send(());
        }
        assert_eq!(
            next_update(&mut runtime.updates).await.scope,
            scope("initial")
        );
        stop(runtime).await;
    }

    #[tokio::test]
    async fn dropping_update_receivers_stops_worker_even_with_request_sender_alive() {
        let directory = TestDirectory::new();
        let mut runtime = ConfigRuntime::spawn(directory.paths(), scope("initial")).unwrap();
        next_update(&mut runtime.updates).await;
        let (_, unrelated_receiver) = watch::channel(None);
        drop(std::mem::replace(&mut runtime.updates, unrelated_receiver));
        tokio::time::timeout(Duration::from_secs(3), async {
            while !runtime.worker.is_finished() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("worker exits when result channel closes");
        assert!(
            runtime
                .request_context(scope("closed"))
                .unwrap_err()
                .contains("stopped")
        );
    }
}
