use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};

use clap::CommandFactory;

use crate::{
    cli::{Cli, CliCommand},
    config::ConfigPaths,
};

/// Run an offline subcommand, returning false when the terminal UI should start.
///
/// # Errors
/// Returns an error if configured paths are invalid or output cannot be written.
pub fn execute(cli: &Cli, output: &mut impl Write) -> io::Result<bool> {
    let Some(command) = &cli.subcommand else {
        return Ok(false);
    };
    match command {
        CliCommand::Info => {
            let paths = ConfigPaths::from_env().map_err(io::Error::other)?;
            let dumps = cli
                .screen_dump_dir
                .clone()
                .map_or_else(default_screen_dump_dir, Ok)?;
            write_info(cli, &paths, &dumps, output)?;
        }
        CliCommand::Version { short } => write_version(*short, output)?,
        CliCommand::Completion { shell } => {
            // The generator writes infallibly into memory; propagate stdout errors
            // ourselves instead of letting a closed pipe panic inside a generator.
            let mut script = Vec::new();
            clap_complete::generate(*shell, &mut Cli::command(), "kux", &mut script);
            output.write_all(&script)?;
        }
    }
    Ok(true)
}

/// Resolve the default screen-dump directory without creating it.
///
/// # Errors
/// Returns an error when neither an absolute state directory nor HOME is available.
pub fn default_screen_dump_dir() -> io::Result<PathBuf> {
    crate::screen_dumps::default_root()
}

#[must_use]
pub fn default_log_file(cli_override: Option<PathBuf>) -> PathBuf {
    if let Some(path) = cli_override {
        return path;
    }
    if let Ok(dir) = std::env::var("K9S_LOGS_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir).join("kux.log");
    }
    if let Ok(dir) = std::env::var("KUX_LOGS_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir).join("kux.log");
    }
    std::env::temp_dir().join("kux.log")
}

fn write_info(
    cli: &Cli,
    paths: &ConfigPaths,
    dumps: &Path,
    output: &mut impl Write,
) -> io::Result<()> {
    writeln!(output, "Kux {}", env!("CARGO_PKG_VERSION"))?;
    for (label, path) in [
        ("Config", paths.config_dir.join("config.yaml")),
        ("Aliases", paths.config_dir.join("aliases.yaml")),
        ("Hotkeys", paths.config_dir.join("hotkeys.yaml")),
        ("Plugins", paths.config_dir.join("plugins.yaml")),
        ("Plugin Directory", paths.config_dir.join("plugins")),
        ("Views", paths.config_dir.join("views.yaml")),
        ("Jumps", paths.config_dir.join("jumps.yaml")),
        ("Benchmarks", paths.config_dir.join("benchmarks.yaml")),
        ("Skins", paths.config_dir.join("skins")),
        ("Context Configs", paths.data_dir.join("clusters")),
        ("Screen Dumps", dumps.to_path_buf()),
        ("Logs", default_log_file(cli.log_file.clone())),
    ] {
        writeln!(output, "{label:<20} {}", path.display())?;
    }
    Ok(())
}

fn write_version(short: bool, output: &mut impl Write) -> io::Result<()> {
    if !short {
        writeln!(output, "Kux\n")?;
    }
    for (label, value) in [
        ("Version", env!("CARGO_PKG_VERSION")),
        (
            "Commit",
            option_env!("KUX_BUILD_COMMIT").unwrap_or("unknown"),
        ),
        ("Date", option_env!("KUX_BUILD_DATE").unwrap_or("unknown")),
        (
            "Target",
            option_env!("KUX_BUILD_TARGET").unwrap_or("unknown"),
        ),
    ] {
        writeln!(output, "{label:<20} {value}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::{Parser, ValueEnum};

    use super::*;

    #[test]
    fn offline_version_and_short_version_print_build_fields() {
        for args in [vec!["kux", "version"], vec!["kux", "version", "--short"]] {
            let cli = Cli::try_parse_from(&args).unwrap();
            let mut output = Vec::new();
            assert!(execute(&cli, &mut output).unwrap());
            let output = String::from_utf8(output).unwrap();
            assert_eq!(output.starts_with("Kux\n"), args.len() == 2);
            for field in ["Version", "Commit", "Date", "Target"] {
                assert!(output.contains(field));
            }
            assert!(output.contains(env!("CARGO_PKG_VERSION")));
        }
    }

    #[test]
    fn info_lists_supported_paths_without_reading_or_creating_directories() {
        let root = std::env::temp_dir().join(format!("kux-info-{}", std::process::id()));
        let paths = ConfigPaths::new(root.join("config"), root.join("data"));
        let cli = Cli::try_parse_from(["kux", "info"]).unwrap();
        let mut output = Vec::new();
        write_info(&cli, &paths, &root.join("dumps"), &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        for suffix in [
            "config/config.yaml",
            "config/plugins.yaml",
            "config/skins",
            "data/clusters",
            "dumps",
        ] {
            assert!(output.contains(root.join(suffix).to_string_lossy().as_ref()));
        }
        assert!(!root.exists());
    }

    #[test]
    fn every_supported_shell_generates_commands_and_connection_flags() {
        for shell in clap_complete::Shell::value_variants() {
            let cli = Cli::try_parse_from(["kux", "completion", &shell.to_string()]).unwrap();
            let mut output = Vec::new();
            assert!(execute(&cli, &mut output).unwrap());
            let output = String::from_utf8(output).unwrap();
            for word in ["kux", "completion", "kubeconfig", "headless"] {
                assert!(
                    output.contains(word),
                    "missing {word} in {shell} completion"
                );
            }
        }
    }

    #[test]
    fn output_failures_propagate_without_panicking() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let cli = Cli::try_parse_from(["kux", "completion", "bash"]).unwrap();
        assert_eq!(
            execute(&cli, &mut Closed).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn terminal_invocations_produce_no_offline_output() {
        let cli = Cli::try_parse_from(["kux", "--demo"]).unwrap();
        let mut output = Vec::new();
        assert!(!execute(&cli, &mut output).unwrap());
        assert_eq!(output, [] as [u8; 0]);
    }
}
