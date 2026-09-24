//! Applying a changed configuration to a running panel.
//!
//! Two things ask for a reload — `topbar reload` over the socket, and the
//! watcher on the configuration file — and both land in [`Reloader::apply`].
//! One path means one behaviour: whatever a `topbar reload` does to a panel is
//! exactly what saving the file in an editor does.
//!
//! # What it costs
//!
//! Everything here is a route, chosen by [`ConfigDelta`]:
//!
//! | changed | done |
//! |---|---|
//! | theme/palette colours, fonts, widget styling | swap CSS/polarity and refresh affected cached-color components |
//! | `theme.animations` / `ripple` | flip the motion switches |
//! | `theme.blur` | re-bind (or release) the blur protocol, rebuild the bars |
//! | one `[widgets.<name>]` section | rebuild that widget on every bar |
//! | bar geometry, placement, Pango rendering | rebuild bars; compositor selector changes reject reload |
//! | `[osd]` | rebuild each bar's capsule |
//! | `[audio]`, `[updates]`, per-widget intervals | tell the service |
//!
//! The cheap routes are common: an accent touches CSS and OSD only,
//! and a changed `clock.format` touches one widget. Nothing rebuilds a bar
//! that does not have to, because a rebuilt bar closes whatever popover was
//! open and restarts every widget's timers.
//!
//! # What it refuses to do
//!
//! A file that does not parse, or that validates with errors, changes
//! **nothing**. The running configuration stays exactly as it was, the failure
//! is reported as one banner naming the first error, and the details go to the
//! log. There is no partial application: half a configuration is a state
//! nobody asked for and nobody can reason about.

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use topbar_core::config::ConfigLoad;
use topbar_core::{Config, ConfigDelta};
use topbar_services::{Runtime, Services};
use tracing::{debug, info, warn};

use crate::bar::{BarManager, SharedConfig};
use crate::bridge::{self, ActionScope};
use crate::{anim, style, wayland};

/// How long to let a burst of file events settle before reading the file.
///
/// Editors do not write configuration files once. `vim` writes a backup,
/// renames it over the original and truncates it in between; `helix` and VS
/// Code write, then `chmod`. Reading on the first event routinely reads an
/// empty file, which then fails to validate and shows the user a banner about a
/// mistake they did not make.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// Everything a reload has to reach.
#[derive(Clone)]
pub struct Reloader {
    services: Services,
    manager: Rc<BarManager>,
    config: SharedConfig,
    /// The `--config` path, so a reload reads the same file the panel started
    /// from rather than whatever the search chain now prefers.
    config_path: Option<PathBuf>,
    /// The file the panel actually started from, which is what is watched.
    source: Option<PathBuf>,
}

fn check_startup_settings(previous: &Config, candidate: &Config) -> Result<(), String> {
    if previous.advanced.compositor != candidate.advanced.compositor {
        return Err("advanced.compositor is startup-only; restart topbar to change it (no settings applied)".into());
    }
    Ok(())
}

impl Reloader {
    /// Describe the running panel.
    pub fn new(
        services: &Services,
        manager: &Rc<BarManager>,
        config: SharedConfig,
        config_path: Option<PathBuf>,
        source: Option<PathBuf>,
    ) -> Self {
        Self {
            services: services.clone(),
            manager: Rc::clone(manager),
            config,
            config_path,
            source,
        }
    }

    /// Re-read the configuration and apply whatever changed.
    ///
    /// The returned string is what `topbar reload` prints; the error is what it
    /// prints instead, and what the watcher turns into a banner.
    pub fn apply(&self) -> Result<String, String> {
        let load = Config::find_and_load(self.config_path.as_deref())
            .map_err(|error| first_line(&error.to_string()))?;
        for warning in &load.warnings {
            warn!("{warning}");
        }
        self.install(load)
    }

    /// Reload whenever the configuration file changes.
    ///
    /// The file is watched *and so is its directory*: an editor saving a file
    /// usually does not write it, it writes a new one and renames it over the
    /// old, which leaves an inode watch pointing at something nobody will ever
    /// write to again. Watching the directory as well catches the rename, and
    /// filtering both down to one path keeps every other file in
    /// `~/.config/topbar/` out of it.
    ///
    /// The reading and the parsing happen off the main thread; only the apply
    /// runs on it. A panel does not stutter because somebody saved a file.
    pub fn watch(&self) {
        let Some(source) = self.source.clone() else {
            debug!("no configuration file to watch; running on defaults");
            return;
        };

        let (events, mut queue) = topbar_services::mpsc::channel::<()>(8);
        let watched = source.clone();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let Ok(event) = event else {
                return;
            };
            if !event.paths.iter().any(|path| path == &watched) {
                return;
            }
            if !matches!(
                event.kind,
                notify::EventKind::Create(_)
                    | notify::EventKind::Modify(_)
                    | notify::EventKind::Remove(_)
            ) {
                return;
            }
            // A full queue is a burst still being drained, which is exactly
            // what the debounce below is for: dropping one is free.
            let _ = events.try_send(());
        });
        let mut watcher = match watcher {
            Ok(watcher) => watcher,
            Err(error) => {
                warn!("configuration changes will not be noticed: {error}");
                return;
            }
        };

        let mut watching = false;
        for path in [Some(source.as_path()), source.parent()]
            .into_iter()
            .flatten()
        {
            match notify::Watcher::watch(&mut watcher, path, notify::RecursiveMode::NonRecursive) {
                Ok(()) => watching = true,
                Err(error) => debug!("not watching {}: {error}", path.display()),
            }
        }
        if !watching {
            warn!("configuration changes will not be noticed: nothing could be watched");
            return;
        }
        info!("watching {} for changes", source.display());

        let reloader = self.clone();
        let config_path = self.config_path.clone();
        gtk4::glib::spawn_future_local(async move {
            // The watcher lives exactly as long as this task, which is as long
            // as the panel: dropping it stops the notifications.
            let _watcher = watcher;
            while queue.recv().await.is_some() {
                // Let the burst finish. An editor writes a backup, renames it
                // over the original and adjusts its mode; reading on the first
                // event reads a file that is empty or half-written, which then
                // fails to validate and tells the user about a mistake they did
                // not make.
                gtk4::glib::timeout_future(DEBOUNCE).await;
                while queue.try_recv().is_ok() {}

                let path = config_path.clone();
                let (done, wait) = topbar_services::oneshot::channel();
                Runtime::handle().spawn_blocking(move || {
                    let _ = done.send(Config::find_and_load(path.as_deref()));
                });
                let Ok(loaded) = wait.await else {
                    continue;
                };

                match loaded {
                    Ok(load) => {
                        for warning in &load.warnings {
                            warn!("{warning}");
                        }
                        match reloader.install(load) {
                            Ok(message) => info!("{message}"),
                            Err(error) => bridge::announce(
                                &format!("config error: {}", first_line(&error)),
                                &error,
                            ),
                        }
                    }
                    // The running configuration is left exactly as it was. One
                    // banner, the first error, and the whole list in the log.
                    Err(error) => {
                        bridge::announce(
                            &format!("config error: {}", first_line(&error.to_string())),
                            &error.to_string(),
                        );
                    }
                }
            }
        });
    }

    /// Apply an already-loaded configuration, and say what that took.
    fn install(&self, load: ConfigLoad) -> Result<String, String> {
        let previous = self.config.current();
        check_startup_settings(&previous, &load.config)?;
        let delta = ConfigDelta::between(&previous, &load.config);
        let source = match &load.source {
            Some(path) => path.display().to_string(),
            None => "built-in defaults".to_string(),
        };

        if delta.is_empty() {
            info!("reloaded {source}; nothing changed");
            return Ok(format!("reloaded {source} (nothing changed)"));
        }
        info!("reloading {source}: {delta}");

        // The shared cell first: everything built from here on — a rebuilt
        // widget, a bar for a monitor that arrives mid-reload — has to read the
        // new values, and the alternative is passing the new config down every
        // path by hand and getting one of them wrong.
        let config = load.config;
        self.config.replace(config.clone());

        // Services before widgets, so a widget built in a moment subscribes to
        // something that is already running.
        let started = self.services.start_if_needed(&config);
        if !started.is_empty() {
            info!(
                "the reload started the {} service",
                started.join(" and the ")
            );
        }
        self.services.sync_custom(&previous, &config);
        self.reconfigure_services(&previous, &config, &delta);

        if delta.style {
            style::apply(&config);
        }
        if delta.motion {
            anim::set_animations_enabled(config.theme.animations);
            anim::ripple::set_enabled(config.theme.ripple);
        }

        // Blur is not CSS and an attachment is never mutated: one attachment
        // owns one surface's effect object for that surface's lifetime. So the
        // protocol is re-bound (or released) and every surface is made again.
        let blur_needs_rebuild = delta.blur && self.reconfigure_blur(config.theme.blur);

        if delta.rebuilds_bars() || blur_needs_rebuild {
            self.manager.rebuild();
        } else {
            if !delta.widgets.is_empty() {
                let rebuilt = self.manager.rebuild_widgets(&delta.widgets);
                debug!("{rebuilt} widget(s) rebuilt across every bar");
            }
            if delta.osd {
                self.manager.reconfigure_osd();
            }
        }

        Ok(format!("reloaded {source} ({delta})"))
    }

    /// Switch blur on or off under a running panel.
    ///
    /// Returns whether the bars have to be rebuilt for it to be visible, which
    /// is "yes" whenever anything actually changed: a surface asks for its
    /// region once, when it is built.
    fn reconfigure_blur(&self, wanted: bool) -> bool {
        let Some(display) = gtk4::gdk::Display::default() else {
            return false;
        };
        let active = wayland::blur::set_enabled(&display, wanted);
        if wanted && !active {
            // The compositor does not offer the protocol, or the environment
            // switched it off. Nothing to rebuild for: every attachment would
            // be inert anyway, and the panel is identical minus the blur.
            info!("theme.blur is on, but blur is not available here");
            return false;
        }
        true
    }

    /// Hand the changed sections to the services that own them.
    ///
    /// A service with a `configure` seam is told rather than restarted: the
    /// weather keeps the forecast it has while re-timing, the crypto service
    /// keeps its prices, and the resource sampler keeps its CPU delta. Only the
    /// update check starts over, because which command it runs at all is
    /// decided from the whole section at once.
    fn reconfigure_services(&self, previous: &Config, config: &Config, delta: &ConfigDelta) {
        let services = self.services.clone();
        let changed = |name: &str| delta.widgets.contains(name);

        if changed("weather") {
            let handle = services.weather.handle().clone();
            let settings = topbar_services::weather::Settings::from_config(&config.widgets.weather);
            bridge::act(ActionScope::Toast { widget: "weather" }, async move {
                handle.configure(settings).await
            });
        }
        if changed("crypto") {
            let handle = services.crypto.handle().clone();
            let interval = topbar_services::crypto::interval(&config.widgets.crypto);
            // The entries too, not just the interval: they live in the
            // service's snapshot rather than being read by the widget, so a
            // rebuilt widget on its own would draw the old list. The service
            // decides whether the file's list or the user's own wins.
            let seed =
                topbar_services::crypto::resolve_entries(None, &config.widgets.crypto.entries);
            bridge::act(ActionScope::Toast { widget: "crypto" }, async move {
                handle.configure(interval, seed).await
            });
        }
        if changed("notmuch") {
            // The query and the interval together decide what runs at all, so
            // the service starts its count over rather than editing one field.
            let notmuch = services.notmuch.clone();
            let settings = config.widgets.notmuch.clone();
            Runtime::handle().spawn(async move { notmuch.configure(&settings).await });
        }
        if changed("system_monitor") {
            let handle = services.resources.handle().clone();
            let interval = Duration::from_secs(config.widgets.system_monitor.interval.max(1));
            Runtime::handle().spawn(async move { handle.configure(interval).await });
        }
        if changed("headset") {
            let headset = services.headset.clone();
            let settings = config.widgets.headset.clone();
            Runtime::handle().spawn(async move { headset.configure(&settings).await });
        }
        if delta.updates {
            let updates = services.updates.clone();
            let settings = config.updates.clone();
            Runtime::handle().spawn(async move { updates.configure(&settings).await });
        }
        if delta.audio && previous.audio.allow_overdrive != config.audio.allow_overdrive {
            let handle = services.audio.handle().clone();
            let allow = config.audio.allow_overdrive;
            bridge::act(ActionScope::Toast { widget: "audio" }, async move {
                handle.set_allow_overdrive(allow).await
            });
        }
    }
}

/// The first line of a multi-line error, for a banner that has to fit.
///
/// A validation failure lists every problem in the file, which is right for a
/// terminal and wrong for a notification: the log has all of them, the banner
/// names the first and says how many more there are.
fn first_line(message: &str) -> String {
    let mut lines = message.lines().filter(|line| !line.trim().is_empty());
    let Some(first) = lines.next() else {
        return "the configuration could not be read".to_string();
    };
    let rest = lines.count();
    if rest == 0 {
        first.to_string()
    } else {
        format!("{first} (and {rest} more)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compositor_changes_reject_the_entire_candidate_before_installation() {
        let previous = Config::default();
        let mut candidate = previous.clone();
        candidate.theme.mode = "light".into();
        candidate.advanced.compositor = "hyprland".into();
        assert!(check_startup_settings(&previous, &candidate).is_err());
        candidate.advanced.compositor = previous.advanced.compositor.clone();
        assert!(check_startup_settings(&previous, &candidate).is_ok());
    }

    #[test]
    fn a_one_line_failure_is_reported_as_it_is() {
        assert_eq!(
            first_line("bar.size: must be greater than 0"),
            "bar.size: must be greater than 0"
        );
    }

    #[test]
    fn a_list_of_failures_names_the_first_and_counts_the_rest() {
        let message = "invalid configuration:\n  bar.size: must be greater than 0\n  \
                       theme.accent: invalid value";
        assert_eq!(first_line(message), "invalid configuration: (and 2 more)");
    }

    #[test]
    fn an_empty_failure_still_says_something() {
        assert!(!first_line("").is_empty());
        assert!(!first_line("\n\n").is_empty());
    }
}
