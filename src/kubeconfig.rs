use kube::config::Kubeconfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextInfo {
    pub name: String,
    pub cluster: Option<String>,
    pub user: Option<String>,
    pub namespace: Option<String>,
    pub kubeconfig_backed: bool,
}

impl ContextInfo {
    #[must_use]
    pub fn synthetic(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            cluster: None,
            user: None,
            namespace: None,
            kubeconfig_backed: false,
        }
    }

    #[must_use]
    pub fn cluster_display(&self) -> &str {
        self.cluster
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or("-")
    }

    #[must_use]
    pub fn user_display(&self) -> &str {
        self.user
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or("-")
    }

    #[must_use]
    pub fn namespace_display(&self) -> &str {
        if self.kubeconfig_backed {
            self.namespace
                .as_deref()
                .filter(|value| !value.is_empty())
                .unwrap_or("default")
        } else {
            "-"
        }
    }

    #[must_use]
    pub fn search_text(&self) -> String {
        format!(
            "{} {} {} {}",
            self.name,
            self.cluster_display(),
            self.user_display(),
            self.namespace_display()
        )
    }
}

impl From<String> for ContextInfo {
    fn from(name: String) -> Self {
        Self::synthetic(name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextCatalog {
    pub active: String,
    pub contexts: Vec<ContextInfo>,
}

#[must_use]
pub fn context_catalog(config: Kubeconfig, requested: Option<&str>) -> ContextCatalog {
    let active = requested
        .map(str::to_owned)
        .or(config.current_context)
        .unwrap_or_else(|| "current".to_owned());
    let mut contexts = config
        .contexts
        .into_iter()
        .map(|named| match named.context {
            Some(context) => ContextInfo {
                name: named.name,
                cluster: Some(context.cluster),
                user: context.user,
                namespace: context.namespace,
                kubeconfig_backed: true,
            },
            None => ContextInfo::synthetic(named.name),
        })
        .collect::<Vec<_>>();
    contexts.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    contexts.dedup_by(|left, right| left.name == right.name);
    if !contexts.iter().any(|item| item.name == active) {
        contexts.push(ContextInfo::synthetic(active.clone()));
        contexts.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    }
    ContextCatalog { active, contexts }
}

/// Reads and normalizes the merged kubeconfig context catalog.
///
/// # Errors
///
/// Returns an error when kube-client cannot read or parse the configured
/// kubeconfig files.
pub fn read_contexts(requested: Option<&str>) -> Result<ContextCatalog, String> {
    Kubeconfig::read()
        .map(|config| context_catalog(config, requested))
        .map_err(|error| format!("Unable to read kubeconfig contexts: {error}"))
}

#[must_use]
pub fn fallback_contexts(requested: Option<&str>) -> ContextCatalog {
    read_contexts(requested).unwrap_or_else(|_| {
        let active = requested.unwrap_or("current").to_owned();
        ContextCatalog {
            active: active.clone(),
            contexts: vec![ContextInfo::synthetic(active)],
        }
    })
}

#[cfg(test)]
mod tests {
    use kube::config::{Context, Kubeconfig, NamedContext};

    use super::*;

    fn named(
        name: &str,
        cluster: &str,
        user: Option<&str>,
        namespace: Option<&str>,
    ) -> NamedContext {
        NamedContext {
            name: name.to_owned(),
            context: Some(Context {
                cluster: cluster.to_owned(),
                user: user.map(str::to_owned),
                namespace: namespace.map(str::to_owned),
                ..Context::default()
            }),
            ..NamedContext::default()
        }
    }

    #[test]
    fn maps_context_name_cluster_user_and_namespace() {
        let config = Kubeconfig {
            current_context: Some("prod".to_owned()),
            contexts: vec![
                named("prod", "gke-prod", Some("alice"), Some("payments")),
                named("dev", "kind-dev", None, None),
            ],
            ..Kubeconfig::default()
        };

        let catalog = context_catalog(config, None);
        assert_eq!(catalog.active, "prod");
        let prod = catalog
            .contexts
            .iter()
            .find(|item| item.name == "prod")
            .expect("prod context");
        assert_eq!(prod.cluster_display(), "gke-prod");
        assert_eq!(prod.user_display(), "alice");
        assert_eq!(prod.namespace_display(), "payments");
        let dev = catalog
            .contexts
            .iter()
            .find(|item| item.name == "dev")
            .expect("dev context");
        assert_eq!(dev.user_display(), "-");
        assert_eq!(dev.namespace_display(), "default");
        assert!(dev.kubeconfig_backed);
    }

    #[test]
    fn requested_and_malformed_contexts_have_safe_fallbacks() {
        let config = Kubeconfig {
            current_context: Some("broken".to_owned()),
            contexts: vec![NamedContext {
                name: "broken".to_owned(),
                context: None,
                ..NamedContext::default()
            }],
            ..Kubeconfig::default()
        };

        let catalog = context_catalog(config, Some("missing"));
        assert_eq!(catalog.active, "missing");
        let broken = catalog
            .contexts
            .iter()
            .find(|item| item.name == "broken")
            .expect("broken context");
        let missing = catalog
            .contexts
            .iter()
            .find(|item| item.name == "missing")
            .expect("missing context");
        assert_eq!(broken.cluster_display(), "-");
        assert_eq!(broken.namespace_display(), "-");
        assert!(!broken.kubeconfig_backed);
        assert!(!missing.kubeconfig_backed);
    }
}
