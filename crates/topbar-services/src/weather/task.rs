//! The one owner of the weather cache.
//!
//! Everything that can change the snapshot happens in this loop: the refresh
//! timer, the connectivity watch, and the three commands the panel can send.
//! Requests themselves are spawned rather than awaited inline, so a fifteen
//! second forecast fetch cannot make the setup dialog's search box wait on it.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::connectivity::ConnectivityState;
use crate::error::SvcError;
use crate::refresh::Refresh;
use crate::state_store::StateStore;
use crate::weather::api::{self, Endpoints};
use crate::weather::model::{
    GeocodeResult, LocationView, Phase, TemperatureUnit, WeatherData, WeatherState,
    phase_after_failure,
};
use crate::weather::{PersistedLocation, Settings};

/// What the panel can ask of the weather service.
pub(crate) enum Command {
    /// Fetch now, whatever the schedule said.
    Refresh,
    /// Look a place name up.
    Search(
        String,
        oneshot::Sender<Result<Vec<GeocodeResult>, SvcError>>,
    ),
    /// Read the weather here from now on.
    SetLocation(LocationView, oneshot::Sender<Result<(), SvcError>>),
    /// Save a popup temperature choice.
    SetUnit(TemperatureUnit, oneshot::Sender<Result<(), SvcError>>),
    /// The configuration changed under us.
    Configure(Settings),
}

/// Run the service until every handle is dropped.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    mut commands: mpsc::Receiver<Command>,
    publisher: watch::Sender<Arc<WeatherState>>,
    settings: Settings,
    unit_override: Option<TemperatureUnit>,
    endpoints: Endpoints,
    store: Option<StateStore>,
    mut connectivity: watch::Receiver<Arc<ConnectivityState>>,
    location: Option<LocationView>,
) {
    let (answers, mut outcomes) = mpsc::channel(1);
    let online = connectivity.borrow_and_update().online;

    let mut task = Task {
        refresh: Refresh::new(settings.interval),
        settings: Settings {
            unit: unit_override.unwrap_or(settings.unit),
            ..settings
        },
        unit_override,
        endpoints,
        publisher,
        store,
        answers,
        location,
        last_good: None,
        due: None,
        online,
        in_flight: false,
        generation: 0,
        deferred: false,
    };

    task.publish_phase();
    // The first fetch is immediate: a panel that starts with no weather on it
    // for half an hour is a panel with no weather widget.
    task.due = Some(Instant::now());

    loop {
        // The deadline is copied out before the branches are built, so the
        // timer future borrows nothing the other branches want mutably.
        let due = task.due;
        let timer = async move {
            match due {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(timer);

        tokio::select! {
            command = commands.recv() => match command {
                Some(command) => task.command(command),
                None => break,
            },
            changed = connectivity.changed() => {
                // A watcher that has stopped is not evidence of being offline.
                let online = changed.map_or(true, |()| connectivity.borrow().online);
                task.set_online(online);
            },
            outcome = outcomes.recv() => {
                if let Some(outcome) = outcome {
                    task.settle(outcome.0, outcome.1);
                }
            },
            () = &mut timer => task.fetch(),
        }
    }

    debug!("the weather service has no handles left; stopping");
}

/// Everything the loop owns.
struct Task {
    settings: Settings,
    unit_override: Option<TemperatureUnit>,
    endpoints: Endpoints,
    publisher: watch::Sender<Arc<WeatherState>>,
    store: Option<StateStore>,
    /// Where a spawned fetch sends what it found.
    answers: mpsc::Sender<(u64, Result<WeatherData, SvcError>)>,
    location: Option<LocationView>,
    /// The last reading that arrived, and when. What stale-while-revalidate
    /// keeps showing.
    last_good: Option<(WeatherData, SystemTime)>,
    /// When to fetch next, after a success or a failure.
    refresh: Refresh,
    /// When the next fetch is due. `None` while nothing is scheduled.
    due: Option<Instant>,
    online: bool,
    /// The current request is out. Changes can supersede it without waiting.
    in_flight: bool,
    /// Invalidates replies from requests for an old location or unit.
    generation: u64,
    /// A fetch came due while the machine was offline and is owed.
    deferred: bool,
}

impl Task {
    /// Publish the phase the current state implies.
    fn publish_phase(&self) {
        let phase = match (&self.location, &self.last_good) {
            (None, _) => Phase::NeedsLocation,
            (Some(_), None) => Phase::Loading,
            (Some(_), Some((data, _))) => Phase::Ready(data.clone()),
        };
        self.publish(phase);
    }

    /// Publish `phase`, unless it is already what subscribers are looking at.
    fn publish(&self, phase: Phase) {
        let state = WeatherState {
            phase,
            location: self.location.clone(),
            unit: self.settings.unit,
        };
        if **self.publisher.borrow() == state {
            return;
        }
        let _ = self.publisher.send(Arc::new(state));
    }

    /// Start a forecast request, or explain to itself why not.
    fn fetch(&mut self) {
        self.due = None;

        let Some(location) = self.location.clone() else {
            self.deferred = false;
            return;
        };
        if !self.online {
            // Not a failure: no backoff, no stale timestamp, nothing on screen
            // changes. The request is simply owed until the network is back.
            debug!("the machine is offline; the weather refresh is deferred");
            self.deferred = true;
            return;
        }
        if self.in_flight {
            return;
        }

        self.deferred = false;
        self.in_flight = true;
        if self.last_good.is_none() {
            self.publish(Phase::Loading);
        }

        let url = api::forecast_url(
            &self.endpoints.forecast,
            location.latitude,
            location.longitude,
            self.settings.unit,
            self.settings.days,
        );
        let unit = self.settings.unit;
        let generation = self.generation;
        let answers = self.answers.clone();
        tokio::spawn(async move {
            let outcome = match api::fetch(url).await {
                Ok(body) => api::parse_forecast(&body, unit),
                Err(error) => Err(error),
            };
            let _ = answers.send((generation, outcome)).await;
        });
    }

    /// A request came back.
    fn settle(&mut self, generation: u64, outcome: Result<WeatherData, SvcError>) {
        if generation != self.generation {
            return;
        }
        self.in_flight = false;
        match outcome {
            Ok(data) => {
                debug!("weather updated: {} day(s)", data.days.len());
                self.last_good = Some((data.clone(), SystemTime::now()));
                self.publish(Phase::Ready(data));
                let wait = self.refresh.succeeded();
                self.arm(wait);
            }
            Err(error) => {
                warn!("the weather could not be refreshed: {error}");
                self.publish(phase_after_failure(self.last_good.as_ref()));
                let wait = self.refresh.failed();
                info!("retrying the weather in {}s", wait.as_secs());
                self.arm(wait);
            }
        }
    }

    /// Schedule the next fetch.
    fn arm(&mut self, wait: Duration) {
        self.due = Some(Instant::now() + wait);
    }

    /// The network came or went.
    fn set_online(&mut self, online: bool) {
        if self.online == online {
            return;
        }
        self.online = online;
        // Coming back online fetches at once rather than waiting out whatever
        // was left of the interval: the reading on screen is from before the
        // connection dropped, and the user can see that it is.
        if online && self.deferred {
            info!("the machine is back online; refreshing the weather");
            self.fetch();
        }
    }

    /// Apply one command.
    fn command(&mut self, command: Command) {
        match command {
            Command::Refresh => self.fetch(),
            Command::Search(query, reply) => self.search(query, reply),
            Command::SetLocation(location, reply) => {
                self.set_location(location);
                let _ = reply.send(Ok(()));
            }
            Command::SetUnit(unit, reply) => {
                self.set_unit(unit);
                let _ = reply.send(Ok(()));
            }
            Command::Configure(settings) => self.configure(settings),
        }
    }

    /// Look a place up, off the loop.
    fn search(&self, query: String, reply: oneshot::Sender<Result<Vec<GeocodeResult>, SvcError>>) {
        if !self.online {
            // Better than fifteen seconds of spinner followed by a timeout.
            let _ = reply.send(Err(SvcError::Http("the machine is offline".to_string())));
            return;
        }

        let url = api::geocoding_url(&self.endpoints.geocoding, &query);
        tokio::spawn(async move {
            let outcome = match api::fetch(url).await {
                Ok(body) => api::parse_geocoding(&body),
                Err(error) => Err(error),
            };
            let _ = reply.send(outcome);
        });
    }

    /// Point the service somewhere else.
    fn set_location(&mut self, location: LocationView) {
        if self.location.as_ref() == Some(&location) {
            // Still refresh: the user pressing Save expects something to happen.
            self.fetch();
            return;
        }

        info!("reading the weather for `{}`", location.label);
        if let Some(store) = &self.store {
            let saved = PersistedLocation {
                label: location.label.clone(),
                latitude: location.latitude,
                longitude: location.longitude,
            };
            store.update(move |state| state.weather.location = Some(saved));
        }

        self.location = Some(location);
        // The reading on screen is for the old place.
        self.invalidate();
    }

    /// A popup choice wins on subsequent configuration reloads.
    fn set_unit(&mut self, unit: TemperatureUnit) {
        if self.unit_override != Some(unit) {
            self.unit_override = Some(unit);
            if let Some(store) = &self.store {
                store.update(move |state| state.weather.unit = Some(unit));
            }
        }
        if self.settings.unit == unit {
            return;
        }
        self.settings.unit = unit;
        self.invalidate();
    }

    /// Retire in-flight work before any superseded reading can be published.
    fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.in_flight = false;
        self.last_good = None;
        self.refresh = Refresh::new(self.settings.interval);
        self.publish_phase();
        self.fetch();
    }

    /// The configuration changed. M12's hot reload is what calls this.
    fn configure(&mut self, mut settings: Settings) {
        settings.unit = self.unit_override.unwrap_or(settings.unit);
        if self.settings == settings {
            return;
        }
        debug!("weather settings changed; the cache is no longer valid");
        self.settings = settings;
        self.invalidate();
    }
}
