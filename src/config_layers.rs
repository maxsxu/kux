//! Effective settings and extensions are assembled entirely on the config worker.

use super::ConfigContext;
use crate::{
    config::{ConfigDiagnostic, ConfigPaths, ConfigWatcher, UserConfig},
    plugins::{PluginCatalog, PluginWatcher},
    theme::Skin,
};

pub(super) struct ConfigurationLayers {
    paths: ConfigPaths,
    settings: ConfigWatcher,
    plugins: PluginWatcher,
    base: UserConfig,
    catalog: PluginCatalog,
    skin: Skin,
    last: Option<UserConfig>,
}

impl ConfigurationLayers {
    pub(super) fn new(paths: ConfigPaths, scope: &ConfigContext) -> Self {
        Self {
            settings: ConfigWatcher::new(
                paths.clone(),
                scope.cluster.as_deref(),
                Some(&scope.context),
            ),
            plugins: PluginWatcher::new(
                paths.clone(),
                scope.cluster.as_deref(),
                Some(&scope.context),
            ),
            paths,
            base: UserConfig::default(),
            catalog: PluginCatalog::default(),
            skin: Skin::default(),
            last: None,
        }
    }

    pub(super) fn refresh(&mut self, scope: &ConfigContext, requested: bool) -> Option<UserConfig> {
        if requested {
            self.base = self
                .settings
                .set_context(scope.cluster.as_deref(), Some(&scope.context));
            self.catalog = self
                .plugins
                .set_context(scope.cluster.as_deref(), Some(&scope.context));
        } else {
            if let Some(config) = self.settings.reload_if_changed() {
                self.base = config;
            }
            if let Some(catalog) = self.plugins.reload_if_changed() {
                self.catalog = catalog;
            }
        }
        let mut effective = self.base.clone();
        effective.plugins = self.catalog.clone();
        effective
            .diagnostics
            .extend(self.catalog.diagnostics.clone());
        if let Some(name) = effective.settings.ui.skin.as_deref() {
            match Skin::load_named(&self.paths, name) {
                Ok(skin) => self.skin = skin,
                Err(message) => effective.diagnostics.push(ConfigDiagnostic {
                    path: self
                        .paths
                        .config_dir
                        .join("skins")
                        .join(format!("{name}.yaml")),
                    message,
                }),
            }
        } else {
            self.skin = Skin::default();
        }
        effective.theme = self.skin.clone();
        if !requested && self.last.as_ref() == Some(&effective) {
            return None;
        }
        self.last = Some(effective.clone());
        Some(effective)
    }
}
