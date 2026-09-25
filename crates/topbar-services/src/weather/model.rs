//! The snapshot the whole panel reads its weather from.
//!
//! There is exactly one of these, published through one watch channel, and the
//! bar label, the forecast popover and the control panel's card all render
//! from it. v1 had two caches and a widget that shelled out to fetch a second
//! copy of the same data; the shape here is what makes that impossible.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// Which scale temperatures are reported in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TemperatureUnit {
    /// `°C`.
    #[default]
    Celsius,
    /// `°F`.
    Fahrenheit,
}

impl TemperatureUnit {
    /// Read `widgets.weather.unit`.
    ///
    /// Config validation has already rejected anything else, so an unknown
    /// value here means the panel is running unvalidated config and Celsius is
    /// the safer guess.
    pub fn from_config(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "f" | "fahrenheit" => Self::Fahrenheit,
            _ => Self::Celsius,
        }
    }

    /// What Open-Meteo calls it.
    pub fn api_value(self) -> &'static str {
        match self {
            Self::Celsius => "celsius",
            Self::Fahrenheit => "fahrenheit",
        }
    }

    /// The suffix a temperature is written with.
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Celsius => "°C",
            Self::Fahrenheit => "°F",
        }
    }
}

/// Where the weather is being read for.
#[derive(Debug, Clone, PartialEq)]
pub struct LocationView {
    /// What to show the user: a place name, or the coordinates themselves.
    pub label: String,
    /// Degrees north.
    pub latitude: f64,
    /// Degrees east.
    pub longitude: f64,
}

impl LocationView {
    /// A location for coordinates typed in by hand.
    ///
    /// An empty label falls back to the coordinates, so the header of the
    /// forecast card always says *something* about where it is reading from.
    pub fn new(label: impl Into<String>, latitude: f64, longitude: f64) -> Self {
        let label = label.into();
        let label = if label.trim().is_empty() {
            format!("{latitude:.4}, {longitude:.4}")
        } else {
            label.trim().to_string()
        };
        Self {
            label,
            latitude,
            longitude,
        }
    }
}

/// One place the geocoder found.
#[derive(Debug, Clone, PartialEq)]
pub struct GeocodeResult {
    /// `City — Region, Country`.
    pub label: String,
    /// Degrees north.
    pub latitude: f64,
    /// Degrees east.
    pub longitude: f64,
}

impl From<GeocodeResult> for LocationView {
    fn from(result: GeocodeResult) -> Self {
        Self {
            label: result.label,
            latitude: result.latitude,
            longitude: result.longitude,
        }
    }
}

/// Conditions right now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CurrentWeather {
    /// Air temperature, in the configured unit.
    pub temperature: f64,
    /// What it feels like, in the same unit.
    pub feels_like: f64,
    /// WMO weather-interpretation code.
    pub code: u16,
    /// Whether the sun is up there, which picks the day or night icon.
    pub is_day: bool,
}

/// One day of the forecast.
#[derive(Debug, Clone, PartialEq)]
pub struct DailyWeather {
    /// ISO date, `YYYY-MM-DD`. Formatted into a weekday by the GTK side, which
    /// is the side that owns `chrono` and the user's locale.
    pub date: String,
    /// WMO weather-interpretation code.
    pub code: u16,
    /// The day's high.
    pub high: f64,
    /// The day's low.
    pub low: f64,
    /// Chance of precipitation, 0..=100. `None` when the API omitted it.
    pub precipitation: Option<u8>,
}

/// Everything one successful fetch produced.
#[derive(Debug, Clone, PartialEq)]
pub struct WeatherData {
    /// Conditions now.
    pub current: CurrentWeather,
    /// The days that were asked for, today first.
    pub days: Vec<DailyWeather>,
    /// The unit every temperature above is in.
    pub unit: TemperatureUnit,
}

/// What the panel should be drawing.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Phase {
    /// Nobody has told the panel where it is.
    #[default]
    NeedsLocation,
    /// There is a location and the first fetch has not landed yet. Only ever
    /// seen once per location: a refresh keeps the data that is on screen.
    Loading,
    /// Fresh data.
    Ready(WeatherData),
    /// The last good data, and when it was fetched. The panel keeps showing it
    /// — a forecast an hour old is worth far more than an empty card — and
    /// says how old it is.
    Stale(WeatherData, SystemTime),
    /// A fetch failed and none has ever succeeded, so there is nothing to keep.
    Unavailable,
}

/// The published weather snapshot.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WeatherState {
    /// What to draw.
    pub phase: Phase,
    /// Where it is being read for, once there is a location at all.
    pub location: Option<LocationView>,
    /// The unit chosen for the next reading, including while it loads.
    pub unit: TemperatureUnit,
}

impl WeatherState {
    /// The data to render, fresh or stale.
    pub fn data(&self) -> Option<&WeatherData> {
        match &self.phase {
            Phase::Ready(data) | Phase::Stale(data, _) => Some(data),
            Phase::NeedsLocation | Phase::Loading | Phase::Unavailable => None,
        }
    }

    /// When the data on screen was fetched, if it is no longer current.
    pub fn stale_since(&self) -> Option<SystemTime> {
        match &self.phase {
            Phase::Stale(_, since) => Some(*since),
            _ => None,
        }
    }

    /// Whether there is nothing at all to show but a location was set.
    pub fn is_unavailable(&self) -> bool {
        matches!(self.phase, Phase::Unavailable)
    }
}

/// The phase a failed fetch leaves the panel in.
///
/// Keeping the last good reading is the whole of stale-while-revalidate: a
/// dropped Wi-Fi connection dims the card and adds a timestamp rather than
/// blanking it.
pub fn phase_after_failure(last_good: Option<&(WeatherData, SystemTime)>) -> Phase {
    match last_good {
        Some((data, since)) => Phase::Stale(data.clone(), *since),
        None => Phase::Unavailable,
    }
}

/// Whether a latitude/longitude pair names a point on Earth.
///
/// Ported from v1, `NaN` rejection and all: an entry box is the one place
/// these arrive from, and `"nan".parse::<f64>()` succeeds.
pub fn valid_coordinates(latitude: f64, longitude: f64) -> bool {
    latitude.is_finite()
        && longitude.is_finite()
        && (-90.0..=90.0).contains(&latitude)
        && (-180.0..=180.0).contains(&longitude)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> WeatherData {
        WeatherData {
            current: CurrentWeather {
                temperature: 21.0,
                feels_like: 20.0,
                code: 2,
                is_day: true,
            },
            days: Vec::new(),
            unit: TemperatureUnit::Celsius,
        }
    }

    #[test]
    fn the_unit_is_read_the_way_v1_read_it() {
        assert_eq!(
            TemperatureUnit::from_config("fahrenheit"),
            TemperatureUnit::Fahrenheit
        );
        assert_eq!(
            TemperatureUnit::from_config("F"),
            TemperatureUnit::Fahrenheit
        );
        assert_eq!(
            TemperatureUnit::from_config("celsius"),
            TemperatureUnit::Celsius
        );
        assert_eq!(
            TemperatureUnit::from_config("nonsense"),
            TemperatureUnit::Celsius
        );
    }

    #[test]
    fn coordinate_ranges_are_the_ones_the_planet_has() {
        assert!(valid_coordinates(0.0, 0.0));
        assert!(valid_coordinates(-90.0, 180.0));
        assert!(valid_coordinates(90.0, -180.0));
        assert!(!valid_coordinates(-90.1, 0.0));
        assert!(!valid_coordinates(0.0, 180.1));
        assert!(!valid_coordinates(f64::NAN, 0.0));
        assert!(!valid_coordinates(0.0, f64::INFINITY));
    }

    #[test]
    fn a_location_with_no_name_is_labelled_by_its_coordinates() {
        let location = LocationView::new("   ", 55.75204, 37.61781);
        assert_eq!(location.label, "55.7520, 37.6178");
    }

    #[test]
    fn a_named_location_keeps_its_name() {
        let location = LocationView::new("  Moscow  ", 55.75204, 37.61781);
        assert_eq!(location.label, "Moscow");
    }

    #[test]
    fn a_failure_with_data_behind_it_goes_stale_rather_than_blank() {
        let at = SystemTime::UNIX_EPOCH;
        let phase = phase_after_failure(Some(&(sample(), at)));
        assert_eq!(phase, Phase::Stale(sample(), at));

        let state = WeatherState {
            phase,
            location: None,
            unit: TemperatureUnit::Celsius,
        };
        assert_eq!(state.data(), Some(&sample()));
        assert_eq!(state.stale_since(), Some(at));
        assert!(!state.is_unavailable());
    }

    #[test]
    fn a_failure_with_nothing_behind_it_is_unavailable() {
        assert_eq!(phase_after_failure(None), Phase::Unavailable);

        let state = WeatherState {
            phase: Phase::Unavailable,
            location: None,
            unit: TemperatureUnit::Celsius,
        };
        assert_eq!(state.data(), None);
        assert_eq!(state.stale_since(), None);
        assert!(state.is_unavailable());
    }

    #[test]
    fn a_fresh_reading_is_not_stale() {
        let state = WeatherState {
            phase: Phase::Ready(sample()),
            location: None,
            unit: TemperatureUnit::Celsius,
        };
        assert_eq!(state.data(), Some(&sample()));
        assert_eq!(state.stale_since(), None);
    }

    #[test]
    fn a_panel_that_has_never_been_told_where_it_is_needs_a_location() {
        assert_eq!(WeatherState::default().phase, Phase::NeedsLocation);
        assert_eq!(WeatherState::default().data(), None);
    }
}
