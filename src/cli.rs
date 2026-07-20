use std::time::Duration;

use clap::Parser;

use crate::model::ResourceKind;

/// A fast, watch-driven Kubernetes terminal UI.
#[derive(Debug, Clone, Parser)]
#[allow(clippy::struct_excessive_bools)]
#[command(name = "kux", version, about)]
pub struct Cli {
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
    #[arg(long)]
    pub read_only: bool,

    /// Start without the Kux logo. Toggle it at runtime with Ctrl-L.
    #[arg(long)]
    pub no_logo: bool,
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
