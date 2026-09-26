//! The `topbar …` subcommands.
//!
//! Two kinds of command live here, and the difference is deliberate.
//!
//! **Media keys act for themselves.** `volume`, `brightness` and `media` talk
//! to PulseAudio, logind and the session bus directly, in this process, and
//! only *afterwards* tell a panel what happened so a capsule can appear. A key
//! bound to `topbar volume up` therefore works with the panel crashed, with the
//! panel not started yet, and — because `[audio] allow_overdrive` is read on its
//! own, tolerating a file the panel would refuse — with the configuration
//! broken. This is v1's contract, kept, because it is the reason the keys were
//! reliable.
//!
//! **Everything else needs the panel**, because it *is* the panel: only the
//! process holding the layer surfaces can hide a bar or open a popover, and
//! only the process holding the inhibitor's file descriptor can let go of it.
//! Those commands print one clear line when nothing is listening.
//!
//! The OSD frame is best effort throughout: it is sent, its failure is logged
//! at debug and nothing else. A volume key that changed the volume has
//! succeeded whether or not anybody drew a picture of it — so it exits zero and
//! says nothing, which is what a key pressed sixty times an hour should do.

use std::path::Path;
use std::process::ExitCode;

use topbar_core::config::{Config, EXAMPLE_CONFIG_TOML};
use topbar_core::ipc::{self, IpcRequest, IpcResponse};
use topbar_services::Runtime;
use topbar_services::audio::DEFAULT_STEP;
use topbar_services::audio::cli::{AudioCli, CliError as AudioError};
use topbar_services::brightness::DEFAULT_STEP as BRIGHTNESS_STEP;
use topbar_services::brightness::cli::BrightnessCli;
use topbar_services::media::cli::{self as media_cli, Control};
use tracing::debug;

use crate::cli::{
    BrightnessAction, Command, DumpAction, InhibitAction, MediaAction, PopoverAction,
    VisibilityAction, VolumeAction,
};
use crate::ipc_client;

/// Run a subcommand instead of starting the panel.
pub fn run(command: Command, config_path: Option<&Path>) -> ExitCode {
    match command {
        Command::Volume { action } => volume(action, config_path),
        Command::Brightness { action } => brightness(action),
        Command::Media { action } => media(action),
        Command::Inhibit {
            action: InhibitAction::Toggle,
        } => through_panel(&IpcRequest::ToggleInhibitor),
        Command::Bar { action } => through_panel(&IpcRequest::Bar {
            action: visibility(action),
        }),
        Command::Popover { action } => through_panel(&IpcRequest::Popover {
            action: popover(action),
        }),
        Command::Launcher { action } => through_panel(&IpcRequest::Launcher {
            action: visibility(action),
        }),
        Command::Choose {
            layout,
            title,
            message,
            selected,
        } => crate::chooser::run(layout, title, message, selected, config_path),
        Command::Pinentry => crate::pinentry::run(config_path),
        Command::Reload => through_panel(&IpcRequest::Reload),
        Command::Dump { action, json } => dump(action, json),
    }
}

// ---------------------------------------------------------------------------
// Volume
// ---------------------------------------------------------------------------

/// Act on PulseAudio, then tell a panel about it.
fn volume(action: VolumeAction, config_path: Option<&Path>) -> ExitCode {
    // Read alone, and tolerant of anything: a config too broken for the panel
    // to start on must not take the volume keys down with it.
    let allow_overdrive = Config::read_audio_allow_overdrive(config_path);

    let mut audio = match AudioCli::connect(allow_overdrive) {
        Ok(audio) => audio,
        Err(error) => {
            eprintln!("Error: {error}");
            return ExitCode::FAILURE;
        }
    };

    let outcome = match action {
        VolumeAction::Get => {
            println!("{}", audio.volume());
            return ExitCode::SUCCESS;
        }
        VolumeAction::Set { percent } => audio.set_volume(percent).map(|_| ()),
        VolumeAction::Inc { amount } => audio.step_volume(step(amount)).map(|_| ()),
        VolumeAction::Dec { amount } => audio.step_volume(-step(amount)).map(|_| ()),
        VolumeAction::Mute => audio.set_muted(true),
        VolumeAction::Unmute => audio.set_muted(false),
        VolumeAction::ToggleMute => {
            let muted = audio.muted();
            audio.set_muted(!muted)
        }
    };

    match outcome {
        Ok(()) => {
            notify(&IpcRequest::VolumeChanged {
                percent: audio.volume(),
                muted: audio.muted(),
            });
            // Relative and toggling commands print where they ended up, so a
            // keybind can be checked by running it in a terminal.
            match action {
                VolumeAction::Inc { .. } | VolumeAction::Dec { .. } => {
                    println!("{}", audio.volume());
                }
                VolumeAction::ToggleMute => {
                    println!("{}", if audio.muted() { "muted" } else { "unmuted" });
                }
                _ => {}
            }
            ExitCode::SUCCESS
        }
        // A sink that exists but will not take a volume is the one failure the
        // panel draws rather than prints: the capsule says "no output device",
        // which is more use mid-presentation than a line on a terminal nobody
        // is looking at.
        Err(error @ AudioError::NotReady) => {
            notify(&IpcRequest::VolumeUnavailable);
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// A step, defaulting to five points and never wrapping.
fn step(amount: u32) -> i32 {
    let amount = if amount == 0 { DEFAULT_STEP } else { amount };
    i32::try_from(amount).unwrap_or(i32::MAX)
}

// ---------------------------------------------------------------------------
// Brightness
// ---------------------------------------------------------------------------

/// Act on the backlight, then tell a panel about it.
fn brightness(action: BrightnessAction) -> ExitCode {
    Runtime::handle().block_on(async move {
        let backlight = match BrightnessCli::open().await {
            Ok(backlight) => backlight,
            Err(error) => {
                eprintln!("Error: {error}");
                return ExitCode::FAILURE;
            }
        };

        let applied = match action {
            BrightnessAction::Get => {
                println!("{}", backlight.percent());
                return ExitCode::SUCCESS;
            }
            BrightnessAction::Set { percent } => backlight.set(percent).await,
            BrightnessAction::Inc { amount } => backlight.step(bright_step(amount)).await,
            BrightnessAction::Dec { amount } => backlight.step(-bright_step(amount)).await,
        };

        match applied {
            Ok(percent) => {
                notify(&IpcRequest::BrightnessChanged { percent });
                if !matches!(action, BrightnessAction::Set { .. }) {
                    println!("{percent}");
                }
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Error: {error}");
                ExitCode::FAILURE
            }
        }
    })
}

/// A brightness step, defaulting to five points.
fn bright_step(amount: u32) -> i32 {
    let amount = if amount == 0 { BRIGHTNESS_STEP } else { amount };
    i32::try_from(amount).unwrap_or(i32::MAX)
}

// ---------------------------------------------------------------------------
// Media
// ---------------------------------------------------------------------------

/// Act on the most relevant MPRIS player, or list them all.
fn media(action: MediaAction) -> ExitCode {
    Runtime::handle().block_on(async move {
        match action {
            MediaAction::Status => match media_cli::status().await {
                Ok(players) if players.is_empty() => {
                    println!("no media players");
                    ExitCode::SUCCESS
                }
                Ok(players) => {
                    for player in players {
                        println!("{}", player.to_line());
                    }
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("Error: {error}");
                    ExitCode::FAILURE
                }
            },
            action => {
                let control = match action {
                    MediaAction::PlayPause => Control::PlayPause,
                    MediaAction::Next => Control::Next,
                    MediaAction::Previous => Control::Previous,
                    MediaAction::Stop => Control::Stop,
                    MediaAction::Status => unreachable!("answered above"),
                };
                match media_cli::control(control).await {
                    Ok(identity) => {
                        debug!("{control:?} sent to {identity}");
                        ExitCode::SUCCESS
                    }
                    Err(error) => {
                        eprintln!("Error: {error}");
                        ExitCode::FAILURE
                    }
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Panel-only commands
// ---------------------------------------------------------------------------

/// Send a request that only a running panel can answer.
fn through_panel(request: &IpcRequest) -> ExitCode {
    match ipc_client::request(request) {
        Ok(IpcResponse::Ok) => ExitCode::SUCCESS,
        Ok(IpcResponse::Value { text }) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Ok(IpcResponse::Error { message }) => {
            eprintln!("Error: {message}");
            ExitCode::FAILURE
        }
        Ok(IpcResponse::Hello { .. }) => {
            eprintln!("Error: the panel replied with an unexpected handshake");
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("Error: {}", ipc_client::UNREACHABLE);
            debug!("{error}");
            ExitCode::FAILURE
        }
    }
}

/// Tell a panel something happened, and carry on if there is none.
///
/// Deliberately silent on failure. The command has already done what it was
/// asked; a media key that printed "could not reach topbar" on every press
/// because the panel is not running would be noise on a path that succeeded.
fn notify(request: &IpcRequest) {
    if let Err(error) = ipc_client::request(request) {
        debug!("no panel to show an OSD on: {error}");
    }
}

/// Answer `topbar dump`.
///
/// `default-config` is answered here rather than over the socket: it is the
/// compiled-in example, it cannot differ between the CLI and the panel, and
/// printing it should work with nothing running.
fn dump(action: Option<DumpAction>, json: bool) -> ExitCode {
    let target = match action {
        Some(DumpAction::DefaultConfig) if !json => {
            print!("{EXAMPLE_CONFIG_TOML}");
            return ExitCode::SUCCESS;
        }
        Some(DumpAction::DefaultConfig) => ipc::DumpTarget::DefaultConfig,
        Some(DumpAction::Config) => ipc::DumpTarget::Config,
        Some(DumpAction::State) => ipc::DumpTarget::State,
        None => ipc::DumpTarget::All,
    };
    through_panel(&IpcRequest::Dump { target, json })
}

/// Translate the CLI's show/hide/toggle into the protocol's.
fn visibility(action: VisibilityAction) -> ipc::VisibilityAction {
    match action {
        VisibilityAction::Show => ipc::VisibilityAction::Show,
        VisibilityAction::Hide => ipc::VisibilityAction::Hide,
        VisibilityAction::Toggle => ipc::VisibilityAction::Toggle,
    }
}

/// The same, for popovers.
fn popover(action: PopoverAction) -> ipc::PopoverAction {
    match action {
        PopoverAction::Show { widget } => ipc::PopoverAction::Show(widget),
        PopoverAction::Hide { widget } => ipc::PopoverAction::Hide(widget),
        PopoverAction::Toggle { widget } => ipc::PopoverAction::Toggle(widget),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use topbar_services::audio::max_volume_percent;

    #[test]
    fn an_omitted_step_is_five_points() {
        assert_eq!(step(0), 5);
        assert_eq!(bright_step(0), 5);
        assert_eq!(step(12), 12);
        assert_eq!(bright_step(12), 12);
    }

    #[test]
    fn an_absurd_step_saturates_rather_than_wrapping() {
        assert_eq!(step(u32::MAX), i32::MAX);
        assert!(-step(u32::MAX) < 0);
    }

    #[test]
    fn the_ceiling_follows_the_overdrive_policy() {
        // The live config leaves overdrive off, so `topbar volume set 150`
        // lands on 100 rather than deafening anybody.
        assert_eq!(max_volume_percent(false), 100);
        assert!(max_volume_percent(true) > 100);
    }

    #[test]
    fn the_visibility_and_popover_actions_map_one_for_one() {
        assert_eq!(
            visibility(VisibilityAction::Toggle),
            ipc::VisibilityAction::Toggle
        );
        assert_eq!(
            popover(PopoverAction::Show {
                widget: "clock".into()
            }),
            ipc::PopoverAction::Show("clock".into())
        );
        assert_eq!(
            popover(PopoverAction::Hide { widget: None }),
            ipc::PopoverAction::Hide(None)
        );
    }
}
