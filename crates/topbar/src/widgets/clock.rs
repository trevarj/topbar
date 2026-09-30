//! The clock: a strftime-formatted label that ticks on the boundary.
//!
//! The tick is scheduled to land on the next minute (or second, when the
//! format asks for seconds) rather than every 60 s from start-up, so the
//! displayed time never lags the wall clock and re-aligns itself after the
//! machine resumes from sleep.

use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
use std::rc::{Rc, Weak};
use std::time::Duration;

use chrono::{DateTime, Datelike, Local, NaiveDate, Timelike};
use gtk4::prelude::*;
use gtk4::{Calendar, Image, Label, glib};
use topbar_core::config::{ClockConfig, WeatherConfig};
use tracing::warn;

use crate::bar::BarContext;
use crate::bridge::{self, BindingGuard};
use crate::style::{classes, icons};
use crate::surfaces::popovers::{self, PopoverContent, PopoverHandle};
use crate::surfaces::tooltip::{self, TooltipHandle};
use crate::widgets::control_panel::ControlPanel;
use crate::widgets::shell::WidgetShell;

/// Widget name, for CSS classes and the popover registry.
const WIDGET_NAME: &str = "clock";
/// Format used for the tooltip: the full date the bar has no room for.
const TOOLTIP_FORMAT: &str = "%A, %B %-d, %Y";
/// Fallback when the configured format string cannot be rendered.
const FALLBACK_FORMAT: &str = "%a %b %-d  %H:%M";
/// Shown beside the time while Do Not Disturb is on.
const DND_ICON: &str = "notifications-disabled-symbolic";

/// The clock widget.
pub struct ClockWidget {
    shell: WidgetShell,
    /// Holds the ticking state; dropping it cancels the timer.
    _inner: Rc<ClockInner>,
    /// The control panel's claim on the popover host, when it has one.
    _popover: Option<PopoverHandle>,
    /// Keeps the two notification indicators subscribed.
    _indicators: BindingGuard,
}

impl ClockWidget {
    /// Build a clock from `[widgets.clock]`.
    ///
    /// `weather` comes along because the control panel's last card is the
    /// forecast, and the date menu shows it whether or not the weather widget
    /// is in the bar.
    pub fn new(config: &ClockConfig, weather: &WeatherConfig, context: &BarContext) -> Self {
        let shell = WidgetShell::new(classes::CLOCK);

        let label = Label::new(None);
        shell.content().append(&label);

        // GNOME puts the Do Not Disturb state on the date menu's own button,
        // which is where a user looks when the desktop has gone quiet and they
        // want to know whether that is on purpose.
        let dnd = Image::from_icon_name(DND_ICON);
        dnd.add_css_class(classes::CLOCK_DND);
        dnd.set_visible(false);
        shell.content().append(&dnd);

        // And beside it, GNOME's messages indicator: the panel is behind this
        // button and nothing else on the bar says there is anything in it, so
        // without the dot a notification that arrives while the user is looking
        // elsewhere leaves no trace at all. It trails the bell rather than
        // splitting the time from it, so switching Do Not Disturb on does not
        // shift the dot sideways.
        //
        // The two are deliberately independent: Do Not Disturb silences the
        // banner, it does not throw the notification away, and "quiet on
        // purpose, and three things arrived" is a state worth being able to see.
        let unseen = Image::from_icon_name(icons::UNSEEN_NOTIFICATIONS);
        unseen.add_css_class(classes::CLOCK_UNSEEN);
        unseen.set_visible(false);
        shell.content().append(&unseen);

        let indicators =
            bridge::bind_state(shell.content(), context.services.notifications.state(), {
                let dnd = dnd.clone();
                let unseen = unseen.clone();
                let control_panel = config.control_panel;
                move |_: &gtk4::Box, state| {
                    set_visible(&dnd, state.dnd);
                    set_visible(&unseen, shows_unseen(control_panel, state.unseen_count));
                }
            });

        let calendar = tooltip_calendar(config.show_week_numbers);
        let tooltip = tooltip::attach_with_child(shell.root(), "", &calendar);
        let inner = Rc::new(ClockInner {
            label,
            tooltip,
            calendar,
            date: Cell::new(None),
            anchor: shell.root().clone().upcast(),
            format: config.format.clone(),
            per_second: needs_seconds(&config.format),
            timer: RefCell::new(None),
            minute: Cell::new(u32::MAX),
            listeners: RefCell::new(Vec::new()),
        });
        inner.tick();

        // A clock with no control panel is a label: it must not offer a
        // pointer cursor or light up under the mouse.
        let popover = config.control_panel.then(|| {
            shell.make_interactive();
            let settings = config.clone();
            let weather = weather.clone();
            let services = context.services.clone();
            let clock = Rc::downgrade(&inner);
            popovers::attach(context, WIDGET_NAME, shell.root(), move || {
                let panel = ControlPanel::new(&settings, &weather, &services);
                if let Some(clock) = clock.upgrade() {
                    clock.listeners.borrow_mut().push(Rc::downgrade(&panel));
                }
                panel as Rc<dyn PopoverContent>
            })
        });

        Self {
            shell,
            _inner: inner,
            _popover: popover,
            _indicators: indicators,
        }
    }

    /// The widget to put in a bar section.
    pub fn root(&self) -> gtk4::Widget {
        self.shell.root().clone().upcast()
    }
}

struct ClockInner {
    label: Label,
    tooltip: TooltipHandle,
    calendar: Calendar,
    date: Cell<Option<NaiveDate>>,
    anchor: gtk4::Widget,
    format: String,
    per_second: bool,
    timer: RefCell<Option<glib::SourceId>>,
    /// The minute the listeners were last told about.
    minute: Cell<u32>,
    /// Control panels sharing this clock's tick. The panel shows the same
    /// wall clock the bar does, and a timer of its own would drift against
    /// the bar's — one would repaint a fraction of a second after the other,
    /// which is visible when they sit on the same screen. Held weakly: they
    /// belong to whatever built them, and a closed popover must not keep one
    /// alive.
    listeners: RefCell<Vec<Weak<ControlPanel>>>,
}

impl ClockInner {
    /// Render the current time and arm the next tick.
    fn tick(self: &Rc<Self>) {
        let now = Local::now();
        self.render(now);
        self.notify(now);
        self.schedule(now);
    }

    /// Pass a minute boundary on to whoever is sharing this tick.
    ///
    /// A clock formatted with seconds ticks every second; the listeners still
    /// only hear about minutes, which is all any of them display.
    fn notify(&self, now: DateTime<Local>) {
        if self.minute.replace(now.minute()) == now.minute() {
            return;
        }
        // Upgrade first, then call: a listener is free to do whatever it likes
        // while the list is not borrowed, including subscribing another one.
        let live: Vec<Rc<ControlPanel>> = {
            let mut listeners = self.listeners.borrow_mut();
            listeners.retain(|listener| listener.strong_count() > 0);
            listeners.iter().filter_map(Weak::upgrade).collect()
        };
        for panel in live {
            panel.on_minute(now);
        }
    }

    /// Update the label, but only when the string actually changed — a
    /// needless `set_text` costs a relayout of the whole bar.
    fn render(&self, now: DateTime<Local>) {
        let text = render(&self.format, now).unwrap_or_else(|| {
            warn!(
                "widgets.clock.format `{}` is not a valid strftime format; using `{FALLBACK_FORMAT}`",
                self.format
            );
            render(FALLBACK_FORMAT, now).unwrap_or_default()
        });
        if self.label.text() != text {
            self.label.set_text(&text);
        }
        let today = now.date_naive();
        if self.date.get() != Some(today) {
            // Calendar consumes civil date fields; UTC is only a date carrier,
            // not a conversion of the clock's local day.
            if let Ok(date) = glib::DateTime::from_utc(
                today.year(),
                today.month() as i32,
                today.day() as i32,
                0,
                0,
                0.0,
            ) {
                self.calendar.select_day(&date);
            }
            if let Some(date) = render(TOOLTIP_FORMAT, now) {
                self.anchor
                    .update_property(&[gtk4::accessible::Property::Description(&date)]);
                self.tooltip.set_text(&date);
            }
            self.date.set(Some(today));
        }
    }

    /// Arm a one-shot timer for the next boundary.
    ///
    /// Re-arming from inside the callback (instead of a repeating source)
    /// keeps every tick aligned: drift and suspend gaps are corrected on the
    /// following tick rather than accumulating.
    fn schedule(self: &Rc<Self>, now: DateTime<Local>) {
        let delay = next_tick_delay(now.second(), now.nanosecond(), self.per_second);
        let weak: Weak<Self> = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(delay, move || {
            if let Some(inner) = weak.upgrade() {
                // The source has already fired; forget it before re-arming so
                // `Drop` cannot try to remove a dead source.
                *inner.timer.borrow_mut() = None;
                inner.tick();
            }
        });
        *self.timer.borrow_mut() = Some(source);
    }
}

impl Drop for ClockInner {
    fn drop(&mut self) {
        if let Some(source) = self.timer.borrow_mut().take() {
            source.remove();
        }
    }
}

fn tooltip_calendar(show_week_numbers: bool) -> Calendar {
    let calendar = Calendar::new();
    // ponytail: the full-date label is the heading; omit unusable navigation.
    calendar.set_show_heading(false);
    calendar.set_show_day_names(true);
    calendar.set_show_week_numbers(show_week_numbers);
    calendar.set_can_target(false);
    calendar.set_focusable(false);
    calendar
}

/// Whether the unread dot belongs on the bar.
///
/// A clock with no control panel is a label: it opens nothing, so it also never
/// marks anything as seen, and a dot there would light up on the first
/// notification of the session and stay lit for the rest of it. Free function
/// so the rule can be read — and tested — without a display.
fn shows_unseen(control_panel: bool, unseen: usize) -> bool {
    control_panel && unseen > 0
}

/// Show or hide an indicator, without a needless property notification.
fn set_visible(indicator: &Image, visible: bool) {
    if indicator.is_visible() != visible {
        indicator.set_visible(visible);
    }
}

/// Format `now`, or `None` when the format string is invalid.
///
/// `DelayedFormat::to_string` panics on a bad format; writing through
/// `write!` turns that into an error the caller can recover from.
fn render(format: &str, now: DateTime<Local>) -> Option<String> {
    let mut out = String::new();
    write!(out, "{}", now.format(format)).ok()?;
    Some(out)
}

/// Whether a format string displays seconds, and so needs a per-second tick.
///
/// Only real conversion specifiers count: `%%S` is a literal percent followed
/// by an `S`, not a seconds field.
fn needs_seconds(format: &str) -> bool {
    let mut chars = format.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            continue;
        }
        // Skip strftime's padding and locale modifiers to reach the specifier.
        let mut next = chars.next();
        while matches!(next, Some('-' | '_' | '0' | '^' | '#' | 'E' | 'O')) {
            next = chars.next();
        }
        match next {
            // %S seconds, %T = %H:%M:%S, %X locale time, %r 12-hour clock time,
            // %c full date and time, %s Unix timestamp, %f fractional seconds.
            Some('S' | 'T' | 'X' | 'r' | 'c' | 's' | 'f') => return true,
            None => return false,
            _ => {}
        }
    }
    false
}

/// Time until the next tick boundary.
fn next_tick_delay(second: u32, nanosecond: u32, per_second: bool) -> Duration {
    // A leap second reports 60 (and nanoseconds above 1e9); clamp both so the
    // arithmetic cannot underflow.
    let millis_into_second = nanosecond.min(999_999_999) / 1_000_000;
    let remaining = if per_second {
        1_000 - millis_into_second
    } else {
        (60 - second.min(59)) * 1_000 - millis_into_second
    };
    Duration::from_millis(u64::from(remaining.max(1)))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    #[ignore = "requires a GTK display; run under Xvfb with --ignored --exact"]
    fn calendar_tooltip_tracks_local_date_rollover() {
        gtk4::init().expect("GTK display");
        for show_week_numbers in [false, true] {
            let shell = WidgetShell::new(classes::CLOCK);
            let calendar = tooltip_calendar(show_week_numbers);
            let inner = ClockInner {
                label: Label::new(None),
                tooltip: tooltip::attach_with_child(shell.root(), "", &calendar),
                calendar,
                date: Cell::new(None),
                anchor: shell.root().clone().upcast(),
                format: "%H:%M".to_string(),
                per_second: false,
                timer: RefCell::new(None),
                minute: Cell::new(0),
                listeners: RefCell::new(Vec::new()),
            };
            let window = gtk4::Window::new();
            window.set_child(Some(&inner.calendar));
            window.present();
            while glib::MainContext::default().pending() {
                glib::MainContext::default().iteration(false);
            }
            assert!(inner.calendar.is_mapped());
            assert_eq!(inner.calendar.shows_week_numbers(), show_week_numbers);
            // All use the same minute, including a resume-sized jump.
            for (year, month, day) in [
                (2023, 12, 31),
                (2024, 1, 1),
                (2024, 2, 28),
                (2024, 2, 29),
                (2024, 3, 1),
            ] {
                let now = Local
                    .with_ymd_and_hms(year, month, day, 12, 0, 0)
                    .single()
                    .expect("unambiguous local date");
                inner.render(now);
                let selected = inner.calendar.date();
                assert_eq!(
                    (selected.year(), selected.month(), selected.day_of_month()),
                    (year, month as i32, day as i32),
                );
            }
            window.close();
            window.set_child(None::<&gtk4::Widget>);
        }
    }

    #[test]
    fn detects_formats_that_show_seconds() {
        for format in ["%H:%M:%S", "%T", "%-S", "%X", "%r", "%c", "%s", "%H:%M %S"] {
            assert!(needs_seconds(format), "{format} shows seconds");
        }
    }

    #[test]
    fn detects_formats_that_do_not_show_seconds() {
        for format in [
            "%A, %b %d  %H:%M",
            "%a %b %-d  %H:%M",
            "%H:%M",
            "100%%Score",
            "%",
            "",
        ] {
            assert!(!needs_seconds(format), "{format} does not show seconds");
        }
    }

    #[test]
    fn the_unread_dot_needs_something_unread_and_somewhere_to_read_it() {
        assert!(shows_unseen(true, 1));
        assert!(shows_unseen(true, 12));
        assert!(!shows_unseen(true, 0), "nothing new, no dot");
        // Nothing here opens, so nothing here would ever clear the dot again.
        assert!(!shows_unseen(false, 3), "no control panel, no dot");
    }

    #[test]
    fn minute_ticks_land_on_the_boundary() {
        assert_eq!(
            next_tick_delay(0, 0, false),
            Duration::from_millis(60 * 1000)
        );
        assert_eq!(
            next_tick_delay(30, 250_000_000, false),
            Duration::from_millis(29_750)
        );
        assert_eq!(
            next_tick_delay(59, 999_000_000, false),
            Duration::from_millis(1)
        );
    }

    #[test]
    fn second_ticks_land_on_the_boundary() {
        assert_eq!(next_tick_delay(12, 0, true), Duration::from_millis(1_000));
        assert_eq!(
            next_tick_delay(12, 400_000_000, true),
            Duration::from_millis(600)
        );
    }

    #[test]
    fn leap_second_does_not_underflow() {
        assert!(next_tick_delay(60, 1_500_000_000, false) >= Duration::from_millis(1));
    }

    #[test]
    fn renders_the_live_config_format() {
        let now = Local
            .with_ymd_and_hms(2026, 8, 4, 9, 5, 0)
            .single()
            .expect("unambiguous local time");
        assert_eq!(
            render("%A, %b %d  %H:%M", now).as_deref(),
            Some("Tuesday, Aug 04  09:05")
        );
        assert_eq!(
            render(TOOLTIP_FORMAT, now).as_deref(),
            Some("Tuesday, August 4, 2026")
        );
    }

    #[test]
    fn invalid_format_is_reported_not_panicked() {
        let now = Local
            .with_ymd_and_hms(2026, 8, 4, 9, 5, 0)
            .single()
            .expect("unambiguous local time");
        assert_eq!(render("%Q", now), None);
    }
}
