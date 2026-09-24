//! Application start-up: GTK, the stylesheet, and the bars.

use std::cell::RefCell;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Application, gdk, gio};
use topbar_core::Config;
use topbar_services::Services;
use tracing::{error, info};

use crate::anim;
use crate::bar::{BarManager, SharedConfig};
use crate::bridge;
use crate::control;
use crate::reload;
use crate::style;
use crate::surfaces;
use crate::wayland;

/// The GApplication id.
const APP_ID: &str = "io.github.trevarj.topbar";

/// Run the panel until the last bar closes.
///
/// `services` is started before GTK so no widget can ever be built against a
/// service that does not exist yet.
pub fn run(
    config: Config,
    config_path: Option<PathBuf>,
    source: Option<PathBuf>,
    services: Services,
) -> ExitCode {
    force_wayland_backend();
    anim::set_animations_enabled(config.theme.animations);
    anim::ripple::set_enabled(config.theme.ripple);

    let config = SharedConfig::new(config);
    let app = Application::builder()
        .application_id(APP_ID)
        // Single-instance is enforced by a runtime lock file (M8), not by
        // D-Bus name ownership, so a second process must reach `main` and
        // report the conflict itself instead of silently activating the first.
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let manager: Rc<RefCell<Option<Rc<BarManager>>>> = Rc::new(RefCell::new(None));
    app.connect_activate(move |app| {
        // GTK can activate more than once; the bars are built exactly once.
        if manager.borrow().is_some() {
            return;
        }
        if let Some(started) = start(app, &config, &services, config_path.clone(), source.clone()) {
            *manager.borrow_mut() = Some(started);
        } else {
            app.quit();
        }
    });

    // Our own CLI has already parsed argv; GTK must not see it again.
    let status = app.run_with_args::<&str>(&[]);
    if status == gtk4::glib::ExitCode::SUCCESS {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Build the stylesheet and the bars. `None` means the panel cannot run here.
fn start(
    app: &Application,
    config: &SharedConfig,
    services: &Services,
    config_path: Option<PathBuf>,
    source: Option<PathBuf>,
) -> Option<Rc<BarManager>> {
    let Some(display) = gdk::Display::default() else {
        error!("no display; is a Wayland compositor running?");
        return None;
    };
    if !gtk4_layer_shell::is_supported() {
        error!("the compositor does not support wlr-layer-shell; topbar needs it");
        return None;
    }

    pin_icon_theme(&config.current().theme.icons.theme);
    style::apply(&config.current());
    // Before any surface exists: an attachment made against a manager that has
    // not been initialised is inert for good.
    wayland::blur::init(&display, config.current().theme.blur);

    // The panel's own failures are shown as banners, which means the single
    // failure sink needs the daemon before any widget exists to fail.
    bridge::install_reporter(services.notifications.handle().clone());
    // Losing the notification name is itself a failure worth reporting, and it
    // reports through exactly the same funnel every widget uses.
    let notifications = services.notifications.clone();
    bridge::act(
        bridge::ActionScope::Toast {
            widget: "notifications",
        },
        async move { notifications.startup().await },
    );

    let manager = BarManager::new(app, &display, config.clone(), services.clone());
    // Watching first, so a monitor that arrives while the first bars are being
    // built is not missed — and so the count in the first log line is the real
    // one rather than zero.
    manager.watch_monitors();
    manager.sync();
    // One apply path, two things that ask for it: the socket and the file.
    let reloader = reload::Reloader::new(services, &manager, config.clone(), config_path, source);
    reloader.watch();
    // After the bars exist: a `topbar popover show` arriving on the first
    // frame should find something to open.
    control::install(control::Panel::new(
        services,
        &manager,
        config.clone(),
        reloader,
    ));
    surfaces::popovers::install_smoke_hook();
    #[cfg(debug_assertions)]
    surfaces::dump::install();
    info!(
        "topbar is running (motion {}, blur {})",
        if anim::motion_enabled() {
            "enabled"
        } else {
            "disabled"
        },
        if wayland::blur::is_active() {
            "enabled"
        } else {
            "degraded"
        }
    );
    Some(manager)
}

/// Pin the icon theme to what `[theme.icons] theme` names.
///
/// Without this the panel renders with whatever the session's GTK settings
/// say — and on a session with no theme installed at all, with GTK's small
/// embedded subset, where common names resolve and `network-vpn-symbolic`
/// or `power-profile-*-symbolic` quietly do not. The wrapper ships Adwaita
/// (see flake.nix), so the config's default is always satisfiable.
fn pin_icon_theme(name: &str) {
    let Some(settings) = gtk4::Settings::default() else {
        return;
    };
    settings.set_gtk_icon_theme_name(Some(name));
}

/// Pin GDK to Wayland unless the user asked for something else.
///
/// The panel is layer-shell only; on a mixed session GDK would otherwise be
/// free to pick X11 and fail at `init_layer_shell`.
fn force_wayland_backend() {
    if std::env::var_os("GDK_BACKEND").is_some() {
        return;
    }
    // SAFETY: this runs before GTK is initialised and before any thread is
    // spawned, so no other thread can be reading the environment.
    unsafe { std::env::set_var("GDK_BACKEND", "wayland") };
}
