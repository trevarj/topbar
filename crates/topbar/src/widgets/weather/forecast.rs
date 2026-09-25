//! The forecast, drawn once and mounted twice.
//!
//! ```text
//! ┌──────────────────────────────────────────────┐
//! │ Weather                                  ⚙   │  title, gear
//! │ Moscow — Moscow, Russia                      │  location
//! │  ☁  21°   Partly cloudy · Feels like 20°     │  current
//! │ Today  🌧  Rain            24° / 13°   💧70% │  one row per day
//! │ Wed    ☁   Overcast        22° / 12°         │
//! │ …                                            │
//! │ Updated 2h ago · Retry                       │  only while stale
//! └──────────────────────────────────────────────┘
//! ```
//!
//! The same component is the control panel's weather card and the whole of the
//! weather widget's popover. There is one of these per mount and one weather
//! service behind all of them, so the card and the popover can never disagree
//! about what the weather is — which is exactly what v1's second cache made
//! possible.
//!
//! Every row exists from the first frame and is hidden rather than removed, so
//! a forecast arriving does not change the height of the control panel's
//! column.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::SystemTime;

use gtk4::prelude::*;
use gtk4::{Align, Button, Image, Label, Orientation, pango};
use topbar_core::config::WeatherConfig;
use topbar_services::weather::{condition, icon};
use topbar_services::{Phase, Services, WeatherState};

use crate::bridge::{self, ActionScope, BindingGuard};
use crate::style::classes;
use crate::widgets::weather::dialog;

/// Where this component's failures are reported.
const SCOPE: ActionScope = ActionScope::Toast { widget: "weather" };
/// The gear that opens the location dialog.
const CONFIGURE_ICON: &str = "preferences-system-symbolic";
/// The droplet standing in front of a chance of rain. Adwaita has no drop of
/// its own, and this is the glyph its weather set uses for falling water.
const PRECIPITATION_ICON: &str = "weather-showers-symbolic";
/// Shown in the empty state before a location has been chosen.
const NO_LOCATION_ICON: &str = "find-location-symbolic";
/// Shown in the empty state when a fetch has never succeeded.
const UNAVAILABLE_ICON: &str = "weather-severe-alert-symbolic";

/// The forecast component.
pub struct Forecast {
    root: gtk4::Box,
    location: Label,
    current: gtk4::Box,
    current_icon: Image,
    current_temp: Label,
    current_condition: Label,
    days: gtk4::Box,
    rows: Vec<Row>,
    empty: gtk4::Box,
    empty_icon: Image,
    empty_label: Label,
    empty_button: Button,
    stale: gtk4::Box,
    stale_label: Label,
    state: RefCell<Rc<WeatherState>>,
    _bindings: RefCell<Vec<BindingGuard>>,
}

impl Forecast {
    /// Build a forecast bound to the weather service.
    ///
    /// `config` decides how many day rows exist; they are all created here and
    /// never created again.
    pub fn new(config: &WeatherConfig, services: &Services) -> Rc<Self> {
        let root = gtk4::Box::new(Orientation::Vertical, 6);
        root.add_css_class(classes::FORECAST);

        // --- header ---------------------------------------------------------
        let header = gtk4::Box::new(Orientation::Horizontal, 8);
        header.add_css_class(classes::FORECAST_HEADER);

        let titles = gtk4::Box::new(Orientation::Vertical, 1);
        titles.set_hexpand(true);

        let title = Label::new(Some("Weather"));
        title.add_css_class(classes::CARD_TITLE);
        title.set_xalign(0.0);

        let location = Label::new(None);
        location.add_css_class(classes::FORECAST_LOCATION);
        location.set_xalign(0.0);
        location.set_ellipsize(pango::EllipsizeMode::End);

        titles.append(&title);
        titles.append(&location);

        let configure = icon_button(CONFIGURE_ICON, classes::FORECAST_CONFIGURE);
        configure.set_tooltip_text(Some("Change weather location and temperature unit"));
        configure.set_valign(Align::Start);
        configure.connect_clicked({
            let services = services.clone();
            let config = config.clone();
            move |button| dialog::present(&config, &services, button)
        });

        header.append(&titles);
        header.append(&configure);
        root.append(&header);

        // --- current conditions ---------------------------------------------
        let current = gtk4::Box::new(Orientation::Horizontal, 10);
        current.add_css_class(classes::FORECAST_CURRENT);

        let current_icon = Image::new();
        current_icon.add_css_class(classes::FORECAST_CURRENT_ICON);

        let current_temp = Label::new(None);
        current_temp.add_css_class(classes::FORECAST_CURRENT_TEMP);

        let current_condition = Label::new(None);
        current_condition.add_css_class(classes::FORECAST_CURRENT_CONDITION);
        current_condition.set_xalign(0.0);
        current_condition.set_hexpand(true);
        current_condition.set_wrap(true);

        current.append(&current_icon);
        current.append(&current_temp);
        current.append(&current_condition);
        root.append(&current);

        // --- the days -------------------------------------------------------
        let days = gtk4::Box::new(Orientation::Vertical, 2);
        days.add_css_class(classes::FORECAST_DAYS);
        let rows: Vec<Row> = (0..config.forecast_days.clamp(3, 5))
            .map(|_| {
                let row = Row::new();
                days.append(&row.root);
                row
            })
            .collect();
        root.append(&days);

        // --- the two empty states -------------------------------------------
        let empty = gtk4::Box::new(Orientation::Vertical, 8);
        empty.add_css_class(classes::EMPTY_STATE);
        empty.set_valign(Align::Center);
        empty.set_vexpand(true);

        let empty_icon = Image::new();
        empty_icon.add_css_class(classes::EMPTY_STATE_ICON);

        let empty_label = Label::new(None);
        empty_label.add_css_class(classes::EMPTY_STATE_LABEL);
        empty_label.set_justify(gtk4::Justification::Center);

        let empty_button = Button::new();
        empty_button.add_css_class(classes::DIALOG_BUTTON);
        empty_button.set_halign(Align::Center);

        empty.append(&empty_icon);
        empty.append(&empty_label);
        empty.append(&empty_button);
        root.append(&empty);

        // --- the stale footer -----------------------------------------------
        let stale = gtk4::Box::new(Orientation::Horizontal, 6);
        stale.add_css_class(classes::FORECAST_STALE);

        let stale_label = Label::new(None);
        stale_label.set_xalign(0.0);
        stale_label.set_hexpand(true);

        let retry = Button::with_label("Retry");
        retry.add_css_class(classes::FORECAST_RETRY);
        retry.connect_clicked({
            let services = services.clone();
            move |_| refresh_now(&services)
        });

        stale.append(&stale_label);
        stale.append(&retry);
        root.append(&stale);

        let forecast = Rc::new(Self {
            root,
            location,
            current,
            current_icon,
            current_temp,
            current_condition,
            days,
            rows,
            empty,
            empty_icon,
            empty_label,
            empty_button,
            stale,
            stale_label,
            state: RefCell::new(Rc::new(WeatherState::default())),
            _bindings: RefCell::new(Vec::new()),
        });

        // The empty state's button does one of two things depending on which
        // empty state it is, and the state is what says which.
        forecast.empty_button.connect_clicked({
            let weak = Rc::downgrade(&forecast);
            let services = services.clone();
            let config = config.clone();
            move |button| {
                let Some(forecast) = weak.upgrade() else {
                    return;
                };
                if forecast.state.borrow().is_unavailable() {
                    refresh_now(&services);
                } else {
                    dialog::present(&config, &services, button);
                }
            }
        });

        let binding = bridge::bind_state(&forecast.root, services.weather.state(), {
            let weak = Rc::downgrade(&forecast);
            move |_: &gtk4::Box, state: &WeatherState| {
                if let Some(forecast) = weak.upgrade() {
                    forecast.render(state, SystemTime::now());
                }
            }
        });
        forecast._bindings.borrow_mut().push(binding);

        forecast
    }

    /// The widget to put in a card slot or a popover.
    pub fn root(&self) -> &gtk4::Box {
        &self.root
    }

    /// Re-render from the state the service last published.
    ///
    /// Called on every popover open, because "Updated 2h ago" stops being true
    /// while the popover is closed even though nothing has been published.
    pub fn refresh(&self) {
        let state = Rc::clone(&self.state.borrow());
        self.render(&state, SystemTime::now());
    }

    /// Draw `state`.
    fn render(&self, state: &WeatherState, now: SystemTime) {
        *self.state.borrow_mut() = Rc::new(state.clone());

        set_text(
            &self.location,
            state
                .location
                .as_ref()
                .map_or("No location set", |location| location.label.as_str()),
        );

        let Some(data) = state.data() else {
            self.render_empty(&state.phase);
            return;
        };

        show(&self.empty, false);
        show(&self.current, true);
        show(&self.days, true);

        let unit_icon = icon(data.current.code, data.current.is_day);
        set_icon(&self.current_icon, unit_icon);
        set_text(
            &self.current_temp,
            &format!(
                "{}{}",
                data.current.temperature.round() as i64,
                data.unit.symbol()
            ),
        );
        set_text(
            &self.current_condition,
            &format!(
                "{}\nFeels like {}{}",
                condition(data.current.code),
                data.current.feels_like.round() as i64,
                data.unit.symbol()
            ),
        );

        for (index, row) in self.rows.iter().enumerate() {
            match data.days.get(index) {
                Some(day) => row.render(index, day),
                None => show(&row.root, false),
            }
        }

        match state.stale_since() {
            Some(since) => {
                show(&self.stale, true);
                set_text(&self.stale_label, &age(since, now));
            }
            None => show(&self.stale, false),
        }
    }

    /// Draw one of the two states that have no forecast behind them.
    fn render_empty(&self, phase: &Phase) {
        show(&self.current, false);
        show(&self.days, false);
        show(&self.stale, false);
        show(&self.empty, true);

        let (name, message, action) = match phase {
            Phase::Unavailable => (UNAVAILABLE_ICON, "Weather unavailable", Some("Retry")),
            // Loading only happens before the first reading for a location, so
            // there is never anything on screen for it to replace.
            Phase::Loading => (NO_LOCATION_ICON, "Fetching the forecast…", None),
            _ => (NO_LOCATION_ICON, "Set a location", Some("Set location…")),
        };

        set_icon(&self.empty_icon, name);
        set_text(&self.empty_label, message);
        match action {
            Some(label) => {
                show(&self.empty_button, true);
                if self.empty_button.label().as_deref() != Some(label) {
                    self.empty_button.set_label(label);
                }
            }
            None => show(&self.empty_button, false),
        }
    }
}

/// One day of the forecast.
struct Row {
    root: gtk4::Box,
    day: Label,
    icon: Image,
    condition: Label,
    temps: Label,
    precipitation_icon: Image,
    precipitation: Label,
}

impl Row {
    fn new() -> Self {
        let root = gtk4::Box::new(Orientation::Horizontal, 8);
        root.add_css_class(classes::FORECAST_ROW);

        let day = Label::new(None);
        day.add_css_class(classes::FORECAST_DAY);
        day.set_xalign(0.0);
        day.set_width_chars(5);

        let icon = Image::new();
        icon.add_css_class(classes::FORECAST_ICON);

        let condition = Label::new(None);
        condition.add_css_class(classes::FORECAST_CONDITION);
        condition.set_xalign(0.0);
        condition.set_hexpand(true);
        condition.set_ellipsize(pango::EllipsizeMode::End);

        let temps = Label::new(None);
        temps.add_css_class(classes::FORECAST_TEMPS);
        temps.set_xalign(1.0);

        let precipitation_icon = Image::from_icon_name(PRECIPITATION_ICON);
        precipitation_icon.add_css_class(classes::FORECAST_PRECIPITATION);

        let precipitation = Label::new(None);
        precipitation.add_css_class(classes::FORECAST_PRECIPITATION);
        precipitation.set_xalign(1.0);
        precipitation.set_width_chars(4);

        root.append(&day);
        root.append(&icon);
        root.append(&condition);
        root.append(&temps);
        root.append(&precipitation_icon);
        root.append(&precipitation);

        Self {
            root,
            day,
            icon,
            condition,
            temps,
            precipitation_icon,
            precipitation,
        }
    }

    fn render(&self, index: usize, day: &topbar_services::DailyWeather) {
        show(&self.root, true);
        set_text(&self.day, &weekday(index, &day.date));
        // A daily code has no daylight to it: the forecast for Wednesday is
        // about Wednesday, not about Wednesday night.
        set_icon(&self.icon, icon(day.code, true));
        set_text(&self.condition, condition(day.code));
        set_text(
            &self.temps,
            &format!("{} / {}", degrees(day.high), degrees(day.low)),
        );

        // Zero is a real forecast and a droplet beside it would say the
        // opposite of what it means.
        let chance = day.precipitation.filter(|chance| *chance > 0);
        match chance {
            Some(chance) => {
                show(&self.precipitation_icon, true);
                show(&self.precipitation, true);
                set_text(&self.precipitation, &format!("{chance}%"));
            }
            None => {
                show(&self.precipitation_icon, false);
                show(&self.precipitation, false);
            }
        }
    }
}

/// A temperature, rounded, with its degree sign.
///
/// Forecast rows and the narrow bar omit the scale because the current
/// reading identifies it; the full symbol is drawn without another `°`.
pub fn degrees(value: f64) -> String {
    format!("{}°", value.round() as i64)
}

/// The label for a forecast row. Ported from v1.
fn weekday(index: usize, date: &str) -> String {
    if index == 0 {
        return "Today".to_string();
    }
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map_or_else(|_| date.to_string(), |date| date.format("%a").to_string())
}

/// How long ago a stale reading was taken.
pub fn age(since: SystemTime, now: SystemTime) -> String {
    let Ok(elapsed) = now.duration_since(since) else {
        // The clock moved backwards. "In the future" is not a thing to tell a
        // user about the weather.
        return "Updated just now".to_string();
    };
    let minutes = elapsed.as_secs() / 60;
    match minutes {
        0 => "Updated just now".to_string(),
        1..=59 => format!("Updated {minutes}m ago"),
        60..=1439 => format!("Updated {}h ago", minutes / 60),
        _ => format!("Updated {}d ago", minutes / 1440),
    }
}

/// A flat, round icon button.
fn icon_button(name: &str, class: &str) -> Button {
    let button = Button::new();
    button.add_css_class(class);
    button.set_child(Some(&Image::from_icon_name(name)));
    button
}

/// Ask the service for a fresh reading.
fn refresh_now(services: &Services) {
    let handle = services.weather.handle().clone();
    bridge::act(SCOPE, async move { handle.refresh_now().await });
}

/// Set an icon only when it changed: a needless `set_icon_name` reloads it.
fn set_icon(image: &Image, name: &str) {
    if image.icon_name().as_deref() != Some(name) {
        image.set_icon_name(Some(name));
    }
}

/// Set a label only when the text changed, which costs a relayout otherwise.
fn set_text(label: &Label, text: &str) {
    if label.text() != text {
        label.set_text(text);
    }
}

/// Show or hide, only when it is a change.
fn show(widget: &impl IsA<gtk4::Widget>, visible: bool) {
    let widget = widget.as_ref();
    if widget.is_visible() != visible {
        widget.set_visible(visible);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_first_row_is_today_and_the_rest_are_weekdays() {
        assert_eq!(weekday(0, "2026-08-04"), "Today");
        assert_eq!(weekday(1, "2026-08-05"), "Wed");
        assert_eq!(weekday(4, "2026-08-08"), "Sat");
    }

    #[test]
    fn a_date_the_api_never_sends_is_shown_rather_than_swallowed() {
        assert_eq!(weekday(1, "not-a-date"), "not-a-date");
    }

    #[test]
    fn temperatures_are_rounded_to_whole_degrees() {
        assert_eq!(degrees(21.4), "21°");
        assert_eq!(degrees(21.5), "22°");
        assert_eq!(degrees(-0.4), "0°");
        assert_eq!(degrees(-3.7), "-4°");
    }

    #[test]
    fn a_stale_reading_says_how_old_it_is_in_the_largest_unit_that_fits() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000_000);
        let ago = |seconds| age(now - Duration::from_secs(seconds), now);

        assert_eq!(ago(0), "Updated just now");
        assert_eq!(ago(59), "Updated just now");
        assert_eq!(ago(60), "Updated 1m ago");
        assert_eq!(ago(59 * 60), "Updated 59m ago");
        assert_eq!(ago(60 * 60), "Updated 1h ago");
        assert_eq!(ago(2 * 60 * 60 + 30 * 60), "Updated 2h ago");
        assert_eq!(ago(23 * 60 * 60), "Updated 23h ago");
        assert_eq!(ago(24 * 60 * 60), "Updated 1d ago");
        assert_eq!(ago(50 * 60 * 60), "Updated 2d ago");
    }

    #[test]
    fn a_clock_that_moved_backwards_does_not_produce_a_future_reading() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        assert_eq!(
            age(now + Duration::from_secs(3600), now),
            "Updated just now"
        );
    }
}
