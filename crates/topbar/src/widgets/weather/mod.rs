//! The weather widget: an icon, a temperature, and a condition.
//!
//! ```text
//!  ☁  21° Partly cloudy          click → the forecast popover
//!  ☁  21°                        show_description = false
//!     Configure…                 click → the location dialog
//! ```
//!
//! Everything it draws comes out of the one weather service, so the label, the
//! popover it opens and the control panel's weather card are the same reading
//! by construction. The widget owns no timer and no cache of its own.

pub mod dialog;
pub mod forecast;

use std::rc::Rc;
use std::time::SystemTime;

use gtk4::prelude::*;
use gtk4::{Image, Label, pango};
use topbar_core::config::WeatherConfig;
use topbar_services::weather::{condition, icon};
use topbar_services::{Phase, Services, WeatherState};

use crate::bar::BarContext;
use crate::bridge::{self, BindingGuard};
use crate::style::classes;
use crate::surfaces::popovers::{self, PopoverContent, PopoverHandle};
use crate::surfaces::tooltip::TooltipHandle;
use crate::widgets::ellipsize;
use crate::widgets::shell::WidgetShell;
use crate::widgets::weather::forecast::{Forecast, age, degrees};

/// Widget name, for CSS classes and the popover registry.
const WIDGET_NAME: &str = "weather";
/// The label shown before anyone has said where the panel is. Ported from v1,
/// down to the ellipsis meaning "this opens something".
const CONFIGURE_LABEL: &str = "Configure…";
/// Shown while the first reading for a location is on its way.
const LOADING_LABEL: &str = "…";
/// The icon standing in for a reading that never arrived.
const UNAVAILABLE_ICON: &str = "weather-severe-alert-symbolic";

/// The weather widget.
pub struct WeatherWidget {
    shell: WidgetShell,
    /// Holds the label, the tooltip, and the phase the click gesture reads.
    _inner: Rc<Inner>,
    /// The popover's claim on the host.
    _popover: PopoverHandle,
    /// Keeps the label subscribed to the service.
    _binding: BindingGuard,
}

impl WeatherWidget {
    /// Build the widget from `[widgets.weather]`.
    pub fn new(config: &WeatherConfig, context: &BarContext) -> Self {
        let shell = WidgetShell::new(classes::WEATHER);
        shell.make_interactive();

        let icon = Image::new();
        icon.add_css_class(classes::WEATHER_ICON);
        shell.content().append(&icon);

        let label = Label::new(None);
        label.set_ellipsize(pango::EllipsizeMode::End);
        shell.content().append(&label);

        let inner = Rc::new(Inner {
            wrapper: shell.root().clone(),
            icon,
            label,
            tooltip: shell.set_tooltip(&config.tooltip),
            max_chars: config.max_chars.map(|max| max as usize),
            show_description: config.show_description,
            fallback_tooltip: config.tooltip.clone(),
            needs_location: std::cell::Cell::new(true),
        });

        let binding = bridge::bind_state(shell.root(), context.services.weather.state(), {
            let inner = Rc::downgrade(&inner);
            move |_: &gtk4::Box, state: &WeatherState| {
                if let Some(inner) = inner.upgrade() {
                    inner.render(state, SystemTime::now());
                }
            }
        });

        let popover = {
            let settings = config.clone();
            let services = context.services.clone();
            popovers::attach(context, WIDGET_NAME, shell.root(), move || {
                Rc::new(Popover::new(&settings, &services)) as Rc<dyn PopoverContent>
            })
        };

        // Clicking a widget that says "Configure…" has to open the thing it is
        // asking for, not a popover repeating the request. The gesture runs in
        // the capture phase and claims the sequence, which cancels the
        // popover's own gesture before it can toggle anything.
        let setup = gtk4::GestureClick::new();
        setup.set_button(gtk4::gdk::BUTTON_PRIMARY);
        setup.set_propagation_phase(gtk4::PropagationPhase::Capture);
        setup.connect_pressed({
            let inner = Rc::downgrade(&inner);
            let settings = config.clone();
            let services = context.services.clone();
            move |gesture, _, _, _| {
                let Some(inner) = inner.upgrade() else {
                    return;
                };
                if !inner.needs_location.get() {
                    return;
                }
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                if let Some(widget) = gesture.widget() {
                    dialog::present(&settings, &services, &widget);
                }
            }
        });
        shell.root().add_controller(setup);

        // `TOPBAR_SMOKE_OPEN=weather-setup` opens the dialog for a screenshot:
        // there is no synthetic pointer in the dev shell, so a modal that only
        // a click can reach could never be photographed.
        popovers::register_smoke_action(&format!("{WIDGET_NAME}-setup"), {
            let settings = config.clone();
            let services = context.services.clone();
            let anchor = shell.root().clone();
            move || dialog::present(&settings, &services, &anchor)
        });

        Self {
            shell,
            _inner: inner,
            _popover: popover,
            _binding: binding,
        }
    }

    /// The widget to put in a bar section.
    pub fn root(&self) -> gtk4::Widget {
        self.shell.root().clone().upcast()
    }
}

/// Everything the render closure touches.
struct Inner {
    /// The shell's outer box, which is what `.disconnected` dims.
    wrapper: gtk4::Box,
    icon: Image,
    label: Label,
    tooltip: TooltipHandle,
    /// `widgets.weather.max_chars`, the width the label is cut to.
    max_chars: Option<usize>,
    /// `widgets.weather.show_description`, whether the condition is named.
    show_description: bool,
    /// `widgets.weather.tooltip`, shown until there is something better.
    fallback_tooltip: String,
    /// Whether a click should open the dialog rather than the popover.
    needs_location: std::cell::Cell<bool>,
}

impl Inner {
    /// Draw `state`.
    fn render(&self, state: &WeatherState, now: SystemTime) {
        self.needs_location.set(wants_setup(&state.phase));

        let Some(data) = state.data() else {
            self.render_without_data(state);
            return;
        };

        set_icon(&self.icon, icon(data.current.code, data.current.is_day));
        self.icon.set_visible(true);
        set_text(
            &self.label,
            &ellipsize(
                &panel_label(
                    data.current.temperature,
                    data.current.code,
                    self.show_description,
                ),
                self.max_chars,
            ),
        );

        let mut tooltip = format!(
            "{}\n{} · {}{}\nFeels like {}{}",
            state
                .location
                .as_ref()
                .map_or("Weather", |location| location.label.as_str()),
            condition(data.current.code),
            data.current.temperature.round() as i64,
            data.unit.symbol(),
            data.current.feels_like.round() as i64,
            data.unit.symbol(),
        );
        // A stale reading looks exactly like a fresh one on a panel this
        // narrow, so the tooltip is where its age is admitted to.
        if let Some(since) = state.stale_since() {
            tooltip.push('\n');
            tooltip.push_str(&age(since, now));
        }
        self.tooltip.set_text(&tooltip);

        // Dimmed only while there is nothing current to show; a stale reading
        // is still the weather, and greying it out every time a fetch slips
        // would make the panel flicker on a bad connection.
        self.set_disconnected(false);
    }

    /// Draw one of the three states with no reading behind them.
    fn render_without_data(&self, state: &WeatherState) {
        match state.phase {
            Phase::NeedsLocation => {
                self.icon.set_visible(false);
                set_text(&self.label, CONFIGURE_LABEL);
                self.tooltip
                    .set_text("Click to choose where to read the weather");
                self.set_disconnected(false);
            }
            Phase::Loading => {
                self.icon.set_visible(false);
                set_text(&self.label, LOADING_LABEL);
                self.tooltip.set_text(&self.fallback_tooltip);
                self.set_disconnected(false);
            }
            _ => {
                set_icon(&self.icon, UNAVAILABLE_ICON);
                self.icon.set_visible(true);
                set_text(&self.label, "");
                self.tooltip.set_text("The weather is unavailable");
                self.set_disconnected(true);
            }
        }
    }

    /// Wear the panel's has-no-data treatment, or take it off.
    fn set_disconnected(&self, disconnected: bool) {
        if disconnected {
            self.wrapper.add_css_class(classes::DISCONNECTED);
        } else {
            self.wrapper.remove_css_class(classes::DISCONNECTED);
        }
    }
}

/// The popover: the shared forecast component and nothing else.
struct Popover {
    forecast: Rc<Forecast>,
    root: gtk4::Box,
}

impl Popover {
    fn new(config: &WeatherConfig, services: &Services) -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        let forecast = Forecast::new(config, services);
        root.append(forecast.root());
        Self { forecast, root }
    }
}

impl PopoverContent for Popover {
    fn root(&self) -> gtk4::Widget {
        self.root.clone().upcast()
    }

    fn refresh(&self) {
        // "Updated 2h ago" goes on being wrong while the popover is closed and
        // nothing is published, so every open re-renders from the last state.
        self.forecast.refresh();
    }
}

/// The bar's own line: a temperature, and the condition when it is wanted.
///
/// `show_description = false` is for a centre section that has run out of room.
/// Nothing is lost by turning it off — the icon beside it already says what the
/// sky is doing, and the tooltip, the popover and the control panel's forecast
/// card all still name the condition in full.
fn panel_label(temperature: f64, code: u16, show_description: bool) -> String {
    let temperature = degrees(temperature);
    if show_description {
        format!("{temperature} {}", condition(code))
    } else {
        temperature
    }
}

/// Set an icon only when it changed.
fn set_icon(image: &Image, name: &str) {
    if image.icon_name().as_deref() != Some(name) {
        image.set_icon_name(Some(name));
    }
}

/// Set a label only when the text changed, which costs a bar relayout.
fn set_text(label: &Label, text: &str) {
    if label.text() != text {
        label.set_text(text);
    }
}

/// Whether a click should open the location dialog instead of the popover.
///
/// Free function so the widget's click handling has one thing to ask.
pub fn wants_setup(phase: &Phase) -> bool {
    matches!(phase, Phase::NeedsLocation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_label_is_cut_to_the_configured_width() {
        // The rule itself lives in `widgets::ellipsize`, where every widget
        // with a `max_chars` reaches it; this is the weather's own case.
        assert_eq!(
            ellipsize("-11° Thunderstorm with hail", Some(24)),
            "-11° Thunderstorm with …"
        );
    }

    #[test]
    fn the_condition_can_be_left_off_the_bar() {
        // 2 is "Partly cloudy", the longest thing a mild day produces.
        assert_eq!(panel_label(21.4, 2, true), "21° Partly cloudy");
        assert_eq!(panel_label(21.4, 2, false), "21°");
        // Still no unit symbol either way: the tooltip shows °C or °F.
        assert_eq!(panel_label(-0.4, 0, false), "0°");
    }

    #[test]
    fn only_the_no_location_phase_opens_the_dialog_on_a_click() {
        assert!(wants_setup(&Phase::NeedsLocation));
        assert!(!wants_setup(&Phase::Loading));
        assert!(!wants_setup(&Phase::Unavailable));
    }
}
