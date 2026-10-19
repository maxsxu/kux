use std::{sync::Arc, time::Instant};

use super::*;
use crate::{
    app::{KeyCode, KeyEvent, KeyModifiers, ListMode},
    model::{ChildResourceData, ResourceRow, ResourceSnapshot, SourceState},
};

fn cron_job_descriptor() -> ResourceDescriptor {
    ResourceDescriptor::new(
        "batch",
        "v1",
        "batch/v1",
        "CronJob",
        "cronjobs",
        true,
        ResourceOperations::all(),
    )
    .with_short_names(["cj"])
}

fn app(cron_job: bool) -> App {
    let mut app = App::new(
        "selected-context",
        None,
        ResourceKind::Pods,
        false,
        vec!["selected-context".to_owned()],
    );
    app.kind = if cron_job {
        cron_job_descriptor()
    } else {
        job_descriptor()
    };
    app.field_selector = Some("metadata.name=selected".to_owned());
    app.label_selector = Some("environment=prod".to_owned());
    app.history[0] = app.current_view();
    app.apply_snapshot(ResourceSnapshot {
        generation: app.generation,
        kind: app.kind.clone(),
        rows: Arc::new(vec![
            Arc::new(ResourceRow::new("team", "selected", "Ready", "1/1", "", 0)),
            Arc::new(ResourceRow::new("team", "z-other", "Ready", "1/1", "", 0)),
        ]),
        table_columns: Arc::from([]),
        state: SourceState::Ready,
        revision: 1,
        emitted_at: Instant::now(),
    });
    app
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn pending(app: &mut App) -> (u64, ResourceTarget) {
    let AppCommand::Action(ActionIntent::LoadBatchNavigation { id, target }) =
        app.handle_key(key(KeyCode::Enter))
    else {
        panic!("batch Enter must read the selected resource before navigation");
    };
    (id, target)
}

fn result(cron_job: bool) -> BatchNavigationFilter {
    if cron_job {
        BatchNavigationFilter::CronJobJobs {
            owner_name: "selected".to_owned(),
        }
    } else {
        BatchNavigationFilter::JobPods {
            label_selector: Some("app=selected,tier in (web,worker),!debug".to_owned()),
        }
    }
}

#[test]
fn cronjob_and_job_enter_read_selected_resource_then_open_exact_child_scope() {
    for cron_job in [true, false] {
        let mut app = app(cron_job);
        app.read_only = true;
        app.query = "selected".to_owned();
        app.marks.insert(Arc::from("team/z-other"));
        let original = app.current_view();
        let (id, target) = pending(&mut app);
        assert_eq!(target.name, "selected");
        assert_eq!(target.namespace.as_deref(), Some("team"));
        assert_eq!(app.current_view(), original);
        assert_eq!(app.history.as_slice(), std::slice::from_ref(&original));
        let AppCommand::Request(view) =
            app.show_batch_navigation(id, &target, Ok(result(cron_job)))
        else {
            panic!("successful parent read must open its watched child list");
        };
        assert_eq!(view.context, "selected-context");
        assert_eq!(view.namespace.as_deref(), Some("team"));
        if cron_job {
            assert!(view.kind.is_job());
            assert_eq!(
                view.field_selector.as_deref(),
                Some("kux.ownerName=selected")
            );
            assert_eq!(view.label_selector, None);
        } else {
            assert_eq!(view.kind.known_kind(), Some(ResourceKind::Pods));
            assert_eq!(view.field_selector, None);
            assert_eq!(
                view.label_selector.as_deref(),
                Some("app=selected,tier in (web,worker),!debug")
            );
        }
        assert_eq!(app.query, "");
        assert!(app.marks.is_empty());
        assert!(app.navigation.pending.is_none());
        assert_eq!(app.history, [original.clone(), view.clone()]);
        app.apply_snapshot(ResourceSnapshot {
            generation: app.generation,
            kind: app.kind.clone(),
            rows: Arc::new(Vec::new()),
            table_columns: Arc::from([]),
            state: SourceState::Ready,
            revision: 2,
            emitted_at: Instant::now(),
        });
        assert_eq!(app.current_view(), view);
        assert!(app.selected_row().is_none());
        assert_eq!(
            app.handle_key(key(KeyCode::Esc)),
            AppCommand::Request(original)
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char(']'))),
            AppCommand::Request(view)
        );
    }
}

#[test]
fn compact_job_selector_does_not_skip_the_upstream_parent_read() {
    let mut app = app(false);
    let row = ResourceRow::new("team", "selected", "Ready", "1/1", "", 0).with_child_resource_data(
        ChildResourceData {
            kind: ResourceKind::Pods.into(),
            label_selector: Arc::from("stale=compact"),
        },
    );
    app.snapshot.rows = Arc::new(vec![Arc::new(row)]);
    let (id, target) = pending(&mut app);
    let AppCommand::Request(view) = app.show_batch_navigation(id, &target, Ok(result(false)))
    else {
        panic!("fresh selector opens Pods");
    };
    assert_eq!(
        view.label_selector.as_deref(),
        Some("app=selected,tier in (web,worker),!debug")
    );
}

#[test]
fn missing_and_explicit_empty_job_selectors_have_distinct_watched_scopes() {
    for selector in [None, Some(String::new())] {
        let mut app = app(false);
        let (id, target) = pending(&mut app);
        let AppCommand::Request(view) = app.show_batch_navigation(
            id,
            &target,
            Ok(BatchNavigationFilter::JobPods {
                label_selector: selector.clone(),
            }),
        ) else {
            panic!("both selector forms open a Pod list");
        };
        assert_eq!(view.label_selector, selector);
        assert_eq!(
            view.field_selector.as_deref(),
            if selector.is_none() {
                Some("kux.matchNone=true")
            } else {
                None
            }
        );
        app.apply_snapshot(ResourceSnapshot {
            generation: app.generation,
            kind: app.kind.clone(),
            rows: Arc::new(Vec::new()),
            table_columns: Arc::from([]),
            state: SourceState::Ready,
            revision: 2,
            emitted_at: Instant::now(),
        });
        assert!(app.is_resource_view());
        assert_eq!(app.current_view(), view);
        assert_eq!(app.history.len(), 2);
        assert!(app.selected_row().is_none());
    }
}

#[test]
fn missing_selection_or_namespace_never_starts_parent_read() {
    for cron_job in [false, true] {
        for no_rows in [false, true] {
            let mut app = app(cron_job);
            app.snapshot.rows = Arc::new(if no_rows {
                Vec::new()
            } else {
                vec![Arc::new(ResourceRow::new(
                    "-", "selected", "Ready", "1/1", "", 0,
                ))]
            });
            app.rebuild_filter(None);
            let original = app.current_view();
            assert_eq!(app.handle_key(key(KeyCode::Enter)), AppCommand::None);
            assert_eq!(app.current_view(), original);
            assert!(app.navigation.pending.is_none());
        }
    }
}

#[test]
fn deleted_denied_and_malformed_parents_leave_the_original_view() {
    for cron_job in [false, true] {
        for error in ["not found", "forbidden", "invalid selector"] {
            let mut app = app(cron_job);
            let original = app.current_view();
            let (id, target) = pending(&mut app);
            assert_eq!(
                app.show_batch_navigation(id, &target, Err(error.to_owned())),
                AppCommand::None
            );
            assert_eq!(app.current_view(), original);
            assert_eq!(app.history.len(), 1);
            assert_eq!(app.notice.as_deref(), Some(error));
            assert!(app.navigation.pending.is_none());
        }
    }
}

#[test]
fn cancellation_and_reopening_reject_old_success_and_failure() {
    for cron_job in [false, true] {
        let mut app = app(cron_job);
        let (old_id, target) = pending(&mut app);
        assert_eq!(
            app.handle_key(key(KeyCode::Esc)),
            AppCommand::Action(ActionIntent::StopContentRefresh)
        );
        let (id, _) = pending(&mut app);
        for outcome in [Ok(result(cron_job)), Err("stale failure".to_owned())] {
            assert_eq!(
                app.show_batch_navigation(old_id, &target, outcome),
                AppCommand::None
            );
            assert!(app.navigation.pending.is_some());
        }
        assert!(matches!(
            app.show_batch_navigation(id, &target, Ok(result(cron_job))),
            AppCommand::Request(_)
        ));
    }
}

#[test]
fn response_identity_and_filter_type_must_match_selected_batch_resource() {
    for cron_job in [false, true] {
        let mut app = app(cron_job);
        let (id, target) = pending(&mut app);
        let other = ResourceTarget {
            name: "other".to_owned(),
            ..target.clone()
        };
        assert_eq!(
            app.show_batch_navigation(id, &other, Ok(result(cron_job))),
            AppCommand::None
        );
        assert!(app.navigation.pending.is_some());
        assert_eq!(
            app.show_batch_navigation(id, &target, Ok(result(!cron_job))),
            AppCommand::None
        );
        assert!(app.navigation.pending.is_none());
        assert_eq!(app.history.len(), 1);
    }
    let mut app = app(true);
    let (id, target) = pending(&mut app);
    assert_eq!(
        app.show_batch_navigation(
            id,
            &target,
            Ok(BatchNavigationFilter::CronJobJobs {
                owner_name: "other".to_owned(),
            }),
        ),
        AppCommand::None
    );
    assert_eq!(app.history.len(), 1);
}

#[test]
fn context_generation_view_selection_overlay_and_input_invalidate_late_results() {
    for cron_job in [false, true] {
        for change in 0..7 {
            let mut app = app(cron_job);
            let (id, target) = pending(&mut app);
            match change {
                0 => app.context = "new-context".to_owned(),
                1 => app.generation += 1,
                2 => app.overlay = Overlay::Help,
                3 => app.input_mode = InputMode::Command,
                4 => app.selected = 1,
                5 => app.label_selector = Some("new=selector".to_owned()),
                _ => app.namespace = Some("new-team".to_owned()),
            }
            assert_eq!(
                app.show_batch_navigation(id, &target, Ok(result(cron_job))),
                AppCommand::None
            );
            assert!(app.navigation.pending.is_none());
            assert_eq!(app.history.len(), 1);
            assert!(app.notice.is_none());
        }
    }
}

#[test]
fn watch_deletion_settles_stale_navigation_without_erasing_an_unrelated_notice() {
    for cron_job in [false, true] {
        for unrelated in [false, true] {
            let mut app = app(cron_job);
            let (id, target) = pending(&mut app);
            if unrelated {
                app.show_transient_notice("Watch connection recovered");
            }
            app.apply_snapshot(ResourceSnapshot {
                generation: app.generation,
                kind: app.kind.clone(),
                rows: Arc::new(Vec::new()),
                table_columns: Arc::from([]),
                state: SourceState::Ready,
                revision: 2,
                emitted_at: Instant::now(),
            });
            assert_eq!(
                app.show_batch_navigation(id, &target, Ok(result(cron_job))),
                AppCommand::None
            );
            assert!(app.navigation.pending.is_none());
            assert_eq!(app.history.len(), 1);
            assert_eq!(
                app.notice.as_deref(),
                unrelated.then_some("Watch connection recovered")
            );
        }
    }
}

#[test]
fn batch_cancellation_preserves_own_load_and_precedes_unrelated_actions() {
    for cron_job in [false, true] {
        let mut app = app(cron_job);
        let (id, target) = pending(&mut app);
        let own = AppCommand::Action(ActionIntent::LoadBatchNavigation {
            id,
            target: target.clone(),
        });
        assert_eq!(app.cancel_navigation_before_command(&own), None);
        assert_eq!(
            app.cancel_navigation_before_command(&AppCommand::None),
            None
        );
        assert_eq!(
            app.cancel_navigation_before_command(&AppCommand::Action(ActionIntent::LoadImages {
                id: 99,
                target: target.clone(),
            })),
            Some(ActionIntent::StopContentRefresh)
        );
        assert_eq!(
            app.show_batch_navigation(id, &target, Ok(result(cron_job))),
            AppCommand::None
        );
    }
}

#[test]
fn deployment_and_batch_completions_cannot_satisfy_each_others_pending_loads() {
    let mut app = app(true);
    let (id, target) = pending(&mut app);
    assert_eq!(
        app.show_deployment_selector(id, &target, false, Ok("app=other".to_owned())),
        AppCommand::None
    );
    assert!(app.navigation.pending.is_some());
    assert!(matches!(
        app.show_batch_navigation(id, &target, Ok(result(true))),
        AppCommand::Request(_)
    ));
    app.kind = ResourceKind::Deployments.into();
    app.snapshot.kind = app.kind.clone();
    app.snapshot.rows = Arc::new(vec![Arc::new(ResourceRow::new(
        "team", "selected", "Ready", "1/1", "", 0,
    ))]);
    app.rebuild_filter(None);
    let AppCommand::Action(ActionIntent::LoadDeploymentSelector { id, target, .. }) =
        app.open_deployment_pods()
    else {
        panic!("Deployment load");
    };
    assert_eq!(
        app.show_batch_navigation(id, &target, Ok(result(true))),
        AppCommand::None
    );
    assert!(app.navigation.pending.is_some());
}

#[test]
fn only_exact_namespaced_batch_v1_kinds_use_batch_navigation() {
    for cron_job in [false, true] {
        for mutation in 0..5 {
            let mut app = app(cron_job);
            match mutation {
                0 => app.kind.group = Arc::from("example.test"),
                1 => app.kind.version = Arc::from("v1beta1"),
                2 => app.kind.kind = Arc::from("Other"),
                3 => app.kind.plural = Arc::from("other"),
                _ => app.kind.namespaced = false,
            }
            assert_eq!(app.open_batch_children(), AppCommand::None);
            assert!(!matches!(
                app.handle_key(key(KeyCode::Enter)),
                AppCommand::Action(ActionIntent::LoadBatchNavigation { .. })
            ));
        }
        let mut app = app(cron_job);
        app.list_mode = ListMode::Containers {
            namespace: "team".to_owned(),
            pod: "parent".to_owned(),
        };
        assert_eq!(app.open_batch_children(), AppCommand::None);
    }
}

#[test]
fn cronjob_uses_the_discovered_job_descriptor() {
    let mut app = app(true);
    let discovered = job_descriptor().with_short_names(["job", "jobs"]);
    app.resource_options.push(discovered.clone());
    let (id, target) = pending(&mut app);
    let AppCommand::Request(view) = app.show_batch_navigation(id, &target, Ok(result(true))) else {
        panic!("Job view");
    };
    assert_eq!(view.kind, discovered);
}

#[test]
fn cronjob_child_jobs_start_youngest_first_regardless_of_parent_sort() {
    let mut app = app(true);
    app.sort_field = SortField::Status;
    app.sort_descending = false;
    let (id, target) = pending(&mut app);
    assert!(matches!(
        app.show_batch_navigation(id, &target, Ok(result(true))),
        AppCommand::Request(_)
    ));
    assert_eq!(app.sort_field, SortField::Age);
    assert!(app.sort_descending);
    app.apply_snapshot(ResourceSnapshot {
        generation: app.generation,
        kind: app.kind.clone(),
        rows: Arc::new(vec![
            Arc::new(ResourceRow::new("team", "old-job", "Ready", "1/1", "", 1)),
            Arc::new(ResourceRow::new("team", "new-job", "Ready", "1/1", "", 2)),
        ]),
        table_columns: Arc::from([]),
        state: SourceState::Ready,
        revision: 2,
        emitted_at: Instant::now(),
    });
    assert_eq!(app.selected_row().unwrap().name.as_ref(), "new-job");
}
