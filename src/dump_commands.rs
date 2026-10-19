use kux::{
    app::{App, AppCommand, DumpIntent, DumpPayload, Overlay},
    screen_dumps::{DumpFormat, ScreenDumpEntry, ScreenDumpStore},
};
use std::path::PathBuf;
use tokio::sync::mpsc;

struct Request {
    generation: u64,
    context: String,
    cluster: Option<String>,
    config_root: Option<PathBuf>,
    intent: DumpIntent,
}

pub(super) struct Completion {
    generation: u64,
    context: String,
    outcome: Outcome,
}

enum Outcome {
    Listed {
        id: u64,
        entries: Vec<ScreenDumpEntry>,
        notice: Option<String>,
    },
    Preview {
        id: u64,
        name: String,
        body: String,
    },
    Edit {
        id: u64,
        path: PathBuf,
    },
    Saved(PathBuf),
    Failed {
        id: Option<u64>,
        message: String,
    },
}

pub(super) struct DumpRuntime {
    requests: mpsc::Sender<Request>,
    pub results: mpsc::Receiver<Completion>,
    task: tokio::task::JoinHandle<()>,
}

impl DumpRuntime {
    pub fn spawn(root: Option<PathBuf>) -> Self {
        let (requests, mut queued_requests) = mpsc::channel::<Request>(4);
        let (results, receiver) = mpsc::channel(4);
        let task = tokio::spawn(async move {
            while let Some(request) = queued_requests.recv().await {
                let root = root.clone();
                let generation = request.generation;
                let context = request.context.clone();
                let result = tokio::task::spawn_blocking(move || perform(root, request)).await;
                let outcome = result.unwrap_or_else(|error| Outcome::Failed {
                    id: None,
                    message: format!("Screen-dump worker failed: {error}"),
                });
                if results
                    .send(Completion {
                        generation,
                        context,
                        outcome,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            requests,
            results: receiver,
            task,
        }
    }

    pub fn submit(&self, app: &mut App, intent: DumpIntent) {
        if matches!(intent, DumpIntent::Delete { .. }) && app.read_only {
            app.dumps.loading = false;
            app.show_transient_notice("Dump deletion is disabled in read-only mode");
            return;
        }
        let cluster = app.connection.cluster.clone().or_else(|| {
            app.context_options
                .iter()
                .find(|context| context.name == app.context)
                .and_then(|context| context.cluster.clone())
        });
        let config_root = app.user_config.settings.screen_dump_dir.clone();
        let request = Request {
            generation: app.generation,
            context: app.context.clone(),
            cluster,
            config_root,
            intent,
        };
        match self.requests.try_send(request) {
            Ok(()) => {}
            Err(error) => {
                app.dumps.loading = false;
                app.show_transient_notice(format!("Screen dump was not queued: {error}"));
            }
        }
    }
}

impl Drop for DumpRuntime {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn perform(root: Option<PathBuf>, request: Request) -> Outcome {
    let id = match &request.intent {
        DumpIntent::List { id }
        | DumpIntent::Preview { id, .. }
        | DumpIntent::Edit { id, .. }
        | DumpIntent::Delete { id, .. } => Some(*id),
        DumpIntent::Save { .. } => None,
    };
    let result = (|| {
        let store = match root.or(request.config_root) {
            Some(root) => ScreenDumpStore::new(root, request.cluster.as_deref(), &request.context)?,
            None => ScreenDumpStore::from_default(request.cluster.as_deref(), &request.context)?,
        };
        match request.intent {
            DumpIntent::List { id } => Ok(Outcome::Listed {
                id,
                entries: store.list()?,
                notice: None,
            }),
            DumpIntent::Preview { id, entry } => Ok(Outcome::Preview {
                id,
                name: entry.name.clone(),
                body: store.read(&entry)?,
            }),
            DumpIntent::Edit { id, entry } => {
                // Validate the selected file's identity before passing its path to the editor.
                let _ = store.read(&entry)?;
                Ok(Outcome::Edit {
                    id,
                    path: entry.path,
                })
            }
            DumpIntent::Delete { id, entry } => {
                store.delete(&entry)?;
                Ok(Outcome::Listed {
                    id,
                    entries: store.list()?,
                    notice: Some(format!("Deleted {}", entry.name)),
                })
            }
            DumpIntent::Save { name, payload } => {
                let path = match payload {
                    DumpPayload::Text { body, format } => store.save(&name, &body, format)?,
                    DumpPayload::Table(table) => {
                        store.save_with(&name, DumpFormat::Csv, |file| table.write_csv(file))?
                    }
                };
                Ok(Outcome::Saved(path))
            }
        }
    })();
    result.unwrap_or_else(|error: std::io::Error| Outcome::Failed {
        id,
        message: format!("Screen dump failed: {error}"),
    })
}

pub(super) fn apply(app: &mut App, completion: Completion) -> AppCommand {
    if app.context != completion.context || app.generation != completion.generation {
        return AppCommand::None;
    }
    match completion.outcome {
        Outcome::Listed {
            id,
            entries,
            notice,
        } => {
            app.show_screen_dumps(id, entries);
            if id == app.dumps.request_id
                && app.overlay == Overlay::ScreenDumps
                && let Some(notice) = notice
            {
                app.show_transient_notice(notice);
            }
        }
        Outcome::Preview { id, name, body } => app.show_screen_dump_content(id, name, body),
        Outcome::Edit { id, path } if id == app.dumps.request_id => {
            app.dumps.loading = false;
            if app.overlay == Overlay::ScreenDumps {
                return AppCommand::EditDump(path);
            }
        }
        Outcome::Saved(path) => app.show_transient_notice(format!("Saved to {}", path.display())),
        Outcome::Failed {
            id: Some(id),
            message,
        } => app.screen_dump_error(id, message),
        Outcome::Failed { id: None, message } => app.show_transient_notice(message),
        Outcome::Edit { .. } => {}
    }
    AppCommand::None
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use kux::model::ResourceKind;

    use super::*;

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Directory(PathBuf);

    impl Directory {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                "kux-dump-runtime-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            )))
        }
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn app() -> App {
        let mut app = App::new(
            "demo",
            None,
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        app.overlay = Overlay::ScreenDumps;
        app.dumps.request_id = 42;
        app.dumps.loading = true;
        app
    }

    fn isolated_queue(capacity: usize) -> (DumpRuntime, mpsc::Receiver<Request>) {
        let (requests, queued_requests) = mpsc::channel(capacity);
        let (_, results) = mpsc::channel(1);
        let task = tokio::spawn(std::future::pending());
        (
            DumpRuntime {
                requests,
                results,
                task,
            },
            queued_requests,
        )
    }

    #[test]
    fn completions_from_an_old_context_or_generation_cannot_replace_the_browser() {
        let mut app = app();
        app.notice = Some("current request".to_owned());
        for (context, generation) in [
            ("other-context".to_owned(), app.generation),
            (app.context.clone(), app.generation.wrapping_sub(1)),
        ] {
            let result = apply(
                &mut app,
                Completion {
                    context,
                    generation,
                    outcome: Outcome::Listed {
                        id: 42,
                        entries: Vec::new(),
                        notice: Some("stale".to_owned()),
                    },
                },
            );
            assert_eq!(result, AppCommand::None);
            assert!(app.dumps.loading);
            assert_eq!(app.notice.as_deref(), Some("current request"));
        }
    }

    #[test]
    fn editing_requires_the_current_request_and_open_dump_browser() {
        let mut app = app();
        let context = app.context.clone();
        let generation = app.generation;
        let make_completion = |id| Completion {
            context: context.clone(),
            generation,
            outcome: Outcome::Edit {
                id,
                path: PathBuf::from("/tmp/selected-dump.csv"),
            },
        };
        assert_eq!(apply(&mut app, make_completion(41)), AppCommand::None);
        assert!(app.dumps.loading);
        app.overlay = Overlay::None;
        assert_eq!(apply(&mut app, make_completion(42)), AppCommand::None);
        assert!(!app.dumps.loading);
        app.overlay = Overlay::ScreenDumps;
        assert_eq!(
            apply(&mut app, make_completion(42)),
            AppCommand::EditDump(PathBuf::from("/tmp/selected-dump.csv"))
        );
    }

    #[tokio::test]
    async fn readonly_rejects_dump_deletion_before_queueing_work() {
        let directory = Directory::new();
        let store = ScreenDumpStore::new(directory.0.clone(), None, "demo").unwrap();
        let path = store
            .save("selected", "preserved body", DumpFormat::Text)
            .unwrap();
        let entry = store.list().unwrap().pop().unwrap();
        let (runtime, mut queued_requests) = isolated_queue(1);
        let mut app = app();
        app.read_only = true;
        runtime.submit(&mut app, DumpIntent::Delete { id: 42, entry });
        assert_eq!(
            queued_requests.try_recv().err(),
            Some(mpsc::error::TryRecvError::Empty)
        );
        assert!(!app.dumps.loading);
        assert!(app.notice.as_deref().unwrap().contains("read-only"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "preserved body");
    }

    #[tokio::test]
    async fn full_queue_reports_backpressure_and_clears_pending_state() {
        let (runtime, mut queued_requests) = isolated_queue(1);
        let mut app = app();
        runtime.submit(&mut app, DumpIntent::List { id: 42 });
        runtime.submit(&mut app, DumpIntent::List { id: 43 });
        assert!(!app.dumps.loading);
        assert!(app.notice.as_deref().unwrap().contains("not queued"));
        assert!(matches!(
            queued_requests.try_recv().unwrap().intent,
            DumpIntent::List { id: 42 }
        ));
        assert_eq!(
            queued_requests.try_recv().err(),
            Some(mpsc::error::TryRecvError::Empty)
        );
    }
}
