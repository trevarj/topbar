//! The weather: one cache, three surfaces.
//!
//! ```text
//!   model.rs   the published snapshot                     (pure)
//!   wmo.rs     code -> words and an Adwaita icon name     (pure)
//!   api.rs     the two Open-Meteo URLs and their bodies   (pure + I/O)
//!   import.rs  v1's saved coordinates, read once          (pure + I/O)
//!   task.rs    the one owner of all of it
//! ```
//!
//! The bar label, the forecast popover and the control panel's weather card
//! all render from the single [`WeatherState`] this service publishes. v1 kept
//! two caches and had the control panel shell out for a second copy of the
//! same forecast; there is nowhere here for that to happen.
//!
//! When to fetch next is [`crate::refresh`], which the crypto service shares.
//!
//! Refreshing is stale-while-revalidate throughout: a failed fetch keeps the
//! last good reading on screen with the time it was taken, retries on a
//! backoff, and never blanks the card. A machine that is offline does not
//! retry at all — it waits for [`Connectivity`](crate::connectivity) and
//! fetches the moment the network is back.

mod api;
mod import;
mod model;
mod task;
mod wmo;

#[cfg(test)]
mod bus_tests;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};
use topbar_core::config::WeatherConfig;
use tracing::info;

use crate::connectivity::Connectivity;
use crate::error::SvcError;
use crate::lazy::Deferred;
use crate::state_store::StateStore;

pub use api::Endpoints;
pub use model::{
    CurrentWeather, DailyWeather, GeocodeResult, LocationView, Phase, TemperatureUnit, WeatherData,
    WeatherState, valid_coordinates,
};
pub use wmo::{condition, icon};

use task::Command;

/// How many commands may be in flight before a sender waits.
const QUEUE: usize = 8;
/// The narrowest forecast the config allows.
const MIN_DAYS: u32 = 3;
/// The widest.
const MAX_DAYS: u32 = 5;

/// The weather choices `state.json` keeps.
///
/// The panel never writes popup choices back into the user's `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedWeather {
    /// Where the user last told the panel it is.
    pub location: Option<PersistedLocation>,
    /// A popup choice takes precedence over the config's unit, even on reload.
    pub unit: Option<TemperatureUnit>,
}

/// One saved location.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedLocation {
    /// What to call it on screen.
    pub label: String,
    /// Degrees north.
    pub latitude: f64,
    /// Degrees east.
    pub longitude: f64,
}

/// The parts of `[widgets.weather]` the service acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// Which temperature scale to ask for.
    pub unit: TemperatureUnit,
    /// How long between refreshes.
    pub interval: Duration,
    /// How many days of forecast to ask for.
    pub days: u32,
}

impl Settings {
    /// Read them out of the configuration.
    pub fn from_config(config: &WeatherConfig) -> Self {
        Self {
            unit: TemperatureUnit::from_config(&config.unit),
            interval: Duration::from_secs(config.interval),
            days: config.forecast_days.clamp(MIN_DAYS, MAX_DAYS),
        }
    }
}

/// The weather service.
///
/// Cloning is cheap — a channel sender and a watch subscription — so the bar
/// widget, its popover and the control panel each hold their own copy of the
/// same one cache.
#[derive(Clone)]
pub struct Weather {
    handle: WeatherHandle,
    state: watch::Receiver<Arc<WeatherState>>,
    task: Deferred,
}

impl Weather {
    /// Start the service from the configuration and what was remembered.
    ///
    /// `wanted` is whether anything draws weather: the `weather` widget, or the
    /// clock's control panel with its forecast card. A panel with neither asks
    /// Open-Meteo for nothing until a reload adds one.
    pub(crate) fn start(
        config: &WeatherConfig,
        persisted: PersistedWeather,
        store: StateStore,
        connectivity: &Connectivity,
        wanted: bool,
    ) -> Self {
        let startup = startup_location(persisted.location, config, import::from_v1);
        if startup.persist
            && let Some(location) = &startup.location
        {
            let saved = PersistedLocation {
                label: location.label.clone(),
                latitude: location.latitude,
                longitude: location.longitude,
            };
            info!("saving the imported weather location so it is only imported once");
            store.update(move |state| state.weather.location = Some(saved));
        }

        Self::spawn(
            Settings::from_config(config),
            persisted.unit,
            Endpoints::from_env(),
            Some(store),
            connectivity.state(),
            startup.location,
            wanted,
        )
    }

    /// Start the task if it was held back. Returns whether this call did it.
    pub(crate) fn ensure_started(&self) -> bool {
        self.task.start()
    }

    /// The same, with everything named explicitly. Tests use this to point the
    /// service at a local listener and a bus of their own.
    #[cfg(test)]
    pub(crate) fn start_with(
        settings: Settings,
        endpoints: Endpoints,
        store: Option<StateStore>,
        connectivity: watch::Receiver<Arc<crate::connectivity::ConnectivityState>>,
        location: Option<LocationView>,
    ) -> Self {
        Self::spawn(
            settings,
            None,
            endpoints,
            store,
            connectivity,
            location,
            true,
        )
    }

    fn spawn(
        settings: Settings,
        unit_override: Option<TemperatureUnit>,
        endpoints: Endpoints,
        store: Option<StateStore>,
        connectivity: watch::Receiver<Arc<crate::connectivity::ConnectivityState>>,
        location: Option<LocationView>,
        wanted: bool,
    ) -> Self {
        let (commands, queue) = mpsc::channel(QUEUE);
        let (publisher, state) = watch::channel(Arc::new(WeatherState {
            unit: unit_override.unwrap_or(settings.unit),
            ..WeatherState::default()
        }));
        let task = Deferred::spawn(
            wanted,
            task::run(
                queue,
                publisher,
                settings,
                unit_override,
                endpoints,
                store,
                connectivity,
                location,
            ),
        );
        Self {
            handle: WeatherHandle { commands },
            state,
            task,
        }
    }

    /// The handle commands are sent through.
    pub fn handle(&self) -> &WeatherHandle {
        &self.handle
    }

    /// Subscribe to the weather.
    pub fn state(&self) -> watch::Receiver<Arc<WeatherState>> {
        self.state.clone()
    }
}

/// What the panel may ask of the weather service.
#[derive(Clone)]
pub struct WeatherHandle {
    commands: mpsc::Sender<Command>,
}

impl WeatherHandle {
    /// Fetch now, whatever the schedule said.
    ///
    /// The Retry button in the forecast card, and — once M12's lifecycle
    /// service exists — `lifecycle.on_resume`, which is when a laptop opened
    /// after a night asleep has the most obviously wrong forecast on it.
    // M12: lifecycle.on_resume -> refresh_now
    pub async fn refresh_now(&self) -> Result<(), SvcError> {
        self.send(Command::Refresh).await
    }

    /// Look up to five places up by name.
    ///
    /// An empty list means the query matched nothing, which the dialog says in
    /// as many words; an error means the request itself failed.
    pub async fn search(&self, query: String) -> Result<Vec<GeocodeResult>, SvcError> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Search(query, reply)).await?;
        answer
            .await
            .map_err(|_| SvcError::ServiceStopped("weather"))?
    }

    /// Read the weather for a place the search found.
    pub async fn set_location(&self, result: GeocodeResult) -> Result<(), SvcError> {
        self.set(result.into()).await
    }

    /// Read the weather for coordinates typed in by hand.
    pub async fn set_manual(
        &self,
        latitude: f64,
        longitude: f64,
        label: String,
    ) -> Result<(), SvcError> {
        if !valid_coordinates(latitude, longitude) {
            return Err(SvcError::Coordinates(format!("{latitude}, {longitude}")));
        }
        self.set(LocationView::new(label, latitude, longitude))
            .await
    }

    /// Apply a changed `[widgets.weather]` section.
    ///
    /// A popup unit choice wins over this section; otherwise a different unit
    /// invalidates the cache. A different forecast length does so as well.
    pub async fn configure(&self, settings: Settings) -> Result<(), SvcError> {
        self.send(Command::Configure(settings)).await
    }

    /// Save the temperature scale chosen in the popup.
    pub async fn set_unit(&self, unit: TemperatureUnit) -> Result<(), SvcError> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::SetUnit(unit, reply)).await?;
        answer
            .await
            .map_err(|_| SvcError::ServiceStopped("weather"))?
    }

    /// Save a location and refetch for it.
    async fn set(&self, location: LocationView) -> Result<(), SvcError> {
        if !valid_coordinates(location.latitude, location.longitude) {
            return Err(SvcError::Coordinates(format!(
                "{}, {}",
                location.latitude, location.longitude
            )));
        }
        let (reply, answer) = oneshot::channel();
        self.send(Command::SetLocation(location, reply)).await?;
        answer
            .await
            .map_err(|_| SvcError::ServiceStopped("weather"))?
    }

    /// Post a command, or report that the service has stopped.
    async fn send(&self, command: Command) -> Result<(), SvcError> {
        self.commands
            .send(command)
            .await
            .map_err(|_| SvcError::ServiceStopped("weather"))
    }
}

/// The location the panel starts with, and whether it has to be written down.
#[derive(Debug, PartialEq)]
struct Startup {
    location: Option<LocationView>,
    /// True only for a location imported from v1, which is saved once so the
    /// import never runs again.
    persist: bool,
}

/// Resolve where the panel reads the weather for, in priority order.
///
/// 1. What the setup dialog last saved. A choice the user made by hand beats
///    everything, which is what makes the dialog able to override the config.
/// 2. `widgets.weather.latitude` / `.longitude`, when both are present and on
///    Earth.
/// 3. v1's saved coordinates, imported once so an in-place upgrade does not
///    send the user back through the dialog. Reached only when neither of the
///    above supplied a location: an old cache file must never shadow a
///    coordinate the user wrote in their config on purpose.
/// 4. Nothing, which is `NeedsLocation` and a `Configure…` label.
///
/// `import` is a parameter so the order can be tested without a filesystem.
fn startup_location(
    persisted: Option<PersistedLocation>,
    config: &WeatherConfig,
    import: impl FnOnce() -> Option<LocationView>,
) -> Startup {
    if let Some(saved) = persisted
        && valid_coordinates(saved.latitude, saved.longitude)
    {
        return Startup {
            location: Some(LocationView::new(
                saved.label,
                saved.latitude,
                saved.longitude,
            )),
            persist: false,
        };
    }

    if let (Some(latitude), Some(longitude)) = (config.latitude, config.longitude)
        && valid_coordinates(latitude, longitude)
    {
        return Startup {
            location: Some(LocationView::new(String::new(), latitude, longitude)),
            persist: false,
        };
    }

    match import() {
        Some(location) => Startup {
            location: Some(location),
            persist: true,
        },
        None => Startup {
            location: None,
            persist: false,
        },
    }
}
