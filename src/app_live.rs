use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::{
    ActionIntent, App, AppCommand, ContentKind, Overlay, ResourceTarget, UiFlags, YamlContent,
};
use crate::source::ActionResult;

#[cfg(test)]
#[path = "app_live_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "app_managed_tests.rs"]
mod managed_tests;

#[cfg(test)]
#[path = "app_initial_tests.rs"]
mod initial_tests;

pub(super) const LIVE_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const RETRY_WINDOW: Duration = Duration::from_secs(120);

#[derive(Debug)]
pub(super) struct LiveRefreshState {
    pub id: u64,
    pub pending: bool,
    pub due: Instant,
    opening: Option<ContentKind>,
    enabled: bool,
    exhausted: bool,
    epoch: Instant,
    backoff: Duration,
}

impl LiveRefreshState {
    pub fn new() -> Self {
        Self {
            id: 0,
            pending: false,
            due: Instant::now(),
            opening: None,
            enabled: false,
            exhausted: false,
            epoch: Instant::now(),
            backoff: LIVE_REFRESH_INTERVAL,
        }
    }

    fn restart(&mut self, enabled: bool) {
        self.id = self.id.wrapping_add(1);
        self.pending = false;
        self.opening = None;
        self.enabled = enabled;
        self.exhausted = false;
        self.reset_backoff();
    }

    fn reset_backoff(&mut self) {
        self.exhausted = false;
        self.epoch = Instant::now();
        self.backoff = LIVE_REFRESH_INTERVAL;
        self.due = self.epoch + LIVE_REFRESH_INTERVAL;
    }

    fn retry(&mut self, now: Instant, jitter: f64) -> bool {
        let delay = self.backoff.mul_f64(jitter.clamp(0.5, 1.5));
        self.backoff = self.backoff.mul_f64(1.5).min(Duration::from_secs(60));
        self.exhausted = now.saturating_duration_since(self.epoch) + delay > RETRY_WINDOW;
        self.due = now + delay;
        !self.exhausted
    }
}

impl App {
    pub(super) fn request_content(
        &mut self,
        target: ResourceTarget,
        content: ContentKind,
    ) -> AppCommand {
        self.show_content(
            format!("{content:?} · {}", target.name),
            "Loading…",
            None,
            None,
            None,
            content,
        );
        self.content_target = Some(target.clone());
        self.yaml_refresh.pending = true;
        self.yaml_refresh.opening = Some(content);
        AppCommand::Action(ActionIntent::Content {
            id: self.yaml_refresh.id,
            target,
            content,
        })
    }

    pub fn apply_opened_content(&mut self, id: u64, target: &ResourceTarget, result: ActionResult) {
        let Some(kind) = self.content_kind else {
            return;
        };
        if !matches!(
            kind,
            ContentKind::Yaml
                | ContentKind::Describe
                | ContentKind::SecretData
                | ContentKind::HelmValues
        ) || self.yaml_refresh.opening != Some(kind)
            || !self.accepts_refresh(id, target, kind)
        {
            return;
        }
        let label = match kind {
            ContentKind::Yaml => "YAML",
            ContentKind::Describe => "Describe",
            ContentKind::HelmValues => "Values",
            _ => "Content",
        };
        match result {
            ActionResult::Content {
                title,
                body,
                managed_fields,
                decoded_body,
                decoded_managed_fields,
                content,
            } if content == kind => {
                self.content_title = title;
                self.content_body = body;
                self.content_managed_fields = managed_fields;
                self.content_decoded_body = decoded_body;
                self.content_decoded_managed_fields = decoded_managed_fields;
                self.refresh_finished(label, None);
                if self.notice.as_deref() == Some("Loading…") {
                    self.notice = None;
                }
            }
            ActionResult::Error(error) => {
                self.content_body.clear();
                self.refresh_finished(label, Some(error));
            }
            _ => {}
        }
    }

    #[must_use]
    pub fn is_live_content(&self) -> bool {
        self.overlay == Overlay::Content
            && matches!(
                self.content_kind,
                Some(ContentKind::Yaml | ContentKind::Describe)
            )
    }

    #[must_use]
    pub fn content_auto_refresh(&self) -> bool {
        self.is_live_content() && self.yaml_refresh.enabled
    }

    pub(super) fn reset_live_refresh(&mut self, kind: ContentKind) {
        self.yaml_refresh.restart(
            self.user_config.settings.live_view_auto_refresh
                && matches!(kind, ContentKind::Yaml | ContentKind::Describe),
        );
    }

    pub(super) fn toggle_content_refresh(&mut self) -> AppCommand {
        let enabled = !self.yaml_refresh.enabled;
        if self.yaml_refresh.opening.is_some() {
            self.yaml_refresh.enabled = enabled;
        } else {
            self.yaml_refresh.restart(enabled);
        }
        self.show_transient_notice(if enabled {
            "Auto-refresh is enabled"
        } else {
            "Auto-refresh is disabled"
        });
        if self.yaml_refresh.opening.is_some() {
            AppCommand::None
        } else if enabled {
            self.yaml_refresh.due = Instant::now();
            self.poll_yaml_refresh()
        } else {
            AppCommand::Action(ActionIntent::StopContentRefresh)
        }
    }

    /// Return a cancellation when leaving a view with a pending content read.
    pub fn cancel_inactive_content_refresh(&mut self) -> Option<ActionIntent> {
        if self.overlay != Overlay::Content && self.yaml_refresh.pending {
            self.yaml_refresh.restart(false);
            Some(ActionIntent::StopContentRefresh)
        } else {
            None
        }
    }

    pub fn poll_yaml_refresh(&mut self) -> AppCommand {
        if !self.is_live_content()
            || !self.yaml_refresh.enabled
            || self.yaml_refresh.exhausted
            || self.yaml_refresh.pending
            || Instant::now() < self.yaml_refresh.due
        {
            return AppCommand::None;
        }
        self.refresh_content_once()
    }

    pub fn pause_content_refresh(&mut self) {
        self.yaml_refresh.id = self.yaml_refresh.id.wrapping_add(1);
        self.yaml_refresh.pending = false;
        self.yaml_refresh.opening = None;
    }

    /// Fetch the opened resource once without changing auto-refresh or view state.
    pub fn refresh_content_once(&mut self) -> AppCommand {
        if !self.is_live_content() {
            return AppCommand::None;
        }
        let Some(target) = self.content_target.clone() else {
            return AppCommand::None;
        };
        self.yaml_refresh.id = self.yaml_refresh.id.wrapping_add(1);
        self.yaml_refresh.pending = true;
        self.yaml_refresh.opening = None;
        AppCommand::Action(if self.content_kind == Some(ContentKind::Describe) {
            ActionIntent::RefreshDescribe {
                id: self.yaml_refresh.id,
                target,
            }
        } else {
            ActionIntent::RefreshYaml {
                id: self.yaml_refresh.id,
                target,
                include_managed_fields: self
                    .content_managed_fields
                    .as_ref()
                    .is_some_and(|body| !body.is_empty()),
            }
        })
    }

    fn accepts_refresh(&self, id: u64, target: &ResourceTarget, kind: ContentKind) -> bool {
        self.overlay == Overlay::Content
            && self.content_kind == Some(kind)
            && self.content_target.as_ref() == Some(target)
            && self.yaml_refresh.id == id
            && self.yaml_refresh.pending
    }

    pub fn show_managed_yaml(
        &mut self,
        id: u64,
        target: &ResourceTarget,
        result: Result<(String, Option<String>), String>,
    ) {
        if !self.accepts_refresh(id, target, ContentKind::Yaml) {
            return;
        }
        match result {
            Ok((body, decoded_body)) => {
                self.content_managed_fields = Some(body);
                self.content_decoded_managed_fields = decoded_body;
                self.ui.set(UiFlags::MANAGED_FIELDS, true);
                self.refresh_finished("YAML", None);
                self.notice = None;
            }
            Err(error) => self.refresh_finished("YAML", Some(error)),
        }
    }

    fn refresh_finished(&mut self, label: &str, error: Option<String>) {
        self.yaml_refresh.pending = false;
        self.yaml_refresh.opening = None;
        if let Some(error) = error {
            let entropy = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
                % 1_000_001;
            let retrying = self
                .yaml_refresh
                .retry(Instant::now(), 0.5 + f64::from(entropy) / 1_000_000.0);
            let suffix = if retrying {
                ""
            } else {
                " (auto-refresh stopped after repeated failures; toggle r off/on to retry)"
            };
            self.show_transient_notice(format!("{label} refresh failed: {error}{suffix}"));
        } else {
            self.yaml_refresh.reset_backoff();
            if self
                .notice
                .as_deref()
                .is_some_and(|notice| notice.starts_with(&format!("{label} refresh failed:")))
            {
                self.notice = None;
            }
        }
    }

    pub fn apply_yaml_refresh(
        &mut self,
        id: u64,
        target: &ResourceTarget,
        result: Result<YamlContent, String>,
    ) {
        if !self.accepts_refresh(id, target, ContentKind::Yaml) {
            return;
        }
        match result {
            Ok(yaml) => {
                self.content_body = yaml.body;
                self.content_managed_fields = yaml.with_managed_fields;
                self.content_decoded_body = yaml.decoded_body;
                self.content_decoded_managed_fields = yaml.decoded_with_managed_fields;
                if self.ui.managed_fields() && self.content_managed_fields.is_none() {
                    self.ui.toggle(UiFlags::MANAGED_FIELDS);
                }
                self.refresh_finished("YAML", None);
            }
            Err(error) => self.refresh_finished("YAML", Some(error)),
        }
    }

    pub fn apply_describe_refresh(
        &mut self,
        id: u64,
        target: &ResourceTarget,
        result: Result<String, String>,
    ) {
        if !self.accepts_refresh(id, target, ContentKind::Describe) {
            return;
        }
        match result {
            Ok(body) => {
                self.content_body = body;
                self.refresh_finished("Describe", None);
            }
            Err(error) => self.refresh_finished("Describe", Some(error)),
        }
    }
}
