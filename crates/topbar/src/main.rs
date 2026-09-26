//! topbar — GNOME Shell-inspired GTK4 top bar for niri.
//!
//! The binary is both the panel and its command-line client: with no
//! subcommand it loads the configuration and runs the bar, and every
//! subcommand talks to a running panel over the IPC socket.

#![warn(missing_docs)]

mod anim;
mod app;
mod bar;
mod bridge;
mod chooser;
mod cli;
mod commands;
mod control;
mod fonts;
mod ipc_client;
mod pinentry;
mod reload;
mod style;
mod surfaces;
mod wayland;
mod widgets;

use std::process::ExitCode;

use clap::Parser;
use topbar_core::config::{Config, ConfigLoad, EXAMPLE_CONFIG_TOML, Warning};
use topbar_core::logging;
use topbar_services::Services;
use topbar_services::ipc::{InstanceLock, LockError};
use tracing::{info, warn};

use crate::cli::Cli;

fn main() -> ExitCode {
    // `topbar-pinentry` is a separately installed invocation of this binary. It must bypass
    // clap entirely because gpg-agent is allowed to pass pinentry-specific
    // flags that are protocol context, not topbar flags.
    if env!("CARGO_BIN_NAME") == "topbar-pinentry" || invoked_as_pinentry() {
        return pinentry::run(None);
    }
    let cli = Cli::parse();
    logging::init(cli.verbose);

    // --print-example-config is answered before any file is touched so it works
    // even when the user's config is broken.
    if cli.print_example_config {
        print!("{EXAMPLE_CONFIG_TOML}");
        return ExitCode::SUCCESS;
    }

    if let Some(command) = cli.command {
        return commands::run(command, cli.config.as_deref());
    }

    let load = match Config::find_and_load(cli.config.as_deref()) {
        Ok(load) => load,
        Err(err) => {
            eprintln!("Error: {err}");
            return ExitCode::FAILURE;
        }
    };

    // `--check-config` reports the same warnings on stderr, so logging them too
    // would print every one twice.
    if !cli.check_config {
        for warning in &load.warnings {
            warn!("{warning}");
        }
        if let Some(legacy) = &load.legacy_location {
            warn!("{legacy}");
        }
    }

    if cli.strict && !load.warnings.is_empty() {
        eprintln!(
            "Error: {}",
            topbar_core::Error::StrictWarnings(
                load.warnings.iter().map(Warning::to_string).collect()
            )
        );
        return ExitCode::FAILURE;
    }

    if cli.check_config {
        return report_check(&load);
    }

    describe(&load);

    // Before anything else claims anything: a second panel must not take the
    // notification name, put a second bar on every monitor and fight the first
    // one for the IPC socket before discovering it is the second panel. The
    // lock lives until `main` returns, and the kernel releases it however this
    // process ends.
    let _instance = match InstanceLock::acquire() {
        Ok(lock) => Some(lock),
        Err(LockError::Busy) => {
            eprintln!("{}", topbar_services::ipc::ALREADY_RUNNING);
            return ExitCode::FAILURE;
        }
        // Neither a missing `$XDG_RUNTIME_DIR` nor an unwritable one is worth
        // refusing to start over: the panel simply runs without the guard, and
        // says so, exactly as it does when it cannot bind the socket.
        Err(error) => {
            warn!("running without a single-instance lock: {error}");
            None
        }
    };

    // Services start before GTK: their runtime owns its own threads, and no
    // widget should ever be built against a service that does not exist yet.
    let services = Services::start(&load.config);
    // One subscriber to logind, and everything that goes stale while a machine
    // sleeps is told by it. Started here rather than inside `start` so the
    // bundle is complete before anything can be woken.
    services.wake_on_resume();
    app::run(load.config, cli.config, load.source, services)
}

/// Whether this binary was reached through the dedicated pinentry entrypoint.
fn invoked_as_pinentry() -> bool {
    std::env::args_os().next().is_some_and(|program| {
        std::path::Path::new(&program)
            .file_name()
            .is_some_and(|name| name == "topbar-pinentry")
    })
}

/// `--check-config` output: one status line, then every warning on stderr.
fn report_check(load: &ConfigLoad) -> ExitCode {
    match &load.source {
        Some(source) => println!("Configuration valid: {}", source.display()),
        None => println!("Configuration valid (using defaults)"),
    }
    if let Some(legacy) = &load.legacy_location {
        eprintln!("Warning: {legacy}");
    }
    for warning in &load.warnings {
        eprintln!("Warning: {warning}");
    }
    ExitCode::SUCCESS
}

fn describe(load: &ConfigLoad) {
    match &load.source {
        Some(source) => info!("Loaded configuration from {}", source.display()),
        None => warn!("No config file found; using built-in defaults"),
    }
    let widgets = &load.config.widgets;
    info!(
        "Bar {}px; widgets: {} left, {} center, {} right",
        load.config.bar.size,
        widgets.left.len(),
        widgets.center.len(),
        widgets.right.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn v1_flags_still_parse() {
        let cli = Cli::try_parse_from([
            "topbar",
            "-c",
            "/tmp/config.toml",
            "-vv",
            "--check-config",
            "--strict",
        ])
        .expect("v1 flag surface must keep working");
        assert_eq!(
            cli.config.as_deref(),
            Some(std::path::Path::new("/tmp/config.toml"))
        );
        assert_eq!(cli.verbose, 2);
        assert!(cli.check_config);
        assert!(cli.strict);
    }

    #[test]
    fn v1_subcommands_still_parse() {
        for args in [
            vec!["topbar", "volume", "get"],
            vec!["topbar", "volume", "set", "30"],
            vec!["topbar", "volume", "inc", "5"],
            vec!["topbar", "volume", "dec"],
            vec!["topbar", "volume", "mute"],
            vec!["topbar", "volume", "unmute"],
            vec!["topbar", "volume", "toggle-mute"],
            vec!["topbar", "brightness", "get"],
            vec!["topbar", "brightness", "set", "40"],
            vec!["topbar", "brightness", "inc"],
            vec!["topbar", "brightness", "dec", "10"],
            vec!["topbar", "inhibit", "toggle"],
            vec!["topbar", "media", "play-pause"],
            vec!["topbar", "media", "next"],
            vec!["topbar", "media", "previous"],
            vec!["topbar", "media", "stop"],
            vec!["topbar", "media", "status"],
            vec!["topbar", "bar", "show"],
            vec!["topbar", "bar", "hide"],
            vec!["topbar", "bar", "toggle"],
            vec!["topbar", "popover", "show", "clock"],
            vec!["topbar", "popover", "hide"],
            vec!["topbar", "popover", "toggle", "clock"],
            vec!["topbar", "launcher", "show"],
            vec!["topbar", "launcher", "hide"],
            vec!["topbar", "launcher", "toggle"],
            vec!["topbar", "pinentry"],
            vec!["topbar", "reload"],
            vec!["topbar", "dump", "default-config"],
        ] {
            Cli::try_parse_from(&args).unwrap_or_else(|err| panic!("{args:?}: {err}"));
        }
    }

    #[test]
    fn dump_takes_a_target_or_none_at_all() {
        use crate::cli::{Command, DumpAction};

        let all = Cli::try_parse_from(["topbar", "dump"]).expect("a bare dump is valid");
        assert!(matches!(
            all.command,
            Some(Command::Dump {
                action: None,
                json: false
            })
        ));

        let json = Cli::try_parse_from(["topbar", "dump", "state", "--json"])
            .expect("a target plus --json is valid");
        assert!(matches!(
            json.command,
            Some(Command::Dump {
                action: Some(DumpAction::State),
                json: true
            })
        ));
    }

    #[test]
    fn an_omitted_step_still_parses() {
        // `topbar volume inc` with no amount is what a media key is bound to.
        for args in [
            vec!["topbar", "volume", "inc"],
            vec!["topbar", "volume", "dec"],
            vec!["topbar", "brightness", "inc"],
            vec!["topbar", "brightness", "dec"],
        ] {
            Cli::try_parse_from(&args).unwrap_or_else(|err| panic!("{args:?}: {err}"));
        }
    }

    #[test]
    fn brightness_percent_is_range_checked() {
        assert!(Cli::try_parse_from(["topbar", "brightness", "set", "101"]).is_err());
    }

    #[test]
    fn example_config_is_embedded_and_parses() {
        let (config, warnings) =
            Config::parse(EXAMPLE_CONFIG_TOML).expect("embedded example must parse");
        assert_eq!(config, Config::default());
        assert!(warnings.is_empty());
    }
}
