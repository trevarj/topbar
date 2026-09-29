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

use std::cell::Cell;
use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;
use std::rc::Rc;

use gio::prelude::*;
use topbar_core::config::{Config, EXAMPLE_CONFIG_TOML};
use topbar_core::ipc::{self, IpcRequest, IpcResponse};
use topbar_services::Runtime;
use topbar_services::audio::DEFAULT_STEP;
use topbar_services::audio::cli::{AudioCli, CliError as AudioError};
use topbar_services::brightness::DEFAULT_STEP as BRIGHTNESS_STEP;
use topbar_services::brightness::cli::BrightnessCli;
use topbar_services::media::cli::{self as media_cli, Control};
use tracing::debug;
use zbus::zvariant::Value;

use crate::cli::{
    BrightnessAction, Command, DumpAction, InhibitAction, MediaAction, PopoverAction,
    VisibilityAction, VolumeAction,
};
use crate::ipc_client;

/// The desktop-entry activation interface; keep the wire signature typed.
#[zbus::proxy(interface = "org.freedesktop.Application", assume_defaults = false)]
trait Application {
    fn activate(&self, platform_data: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
}

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
        Command::LaunchDesktop { desktop_id } => launch_desktop(&desktop_id),
        Command::Choose {
            layout,
            title,
            message,
            selected,
            wallpaper_provider,
        } => crate::chooser::run(
            layout,
            title,
            message,
            selected,
            wallpaper_provider,
            config_path,
        ),
        Command::Pinentry => crate::pinentry::run(config_path),
        Command::Reload => through_panel(&IpcRequest::Reload),
        Command::Dump { action, json } => dump(action, json),
    }
}

fn launch_desktop(desktop_id: &str) -> ExitCode {
    let Some(info) = gio_unix::DesktopAppInfo::new(desktop_id) else {
        eprintln!("Could not find application desktop ID: {desktop_id}");
        return ExitCode::FAILURE;
    };
    match launch_desktop_info(&info) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Could not launch {desktop_id}: {error}");
            ExitCode::FAILURE
        }
    }
}

fn launch_desktop_info(info: &gio_unix::DesktopAppInfo) -> Result<(), String> {
    if let Some(name) = dbus_application_name(info) {
        let token = std::env::var("XDG_ACTIVATION_TOKEN").ok();
        return Runtime::handle()
            .block_on(async {
                let connection = zbus::Connection::session().await?;
                activate_desktop(&connection, &name, token.as_deref()).await
            })
            .map_err(|error| error.to_string());
    }

    launch_exec(info).map_err(|error| error.to_string())
}

fn dbus_application_name(info: &gio_unix::DesktopAppInfo) -> Option<String> {
    if !info.boolean("DBusActivatable") {
        return None;
    }
    // GIO uses the *filename*, not the desktop ID (which includes nested
    // directory prefixes), to derive the well-known D-Bus name.
    let filename = info.filename()?;
    let name = filename.file_name()?.to_str()?.strip_suffix(".desktop")?;
    zbus::names::WellKnownName::try_from(name).ok()?;
    Some(name.to_owned())
}

async fn activate_desktop(
    connection: &zbus::Connection,
    name: &str,
    token: Option<&str>,
) -> zbus::Result<()> {
    let mut path = String::with_capacity(name.len() + 1);
    path.push('/');
    path.extend(name.chars().map(|ch| match ch {
        '.' => '/',
        '-' => '_',
        ch => ch,
    }));
    let proxy = ApplicationProxy::builder(connection)
        .destination(name)?
        .path(path)?
        .build()
        .await?;
    let mut platform_data = HashMap::new();
    if let Some(token) = token.filter(|token| !token.is_empty()) {
        platform_data.insert("activation-token", Value::from(token));
        platform_data.insert("desktop-startup-id", Value::from(token));
    }
    proxy.activate(platform_data).await
}

fn launch_exec(info: &gio_unix::DesktopAppInfo) -> Result<(), gio::glib::Error> {
    // Keep the scoped helper alive until asynchronous GIO launch has finished;
    // the synchronous API can report success before Exec launch fails.
    let context = gio::glib::MainContext::new();
    context
        .with_thread_default(|| {
            let main_loop = gio::glib::MainLoop::new(Some(&context), false);
            let outcome = Rc::new(Cell::new(None));
            info.launch_uris_async(&[], gio::AppLaunchContext::NONE, gio::Cancellable::NONE, {
                let main_loop = main_loop.clone();
                let outcome = outcome.clone();
                move |result| {
                    outcome.set(Some(result));
                    main_loop.quit();
                }
            });
            main_loop.run();
            outcome.take().expect("GIO launch completed")
        })
        .expect("new GIO main context is available")
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

    struct PrivateBus(std::process::Child);

    impl Drop for PrivateBus {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    struct FakeApplication(std::sync::mpsc::Sender<HashMap<String, zbus::zvariant::OwnedValue>>);

    #[zbus::interface(name = "org.freedesktop.Application")]
    impl FakeApplication {
        fn activate(&self, platform_data: HashMap<String, zbus::zvariant::OwnedValue>) {
            self.0.send(platform_data).expect("test is receiving");
        }
    }

    #[test]
    fn dbus_activation_sends_the_parent_token_to_the_filename_derived_application() {
        use std::io::BufRead;

        let mut command = std::process::Command::new("dbus-daemon");
        if let Some(config) = std::env::var_os("TOPBAR_TEST_DBUS_CONFIG") {
            command.arg("--config-file").arg(config);
        } else {
            command.arg("--session");
        }
        let Ok(mut child) = command
            .args(["--print-address", "--nofork"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            eprintln!("skipping: no dbus-daemon available");
            return;
        };
        let mut address = String::new();
        let read = std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut address);
        let _bus = PrivateBus(child);
        if read.is_err() || !address.starts_with("unix:") {
            eprintln!("skipping: private bus unavailable");
            return;
        }
        let root = std::env::temp_dir().join(format!(
            "topbar-token-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = root.join("nested/org.example.Token-Test.desktop");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "[Desktop Entry]\nType=Application\nName=Example\nExec=true\nDBusActivatable=true\n",
        )
        .unwrap();
        let info = gio_unix::DesktopAppInfo::from_filename(&path).unwrap();
        let name = dbus_application_name(&info).expect("valid filename-derived name");
        assert_eq!(name, "org.example.Token-Test");

        Runtime::handle().block_on(async {
            let (tx, rx) = std::sync::mpsc::channel();
            let server = zbus::connection::Builder::address(address.trim())
                .unwrap()
                .name(name.as_str())
                .unwrap()
                .serve_at("/org/example/Token_Test", FakeApplication(tx))
                .unwrap()
                .build()
                .await
                .unwrap();
            let client = zbus::connection::Builder::address(address.trim())
                .unwrap()
                .build()
                .await
                .unwrap();
            activate_desktop(&client, &name, Some("test-wayland-activation-token"))
                .await
                .unwrap();
            let data = rx.try_recv().expect("application received Activate");
            for key in ["activation-token", "desktop-startup-id"] {
                assert_eq!(
                    String::try_from(data[key].try_clone().unwrap()).unwrap(),
                    "test-wayland-activation-token"
                );
            }
            drop(server);
        });
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scoped_helper_waits_for_real_desktop_launch_errors() {
        let root = std::env::temp_dir().join(format!(
            "topbar-launch-desktop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for (relative, activatable) in [
            ("org.example.App.desktop", false),
            ("org.example/app.desktop", true),
            ("1nested/org.example.App.desktop", false),
        ] {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                format!(
                    "[Desktop Entry]\nType=Application\nName=Example\nExec=true\nPath={}\nDBusActivatable={activatable}\n",
                    root.join("missing").display()
                ),
            )
            .unwrap();
            let info = gio_unix::DesktopAppInfo::from_filename(&path)
                .unwrap_or_else(|| panic!("invalid desktop fixture: {}", path.display()));
            assert_eq!(
                dbus_application_name(&info),
                None,
                "{relative} must use GIO Exec fallback"
            );
            assert!(launch_desktop_info(&info).is_err(), "{relative}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

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
