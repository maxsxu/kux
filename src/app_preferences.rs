use super::{AccessMode, App, UiFlags};
use crate::{cli::Cli, config::UserConfig};

#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)] // Independent, additive command-line switches.
pub struct LaunchPreferences {
    pub headless: bool,
    pub logoless: bool,
    pub crumbsless: bool,
    pub splashless: bool,
    pub invert: bool,
    pub refresh: Option<std::time::Duration>,
}

impl App {
    pub fn configure_launch(&mut self, cli: &Cli) {
        self.launch = LaunchPreferences {
            headless: cli.headless,
            logoless: cli.no_logo,
            crumbsless: cli.crumbsless,
            splashless: cli.splashless,
            invert: cli.invert,
            refresh: cli
                .refresh
                .map(|duration| duration.max(std::time::Duration::from_secs(2))),
        };
        if cli.write {
            self.cli_access = AccessMode::ForceReadWrite;
        }
        self.apply_user_config(self.user_config.clone());
    }

    pub(super) fn apply_launch_preferences(&self, config: &mut UserConfig) {
        config.settings.ui.headless |= self.launch.headless;
        config.settings.ui.logoless |= self.launch.logoless;
        config.settings.ui.crumbsless |= self.launch.crumbsless;
        config.settings.ui.splashless |= self.launch.splashless;
        config.settings.ui.invert |= self.launch.invert;
        if let Some(refresh) = self.launch.refresh {
            config.settings.refresh_rate = Some(refresh);
        }
    }

    pub(super) fn apply_display_preferences(&mut self, config: &UserConfig) {
        for (current, previous, bit, inverse) in [
            (
                config.settings.ui.crumbsless,
                self.user_config.settings.ui.crumbsless,
                UiFlags::CRUMBS,
                true,
            ),
            (
                config.settings.ui.invert,
                self.user_config.settings.ui.invert,
                UiFlags::INVERT,
                false,
            ),
            (
                config.settings.ui.fullscreen,
                self.user_config.settings.ui.fullscreen,
                UiFlags::FULLSCREEN,
                false,
            ),
        ] {
            if current != previous {
                self.ui.set(bit, current != inverse);
            }
        }
    }

    #[must_use]
    pub fn refresh_interval(&self) -> std::time::Duration {
        self.user_config
            .settings
            .refresh_rate
            .unwrap_or(std::time::Duration::from_secs(5))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ResourceKind;
    use clap::Parser;

    #[test]
    fn launch_overrides_survive_reloads_and_write_has_explicit_precedence() {
        let cli = Cli::try_parse_from([
            "kux",
            "--readonly",
            "--write",
            "--headless",
            "--logoless",
            "--crumbsless",
            "--splashless",
            "--invert",
            "--refresh",
            "2.75",
        ])
        .unwrap();
        let mut app = App::new(
            "demo",
            None,
            ResourceKind::Pods,
            cli.read_only,
            vec!["demo".to_owned()],
        );
        app.configure_launch(&cli);
        for _ in 0..2 {
            let mut config = UserConfig::default();
            config.settings.read_only = true;
            app.apply_user_config(config);
            assert!(!app.read_only);
            assert!(!app.ui.header());
            assert!(!app.ui.logo());
            assert!(!app.ui.crumbs());
            assert!(app.ui.invert());
            assert!(app.user_config.settings.ui.splashless);
            assert_eq!(
                app.refresh_interval(),
                std::time::Duration::from_millis(2_750)
            );
        }
    }

    #[test]
    fn config_preferences_react_without_resetting_unrelated_runtime_toggles() {
        let mut app = App::new(
            "demo",
            None,
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        let mut config = UserConfig::default();
        config.settings.ui.fullscreen = true;
        config.settings.ui.crumbsless = true;
        config.settings.ui.invert = true;
        app.apply_user_config(config.clone());
        assert!(app.ui.fullscreen());
        assert!(!app.ui.crumbs());
        assert!(app.ui.invert());
        app.ui.toggle(UiFlags::WIDE);
        config.settings.ui.invert = false;
        app.apply_user_config(config);
        assert!(app.ui.wide());
        assert!(!app.ui.invert());
    }

    #[test]
    fn global_refresh_reload_preserves_fixed_live_cadence_and_pending_requests() {
        use std::time::{Duration, Instant};
        let mut app = App::new(
            "demo",
            None,
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        let mut config = UserConfig::default();
        config.settings.refresh_rate = Some(Duration::from_secs(600));
        config.settings.live_view_auto_refresh = true;
        app.apply_user_config(config.clone());
        let opened = Instant::now();
        app.show_content(
            "YAML",
            "kind: Pod",
            None,
            None,
            None,
            super::super::ContentKind::Yaml,
        );
        let old_due = app.yaml_refresh.due;
        assert!(old_due >= opened + Duration::from_secs(5));
        assert!(old_due <= Instant::now() + Duration::from_secs(5));
        config.settings.refresh_rate = Some(Duration::from_secs(2));
        app.apply_user_config(config.clone());
        assert_eq!(app.refresh_interval(), Duration::from_secs(2));
        assert_eq!(app.yaml_refresh.due, old_due);
        app.yaml_refresh.pending = true;
        let pending_due = app.yaml_refresh.due;
        config.settings.refresh_rate = Some(Duration::from_secs(3));
        app.apply_user_config(config);
        assert!(app.yaml_refresh.pending);
        assert_eq!(app.yaml_refresh.due, pending_due);
    }
}
