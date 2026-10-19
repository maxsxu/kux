use std::{path::PathBuf, time::Duration};

use clap::{Parser, Subcommand};

use crate::{connection::ConnectionOptions, model::ResourceKind};

/// A fast, watch-driven Kubernetes terminal UI.
#[derive(Debug, Clone, Parser)]
#[allow(clippy::struct_excessive_bools)]
#[command(name = "kux", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub subcommand: Option<CliCommand>,

    #[command(flatten)]
    pub connection: ConnectionOptions,
    /// Kubeconfig context. Uses the current context when omitted.
    #[arg(long)]
    pub context: Option<String>,

    /// Limit namespaced resources to this namespace.
    #[arg(short = 'n', long, conflicts_with = "all_namespaces")]
    pub namespace: Option<String>,

    /// Watch namespaced resources across every namespace.
    #[arg(short = 'A', long)]
    pub all_namespaces: bool,

    /// Resource view shown at startup.
    #[arg(short = 'r', long, value_enum, default_value_t = ResourceKind::Pods)]
    pub resource: ResourceKind,

    /// Command to run at startup, including discovered resources and aliases.
    #[arg(short = 'c', long, conflicts_with = "resource")]
    pub command: Option<String>,

    /// Use generated resources instead of connecting to Kubernetes.
    #[arg(long)]
    pub demo: bool,

    /// Number of generated resources in demo mode.
    #[arg(long, default_value_t = 50_000, value_parser = parse_positive_usize)]
    pub demo_items: usize,

    /// Maximum rate for publishing coalesced Kubernetes updates.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u64).range(16..))]
    pub publish_interval_ms: u64,

    /// Maximum terminal frame rate. Kux renders only when state is dirty.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=120))]
    pub fps: u64,

    /// Use paginated list-then-watch instead of Kubernetes streaming lists.
    #[arg(long)]
    pub legacy_list: bool,

    /// Disable delete, kill, edit, restart, and other mutating commands.
    #[arg(long, visible_alias = "readonly")]
    pub read_only: bool,

    /// Enable mutating commands, overriding the read-only configuration and flag.
    #[arg(long)]
    pub write: bool,

    /// Start without the application header.
    #[arg(long)]
    pub headless: bool,

    /// Start without the Kux logo. Toggle it at runtime with Ctrl-L.
    #[arg(long, visible_alias = "logoless")]
    pub no_logo: bool,

    /// Start without navigation breadcrumbs.
    #[arg(long)]
    pub crumbsless: bool,

    /// Skip the startup splash screen.
    #[arg(long)]
    pub splashless: bool,

    /// Invert the skin between dark and light while preserving color hues.
    #[arg(long)]
    pub invert: bool,

    /// Directory for saved screen and log dumps.
    #[arg(long, value_name = "DIRECTORY")]
    pub screen_dump_dir: Option<PathBuf>,

    /// Specify a log level (error, warn, info, debug).
    #[arg(
        short = 'l',
        long = "logLevel",
        visible_alias = "log-level",
        value_name = "LEVEL"
    )]
    pub log_level: Option<String>,

    /// Specify the log file path.
    #[arg(long = "logFile", visible_alias = "log-file", value_name = "FILE")]
    pub log_file: Option<PathBuf>,

    /// Periodic refresh interval in seconds, including fractions. -r selects a resource.
    #[arg(long, value_name = "SECONDS", value_parser = parse_refresh)]
    pub refresh: Option<Duration>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum CliCommand {
    /// Show configuration and data paths without connecting to Kubernetes.
    Info,
    /// Print version and build information.
    Version {
        /// Print version/build information without the application heading.
        #[arg(short = 's', long)]
        short: bool,
    },
    /// Generate a shell completion script on standard output.
    Completion {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

fn parse_refresh(value: &str) -> Result<Duration, String> {
    let seconds = value
        .parse::<f64>()
        .map_err(|_| "Refresh interval must be a positive number of seconds".to_owned())?;
    let duration = Duration::try_from_secs_f64(seconds)
        .map_err(|_| "Refresh interval must be a finite, positive number of seconds".to_owned())?;
    if duration.is_zero() {
        return Err("Refresh interval must be greater than zero".to_owned());
    }
    if std::time::Instant::now().checked_add(duration).is_none() {
        return Err("Refresh interval is too large for the system timer".to_owned());
    }
    Ok(duration)
}

fn parse_positive_usize(value: &str) -> Result<usize, String> {
    let parsed = value.parse::<usize>().map_err(|error| error.to_string())?;
    if parsed == 0 {
        Err("value must be greater than zero".to_owned())
    } else {
        Ok(parsed)
    }
}

impl Cli {
    #[must_use]
    pub const fn publish_interval(&self) -> Duration {
        Duration::from_millis(self.publish_interval_ms)
    }

    #[must_use]
    pub const fn frame_interval(&self) -> Duration {
        Duration::from_nanos(1_000_000_000 / self.fps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_command_supports_gvr_and_readonly_alias() {
        let cli = Cli::try_parse_from([
            "kux",
            "--command",
            "example.io/v1/widgets team-a",
            "--readonly",
        ])
        .unwrap();
        assert_eq!(cli.command.as_deref(), Some("example.io/v1/widgets team-a"));
        assert!(cli.read_only);
        assert!(Cli::try_parse_from(["kux", "--resource", "pods", "--command", "deploy"]).is_err());
    }

    #[test]
    fn refresh_accepts_fractions_and_preserves_resource_short_flag() {
        let cli = Cli::try_parse_from(["kux", "--refresh", "2.75", "-r", "services"]).unwrap();
        assert_eq!(cli.refresh, Some(Duration::from_millis(2_750)));
        assert_eq!(cli.resource, ResourceKind::Services);
        for value in ["0", "-1", "NaN", "inf", "1e100", "1e19", "fast", "1e-100"] {
            assert!(Cli::try_parse_from(["kux", &format!("--refresh={value}")]).is_err());
        }
    }

    #[test]
    fn presentation_and_write_flags_are_explicit_overrides() {
        let cli = Cli::try_parse_from([
            "kux",
            "--readonly",
            "--write",
            "--headless",
            "--logoless",
            "--crumbsless",
            "--splashless",
            "--invert",
            "--screen-dump-dir",
            "/tmp/kux-screen-dumps",
        ])
        .unwrap();
        assert!(cli.read_only && cli.write);
        assert!(cli.headless && cli.no_logo && cli.crumbsless && cli.splashless && cli.invert);
        assert_eq!(
            cli.screen_dump_dir,
            Some(PathBuf::from("/tmp/kux-screen-dumps"))
        );
        let default = Cli::try_parse_from(["kux"]).unwrap();
        assert!(default.refresh.is_none());
        assert!(default.screen_dump_dir.is_none());
        assert!(default.log_level.is_none());
        assert!(default.log_file.is_none());
        assert!(!default.write && !default.headless && !default.splashless && !default.invert);

        let log_cli =
            Cli::try_parse_from(["kux", "-l", "debug", "--logFile", "/tmp/kux-debug.log"]).unwrap();
        assert_eq!(log_cli.log_level.as_deref(), Some("debug"));
        assert_eq!(log_cli.log_file, Some(PathBuf::from("/tmp/kux-debug.log")));
    }
}
