//! Connection overrides shared by Kubernetes clients and kubectl subprocesses.

use std::{collections::BTreeMap, ffi::OsString, fmt, path::PathBuf, time::Duration};

use clap::Args;
use kube::{
    Client, Config,
    config::{
        AuthInfo, Cluster, Context, KubeConfigOptions, Kubeconfig, KubeconfigError, NamedCluster,
        NamedContext,
    },
};

#[derive(Clone, Default, PartialEq, Eq, Args)]
pub struct ConnectionOptions {
    /// Path to the kubeconfig file used for all requests and context operations.
    #[arg(long)]
    pub kubeconfig: Option<PathBuf>,
    /// Override the kubeconfig cluster.
    #[arg(long)]
    pub cluster: Option<String>,
    /// Override the kubeconfig user.
    #[arg(long)]
    pub user: Option<String>,
    /// Username to impersonate for Kubernetes operations.
    #[arg(long = "as")]
    pub impersonate: Option<String>,
    /// Group to impersonate; may be repeated.
    #[arg(long = "as-group")]
    pub impersonate_groups: Vec<String>,
    /// Request timeout, such as 30s, 500ms, or 1m30s; zero disables timeouts.
    #[arg(long, value_parser = validate_timeout)]
    pub request_timeout: Option<String>,
    /// Override the Kubernetes API server address.
    #[arg(long, value_parser = validate_server)]
    pub server: Option<String>,
    /// Path to the certificate authority bundle for TLS verification.
    #[arg(long)]
    pub certificate_authority: Option<PathBuf>,
    /// Path to a client certificate for TLS authentication.
    #[arg(long)]
    pub client_certificate: Option<PathBuf>,
    /// Path to a client private key for TLS authentication.
    #[arg(long)]
    pub client_key: Option<PathBuf>,
    /// Bearer token used to authenticate to the API server.
    #[arg(long)]
    pub token: Option<String>,
    /// Disable server certificate verification; accepts an explicit false value.
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub insecure_skip_tls_verify: Option<bool>,
}

impl fmt::Debug for ConnectionOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionOptions")
            .field("kubeconfig", &self.kubeconfig)
            .field("cluster", &self.cluster)
            .field("user", &self.user)
            .field("impersonate", &self.impersonate)
            .field("impersonate_groups", &self.impersonate_groups)
            .field("request_timeout", &self.request_timeout)
            .field("server", &self.server)
            .field("certificate_authority", &self.certificate_authority)
            .field("client_certificate", &self.client_certificate)
            .field("client_key", &self.client_key)
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .field("insecure_skip_tls_verify", &self.insecure_skip_tls_verify)
            .finish()
    }
}

impl ConnectionOptions {
    /// Deadline for individual action API calls, using the K9s default when no override is set.
    /// An explicit zero preserves the request-timeout flag's no-deadline behavior.
    ///
    /// # Errors
    /// Returns an error for an invalid programmatically supplied timeout.
    pub fn api_call_timeout(&self) -> Result<Option<Duration>, String> {
        let duration = self
            .request_timeout
            .as_deref()
            .map_or_else(|| Ok(Duration::from_secs(120)), parse_timeout)?;
        Ok((!duration.is_zero()).then_some(duration))
    }

    /// Read the explicitly selected file, or Kubernetes' normal merged config.
    ///
    /// # Errors
    /// Returns a diagnostic without including malformed credential contents.
    pub fn read_kubeconfig(&self) -> Result<Kubeconfig, String> {
        self.kubeconfig
            .as_ref()
            .map_or_else(Kubeconfig::read, Kubeconfig::read_from)
            .map_err(|error| self.redact(&safe_config_error(error)))
    }

    /// Build connection configuration with the same overrides for every context.
    /// Explicit kubeconfig/context/cluster/user selections never fall back to a
    /// different source of credentials.
    ///
    /// # Errors
    /// Returns configuration or certificate errors with explicit tokens redacted.
    pub async fn config(&self, context: Option<&str>) -> Result<Config, String> {
        if self.insecure_skip_tls_verify == Some(true) && self.certificate_authority.is_some() {
            return Err(
                "--certificate-authority and --insecure-skip-tls-verify=true cannot be combined"
                    .to_owned(),
            );
        }
        if self == &Self::default() {
            return match context {
                Some(context) => Config::from_kubeconfig(&KubeConfigOptions {
                    context: Some(context.to_owned()),
                    ..KubeConfigOptions::default()
                }).await.map_err(safe_config_error),
                None => Config::infer().await.map_err(|_| "Unable to infer Kubernetes configuration from kubeconfig or service account".to_owned()),
            };
        }
        if let Some(server) = &self.server {
            validate_server(server)?;
        }
        let mut config = match self.read_kubeconfig() {
            Ok(raw) => self.config_from_kubeconfig(raw, context).await?,
            Err(error)
                if self.kubeconfig.is_some()
                    || self.cluster.is_some()
                    || self.user.is_some()
                    || context.is_some() =>
            {
                return Err(error);
            }
            Err(_) if self.server.is_some() => {
                self.config_from_kubeconfig(Kubeconfig::default(), None)
                    .await?
            }
            Err(_) => Config::incluster()
                .map_err(|_| "Unable to load kubeconfig or in-cluster configuration".to_owned())?,
        };
        if context.is_none() {
            config.apply_debug_overrides();
        }
        self.apply_runtime_overrides(&mut config)?;
        Ok(config)
    }

    async fn config_from_kubeconfig(
        &self,
        mut raw: Kubeconfig,
        context: Option<&str>,
    ) -> Result<Config, String> {
        if context.is_none()
            && raw.current_context.as_deref().is_none_or(str::is_empty)
            && (self.server.is_some() || self.cluster.is_some())
        {
            let mut synthetic_name = "kux-cli".to_owned();
            while raw
                .contexts
                .iter()
                .any(|entry| entry.name == synthetic_name)
            {
                synthetic_name.push('-');
            }
            raw.current_context = Some(synthetic_name.clone());
            raw.contexts.push(NamedContext {
                name: synthetic_name,
                context: Some(Context {
                    cluster: self.cluster.clone().unwrap_or_else(|| "kux-cli".to_owned()),
                    ..Context::default()
                }),
                ..NamedContext::default()
            });
            if self.cluster.is_none() {
                raw.clusters.insert(
                    0,
                    NamedCluster {
                        name: "kux-cli".to_owned(),
                        cluster: Some(Cluster::default()),
                        ..NamedCluster::default()
                    },
                );
            }
        }
        for cluster in raw
            .clusters
            .iter_mut()
            .filter_map(|entry| entry.cluster.as_mut())
        {
            if let Some(server) = &self.server {
                cluster.server = Some(server.clone());
                cluster.tls_server_name = None;
            }
            if let Some(path) = &self.certificate_authority {
                cluster.certificate_authority = Some(path.to_string_lossy().into_owned());
                cluster.certificate_authority_data = None;
                cluster.insecure_skip_tls_verify = Some(false);
            }
            if let Some(insecure) = self.insecure_skip_tls_verify {
                cluster.insecure_skip_tls_verify = Some(insecure);
                if insecure {
                    cluster.certificate_authority = None;
                    cluster.certificate_authority_data = None;
                }
            }
        }
        Config::from_custom_kubeconfig(
            raw,
            &KubeConfigOptions {
                context: context.map(str::to_owned),
                cluster: self.cluster.clone(),
                user: self.user.clone(),
            },
        )
        .await
        .map_err(|error| self.redact(&safe_config_error(error)))
    }

    fn apply_runtime_overrides(&self, config: &mut Config) -> Result<(), String> {
        if let Some(server) = &self.server {
            config.cluster_url = server
                .parse()
                .map_err(|_| "Invalid API server address".to_owned())?;
            config.tls_server_name = None;
        }
        if let Some(insecure) = self.insecure_skip_tls_verify {
            config.accept_invalid_certs = insecure;
        }
        if let Some(path) = &self.certificate_authority
            && self.insecure_skip_tls_verify != Some(true)
        {
            config.root_cert_file = Some(path.clone());
            config.root_cert = None;
            config.accept_invalid_certs = false;
        }
        if self.insecure_skip_tls_verify == Some(true) {
            config.root_cert_file = None;
            config.root_cert = None;
        }
        if let Some(timeout) = &self.request_timeout {
            let duration = parse_timeout(timeout)?;
            let timeout = (!duration.is_zero()).then_some(duration);
            config.connect_timeout = timeout;
            config.read_timeout = timeout;
            config.write_timeout = timeout;
        }
        self.apply_auth_overrides(&mut config.auth_info);
        Ok(())
    }

    fn apply_auth_overrides(&self, auth: &mut AuthInfo) {
        if let Some(token) = &self.token {
            auth.token = Some(token.clone().into());
            auth.token_file = None;
            auth.username = None;
            auth.password = None;
            auth.auth_provider = None;
            auth.exec = None;
        }
        if let Some(path) = &self.client_certificate {
            auth.client_certificate = Some(path.to_string_lossy().into_owned());
            auth.client_certificate_data = None;
        }
        if let Some(path) = &self.client_key {
            auth.client_key = Some(path.to_string_lossy().into_owned());
            auth.client_key_data = None;
        }
        if let Some(user) = &self.impersonate {
            auth.impersonate = Some(user.clone());
        }
        if !self.impersonate_groups.is_empty() {
            auth.impersonate_groups = Some(self.impersonate_groups.clone());
        }
    }

    /// Construct a client without making a network request.
    ///
    /// # Errors
    /// Returns a redacted configuration or client construction diagnostic.
    pub async fn client(&self, context: Option<&str>) -> Result<Client, String> {
        Client::try_from(self.config(context).await?)
            .map_err(|error| self.redact(&format!("Unable to create Kubernetes client: {error}")))
    }

    /// Global arguments shared by kubectl invocations. This contains credentials
    /// when a token was supplied; never log or display the returned arguments.
    #[must_use]
    pub fn kubectl_args(&self, context: Option<&str>) -> Vec<OsString> {
        let mut args = Vec::new();
        for (flag, value) in [
            (
                "--kubeconfig",
                self.kubeconfig.as_ref().map(|path| path.as_os_str()),
            ),
            (
                "--certificate-authority",
                self.certificate_authority
                    .as_ref()
                    .map(|path| path.as_os_str()),
            ),
            (
                "--client-certificate",
                self.client_certificate
                    .as_ref()
                    .map(|path| path.as_os_str()),
            ),
            (
                "--client-key",
                self.client_key.as_ref().map(|path| path.as_os_str()),
            ),
        ] {
            if let Some(value) = value {
                args.extend([OsString::from(flag), value.to_owned()]);
            }
        }
        for (flag, value) in [
            ("--context", context),
            ("--cluster", self.cluster.as_deref()),
            ("--user", self.user.as_deref()),
            ("--as", self.impersonate.as_deref()),
            ("--request-timeout", self.request_timeout.as_deref()),
            ("--server", self.server.as_deref()),
            ("--token", self.token.as_deref()),
        ] {
            if let Some(value) = value {
                args.extend([OsString::from(flag), OsString::from(value)]);
            }
        }
        for group in &self.impersonate_groups {
            args.extend([OsString::from("--as-group"), OsString::from(group)]);
        }
        if let Some(insecure) = self.insecure_skip_tls_verify {
            args.push(OsString::from(format!(
                "--insecure-skip-tls-verify={insecure}"
            )));
        }
        args
    }

    /// Global arguments shared by `helm` CLI invocations.
    #[must_use]
    pub fn helm_args(&self, context: Option<&str>) -> Vec<OsString> {
        let mut args = Vec::new();
        for (flag, value) in [
            (
                "--kubeconfig",
                self.kubeconfig.as_ref().map(|path| path.as_os_str()),
            ),
            (
                "--kube-ca-file",
                self.certificate_authority
                    .as_ref()
                    .map(|path| path.as_os_str()),
            ),
        ] {
            if let Some(value) = value {
                args.extend([OsString::from(flag), value.to_owned()]);
            }
        }
        for (flag, value) in [
            ("--kube-context", context),
            ("--kube-as-user", self.impersonate.as_deref()),
            ("--kube-apiserver", self.server.as_deref()),
            ("--kube-token", self.token.as_deref()),
        ] {
            if let Some(value) = value {
                args.extend([OsString::from(flag), OsString::from(value)]);
            }
        }
        for group in &self.impersonate_groups {
            args.extend([OsString::from("--kube-as-group"), OsString::from(group)]);
        }
        if let Some(insecure) = self.insecure_skip_tls_verify {
            args.push(OsString::from(format!(
                "--kube-insecure-skip-tls-verify={insecure}"
            )));
        }
        args
    }

    /// Remove explicitly supplied credentials from subprocess and API diagnostics.
    #[must_use]
    pub fn redact(&self, text: &str) -> String {
        self.token
            .as_deref()
            .filter(|value| !value.is_empty())
            .map_or_else(
                || text.to_owned(),
                |token| text.replace(token, "[REDACTED]"),
            )
    }

    /// Connection identity overrides for plugin processes; excludes credentials.
    #[must_use]
    pub fn plugin_env(&self) -> BTreeMap<String, String> {
        let mut environment = BTreeMap::new();
        if let Some(user) = self.impersonate.as_ref().or(self.user.as_ref()) {
            environment.insert("USER".to_owned(), user.clone());
        }
        if let Some(cluster) = &self.cluster {
            environment.insert("CLUSTER".to_owned(), cluster.clone());
        }
        if !self.impersonate_groups.is_empty() {
            environment.insert("GROUPS".to_owned(), self.impersonate_groups.join(","));
        }
        let kubeconfig = self
            .kubeconfig
            .as_ref()
            .map(|path| path.as_os_str().to_owned())
            .or_else(|| std::env::var_os("KUBECONFIG"));
        if let Some(path) = kubeconfig {
            environment.insert("KUBECONFIG".to_owned(), path.to_string_lossy().into_owned());
        }
        environment
    }
}

fn safe_config_error(error: KubeconfigError) -> String {
    match error {
        KubeconfigError::Parse(_) => {
            "Unable to parse kubeconfig YAML; check the selected file".to_owned()
        }
        error => format!("Unable to load kubeconfig: {error}"),
    }
}

fn validate_server(value: &str) -> Result<String, String> {
    let uri = value
        .parse::<http::Uri>()
        .map_err(|_| "API server must be an HTTP(S) URL".to_owned())?;
    if !matches!(uri.scheme_str(), Some("http" | "https"))
        || uri
            .authority()
            .is_none_or(|authority| authority.as_str().contains('@'))
        || uri.query().is_some()
        || value.contains('#')
    {
        return Err(
            "API server must be an HTTP(S) URL without credentials, query, or fragment".to_owned(),
        );
    }
    Ok(value.to_owned())
}

fn validate_timeout(value: &str) -> Result<String, String> {
    parse_timeout(value).map(|_| value.to_owned())
}

fn parse_timeout(value: &str) -> Result<Duration, String> {
    if let Ok(seconds) = value.parse::<u64>() {
        return Ok(Duration::from_secs(seconds));
    }
    let pattern =
        regex::Regex::new(r"^(\d+(?:\.\d*)?|\.\d+)(ns|us|µs|μs|ms|s|m|h)").expect("duration regex");
    let mut remaining = value;
    let mut seconds = 0.0;
    while !remaining.is_empty() {
        let matched = pattern.captures(remaining).ok_or_else(|| {
            "Request timeout must be a nonnegative duration such as 30s, 500ms, or 1m30s".to_owned()
        })?;
        let number: f64 = matched[1]
            .parse()
            .map_err(|_| "Request timeout is out of range".to_owned())?;
        let factor = match &matched[2] {
            "ns" => 1e-9,
            "us" | "µs" | "μs" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => unreachable!(),
        };
        seconds += number * factor;
        remaining = &remaining[matched[0].len()..];
    }
    if value.is_empty() {
        return Err("Request timeout cannot be empty".to_owned());
    }
    Duration::try_from_secs_f64(seconds).map_err(|_| "Request timeout is out of range".to_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use clap::Parser;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[test]
    fn action_deadlines_use_upstream_default_and_explicit_request_timeout() {
        let mut options = ConnectionOptions::default();
        assert_eq!(
            options.api_call_timeout().unwrap(),
            Some(Duration::from_secs(120))
        );
        options.request_timeout = Some("1m30s".to_owned());
        assert_eq!(
            options.api_call_timeout().unwrap(),
            Some(Duration::from_secs(90))
        );
        options.request_timeout = Some("0".to_owned());
        assert_eq!(options.api_call_timeout().unwrap(), None);
        options.request_timeout = Some("invalid".to_owned());
        assert!(options.api_call_timeout().is_err());
    }

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("kux-connection-{}-{sequence}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn write_config(&self) -> PathBuf {
            let path = self.0.join("config");
            std::fs::write(&path, "apiVersion: v1\nkind: Config\ncurrent-context: first\ncontexts:\n- name: first\n  context: {cluster: one, user: alice, namespace: team-a}\n- name: second\n  context: {cluster: two, user: bob, namespace: team-b}\nclusters:\n- name: one\n  cluster: {server: 'https://first.invalid'}\n- name: two\n  cluster: {server: 'https://second.invalid'}\nusers:\n- name: alice\n  user: {token: fixture-alice-token}\n- name: bob\n  user: {token: fixture-bob-token}\n").unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn parses_cli_connection_overrides_without_debugging_tokens() {
        let cli = crate::cli::Cli::try_parse_from([
            "kux",
            "--kubeconfig",
            "/tmp/selected config",
            "--cluster",
            "two",
            "--user",
            "bob",
            "--as",
            "viewer",
            "--as-group",
            "one",
            "--as-group",
            "two",
            "--request-timeout",
            "1m30.5s",
            "--server",
            "https://server.invalid",
            "--certificate-authority",
            "/tmp/ca",
            "--client-certificate",
            "/tmp/cert",
            "--client-key",
            "/tmp/key",
            "--token",
            "fixture-override-token",
            "--insecure-skip-tls-verify=false",
        ])
        .unwrap();
        assert_eq!(cli.connection.impersonate_groups, ["one", "two"]);
        assert_eq!(cli.connection.insecure_skip_tls_verify, Some(false));
        let debug = format!("{cli:?}");
        assert!(!debug.contains("fixture-override-token"));
        assert!(debug.contains("[REDACTED]"));
        assert!(crate::cli::Cli::try_parse_from(["kux", "--as-group", "one"]).is_ok());
        for timeout in ["-1s", "garbage", "", "NaNs"] {
            assert!(parse_timeout(timeout).is_err());
        }
        assert_eq!(
            parse_timeout("1m30.5s").unwrap(),
            Duration::from_millis(90_500)
        );
        assert_eq!(parse_timeout("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_timeout("15").unwrap(), Duration::from_secs(15));
        assert!(validate_server("https://user:credential@server.invalid").is_err());
    }

    #[tokio::test]
    async fn explicit_file_cluster_and_user_overrides_survive_context_changes() {
        let fixture = Fixture::new();
        let options = ConnectionOptions {
            kubeconfig: Some(fixture.write_config()),
            cluster: Some("two".to_owned()),
            user: Some("bob".to_owned()),
            impersonate: Some("viewer".to_owned()),
            impersonate_groups: vec!["readers".to_owned(), "auditors".to_owned()],
            request_timeout: Some("1.5s".to_owned()),
            ..ConnectionOptions::default()
        };
        for (context, namespace) in [("first", "team-a"), ("second", "team-b")] {
            let config = options.config(Some(context)).await.unwrap();
            assert_eq!(config.cluster_url.host(), Some("second.invalid"));
            assert_eq!(config.default_namespace, namespace);
            assert_eq!(config.read_timeout, Some(Duration::from_millis(1_500)));
            assert_eq!(config.auth_info.impersonate.as_deref(), Some("viewer"));
            assert_eq!(
                config.auth_info.impersonate_groups,
                Some(vec!["readers".to_owned(), "auditors".to_owned()])
            );
            let auth = serde_json::to_value(config.auth_info).unwrap();
            let selected_user_token = auth["token"] == "fixture-bob-token";
            assert!(selected_user_token, "selected user credentials must match");
        }
        let catalog = crate::kubeconfig::read_contexts_with(&options, None).unwrap();
        assert_eq!(catalog.active, "first");
        assert!(
            catalog
                .contexts
                .iter()
                .all(|context| context.cluster.as_deref() == Some("two")
                    && context.user.as_deref() == Some("bob"))
        );
    }

    #[test]
    fn token_and_client_credentials_replace_inherited_authentication() {
        let options = ConnectionOptions {
            token: Some("fixture-explicit-token".to_owned()),
            client_certificate: Some(PathBuf::from("/explicit/cert")),
            client_key: Some(PathBuf::from("/explicit/key")),
            request_timeout: Some("0".to_owned()),
            ..ConnectionOptions::default()
        };
        let mut config = Config::new("https://server.invalid".parse().unwrap());
        config.auth_info = serde_json::from_value(serde_json::json!({
            "username": "basic", "password": "fixture-password", "tokenFile": "/stale/token",
            "auth-provider": {"name": "oidc", "config": {"id-token": "fixture-old-token"}},
            "exec": {"command": "/must-not-run", "apiVersion": "client.authentication.k8s.io/v1"},
            "client-certificate-data": "old", "client-key-data": "old"
        }))
        .unwrap();
        options.apply_runtime_overrides(&mut config).unwrap();
        assert!(config.auth_info.exec.is_none());
        assert!(config.auth_info.auth_provider.is_none());
        assert!(config.auth_info.username.is_none());
        assert!(config.auth_info.password.is_none());
        assert!(config.auth_info.token_file.is_none());
        assert!(config.auth_info.client_certificate_data.is_none());
        assert!(config.auth_info.client_key_data.is_none());
        assert_eq!(
            config.auth_info.client_certificate.as_deref(),
            Some("/explicit/cert")
        );
        assert_eq!(config.connect_timeout, None);
        assert_eq!(config.read_timeout, None);
        assert_eq!(config.write_timeout, None);
    }

    #[tokio::test]
    async fn tls_overrides_replace_inherited_ca_sources_before_loading() {
        let fixture = Fixture::new();
        let ca_file = fixture.0.join("override-ca.pem");
        std::fs::write(
            &ca_file,
            "-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        let mut raw = Kubeconfig::read_from(fixture.write_config()).unwrap();
        let cluster = raw.clusters[0].cluster.as_mut().unwrap();
        cluster.certificate_authority_data = Some("invalid embedded data".to_owned());
        cluster.certificate_authority = Some("/must-not-read/old-ca".to_owned());
        cluster.insecure_skip_tls_verify = Some(true);
        let options = ConnectionOptions {
            certificate_authority: Some(ca_file.clone()),
            ..ConnectionOptions::default()
        };
        let mut config = options
            .config_from_kubeconfig(raw.clone(), Some("first"))
            .await
            .unwrap();
        options.apply_runtime_overrides(&mut config).unwrap();
        assert!(!config.accept_invalid_certs);
        assert_eq!(config.root_cert_file, Some(ca_file));
        let insecure = ConnectionOptions {
            insecure_skip_tls_verify: Some(true),
            ..ConnectionOptions::default()
        };
        let config = insecure
            .config_from_kubeconfig(raw, Some("first"))
            .await
            .unwrap();
        assert!(config.accept_invalid_certs);
        assert_eq!(config.root_cert, None);
        let conflicting = ConnectionOptions {
            certificate_authority: Some(fixture.0.join("override-ca.pem")),
            insecure_skip_tls_verify: Some(true),
            ..ConnectionOptions::default()
        };
        assert!(
            conflicting
                .config(None)
                .await
                .unwrap_err()
                .contains("cannot be combined")
        );
    }

    #[tokio::test]
    async fn server_or_cluster_override_supports_config_without_current_context() {
        let options = ConnectionOptions {
            server: Some("https://selected.invalid".to_owned()),
            ..ConnectionOptions::default()
        };
        let config = options
            .config_from_kubeconfig(Kubeconfig::default(), None)
            .await
            .unwrap();
        assert_eq!(config.cluster_url.host(), Some("selected.invalid"));
        let fixture = Fixture::new();
        let mut raw = Kubeconfig::read_from(fixture.write_config()).unwrap();
        raw.clusters[0].cluster.as_mut().unwrap().tls_server_name = Some("old.invalid".to_owned());
        let overridden = options
            .config_from_kubeconfig(raw.clone(), Some("first"))
            .await
            .unwrap();
        assert_eq!(overridden.tls_server_name, None);
        raw.current_context = None;
        let options = ConnectionOptions {
            cluster: Some("two".to_owned()),
            user: Some("bob".to_owned()),
            ..ConnectionOptions::default()
        };
        let config = options.config_from_kubeconfig(raw, None).await.unwrap();
        assert_eq!(config.cluster_url.host(), Some("second.invalid"));
    }

    #[tokio::test]
    async fn real_client_sends_selected_bearer_and_impersonation_headers() {
        let fixture = Fixture::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let options = ConnectionOptions {
            kubeconfig: Some(fixture.write_config()),
            server: Some(format!("http://{}", listener.local_addr().unwrap())),
            token: Some("fixture-explicit-token".to_owned()),
            impersonate: Some("viewer".to_owned()),
            impersonate_groups: vec!["readers".to_owned(), "auditors".to_owned()],
            ..ConnectionOptions::default()
        };
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                let mut buffer = [0; 2048];
                let count = stream.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&buffer[..count]);
            }
            let headers = String::from_utf8(bytes).unwrap().to_lowercase();
            assert!(headers.contains("authorization: bearer fixture-explicit-token\r\n"));
            assert!(headers.contains("impersonate-user: viewer\r\n"));
            assert!(headers.contains("impersonate-group: readers\r\n"));
            assert!(headers.contains("impersonate-group: auditors\r\n"));
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await.unwrap();
        });
        let client = options.client(Some("second")).await.unwrap();
        let request = http::Request::get("/version").body(Vec::new()).unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            client.request::<serde_json::Value>(request),
        )
        .await
        .unwrap()
        .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn explicit_missing_or_malformed_file_never_falls_back_or_leaks_source() {
        let fixture = Fixture::new();
        let path = fixture.0.join("missing");
        let options = ConnectionOptions {
            kubeconfig: Some(path.clone()),
            server: Some("http://127.0.0.1:1".to_owned()),
            ..ConnectionOptions::default()
        };
        assert!(options.config(None).await.is_err());
        std::fs::write(
            path,
            "users: [{name: a, user: {token: fixture-private-token}}\n",
        )
        .unwrap();
        let error = options.config(None).await.unwrap_err();
        assert!(!error.contains("fixture-private-token"));
        assert!(error.contains("parse kubeconfig"));
    }

    #[test]
    fn subprocess_arguments_and_plugin_identity_preserve_overrides_without_shell_parsing() {
        let options = ConnectionOptions {
            kubeconfig: Some(PathBuf::from("/tmp/config with spaces")),
            cluster: Some("selected-cluster".to_owned()),
            user: Some("selected-user".to_owned()),
            impersonate: Some("effective-user".to_owned()),
            impersonate_groups: vec!["one".to_owned(), "two".to_owned()],
            token: Some("fixture-token".to_owned()),
            insecure_skip_tls_verify: Some(false),
            ..ConnectionOptions::default()
        };
        let args = options.kubectl_args(Some("selected-context"));
        for (flag, value) in [
            ("--kubeconfig", "/tmp/config with spaces"),
            ("--context", "selected-context"),
            ("--cluster", "selected-cluster"),
            ("--user", "selected-user"),
            ("--as", "effective-user"),
            ("--as-group", "one"),
            ("--as-group", "two"),
            ("--token", "fixture-token"),
        ] {
            assert!(
                args.windows(2)
                    .any(|pair| pair[0] == flag && pair[1] == value)
            );
        }
        assert!(args.contains(&OsString::from("--insecure-skip-tls-verify=false")));
        let environment = options.plugin_env();
        assert_eq!(environment["USER"], "effective-user");
        assert_eq!(environment["GROUPS"], "one,two");
        assert_eq!(environment["KUBECONFIG"], "/tmp/config with spaces");
        assert!(!format!("{environment:?}").contains("fixture-token"));
        assert_eq!(
            options.redact("server echoed fixture-token"),
            "server echoed [REDACTED]"
        );
    }
}
