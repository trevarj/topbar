//! The weather service against a real bus and a real socket — both private.
//!
//! The NetworkManager here is a stand-in served on a `dbus-daemon` that lives
//! for one test, and the Open-Meteo endpoints are a listener on loopback that
//! answers with the recorded fixtures. Nothing in this file touches the
//! machine's real system bus or the real internet, which is what makes it safe
//! to run `cargo test` on a laptop.
//!
//! What is only covered here is the wiring: that a `StateChanged` signal
//! becomes an online/offline transition, and that a transition to online
//! actually fires the fetch that was owed.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;

use super::*;
use crate::connectivity::bus_tests::{serve_nm, set_nm_state};
use crate::connectivity::{Connectivity, ConnectivityState};
use crate::private_bus::private_bus;
use crate::state_store::StateStore;
use crate::weather::api::Endpoints;

const FORECAST: &str = include_str!("../../tests/fixtures/open-meteo-forecast-celsius.json");
const FAHRENHEIT: &str = include_str!("../../tests/fixtures/open-meteo-forecast-fahrenheit.json");
const GEOCODING: &str = include_str!("../../tests/fixtures/open-meteo-geocoding.json");
const RATE_LIMIT: &str = include_str!("../../tests/fixtures/open-meteo-rate-limit.json");

/// How long a test waits for the service to catch up before failing.
const PATIENCE: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// An Open-Meteo that answers from the fixtures
// ---------------------------------------------------------------------------

/// What the stub answers with. Swappable, so one service can watch its
/// endpoint go down under it.
type Answer = Arc<std::sync::Mutex<(u16, String)>>;

/// A listener standing in for Open-Meteo.
struct StubApi {
    endpoints: Endpoints,
    requests: Arc<AtomicUsize>,
    answer: Answer,
}

impl StubApi {
    /// Serve `body` with `status` to everything that connects.
    async fn start(status: u16, body: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback is available");
        let address = listener.local_addr().expect("a bound port");
        let requests = Arc::new(AtomicUsize::new(0));
        let answer: Answer = Arc::new(std::sync::Mutex::new((status, body.to_string())));

        let counter = Arc::clone(&requests);
        let served = Arc::clone(&answer);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                // Read the request head; the body is never used.
                let mut scratch = [0_u8; 2048];
                let _ = socket.read(&mut scratch).await;
                let (status, body) = served.lock().expect("the answer is not poisoned").clone();
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                     Content-Length: {length}\r\nConnection: close\r\n\r\n{body}",
                    length = body.len(),
                );
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });

        Self {
            endpoints: Endpoints {
                forecast: format!("http://{address}/forecast"),
                geocoding: format!("http://{address}/search"),
            },
            requests,
            answer,
        }
    }

    /// Answer differently from now on.
    fn answer_with(&self, status: u16, body: &str) {
        *self.answer.lock().expect("the answer is not poisoned") = (status, body.to_string());
    }

    /// How many requests it has answered.
    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Wait until a published snapshot satisfies `predicate`.
async fn wait_for(
    state: &mut watch::Receiver<Arc<WeatherState>>,
    what: &str,
    predicate: impl Fn(&WeatherState) -> bool,
) -> Arc<WeatherState> {
    let wait = async {
        loop {
            // Cloned out before testing: a read guard held across an await
            // deadlocks against the task trying to publish the next value.
            let snapshot = state.borrow_and_update().clone();
            if predicate(&snapshot) {
                return snapshot;
            }
            state.changed().await.expect("the weather service is alive");
        }
    };
    tokio::time::timeout(PATIENCE, wait)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// Wait until `predicate` holds, polling.
async fn wait_until(what: &str, predicate: impl Fn() -> bool) {
    let wait = async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(PATIENCE, wait)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

fn settings() -> Settings {
    Settings::from_config(&WeatherConfig::default())
}

fn moscow() -> LocationView {
    LocationView::new("Moscow", 55.75204, 37.61781)
}

/// A connectivity channel a test drives by hand.
fn manual_connectivity(
    online: bool,
) -> (
    watch::Sender<Arc<ConnectivityState>>,
    watch::Receiver<Arc<ConnectivityState>>,
) {
    watch::channel(Arc::new(ConnectivityState { online }))
}

// ---------------------------------------------------------------------------
// The weather service driven by connectivity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn coming_back_online_fires_the_fetch_that_was_owed() {
    let api = StubApi::start(200, FORECAST).await;
    let (online, connectivity) = manual_connectivity(false);

    let weather = Weather::start_with(
        settings(),
        api.endpoints.clone(),
        None,
        connectivity,
        Some(moscow()),
    );
    let mut state = weather.state();

    // Offline: the first fetch is due immediately and is deferred instead. The
    // panel shows Loading rather than a failure, because nothing failed.
    wait_for(&mut state, "the first frame", |state| {
        state.phase == Phase::Loading
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(api.requests(), 0, "an offline panel must not dial out");

    online
        .send(Arc::new(ConnectivityState { online: true }))
        .ok();

    let ready = wait_for(&mut state, "a forecast", |state| {
        matches!(state.phase, Phase::Ready(_))
    })
    .await;
    let data = ready.data().expect("a reading");
    assert_eq!(data.days.len(), 5);
    assert_eq!(data.current.code, 2);
    assert_eq!(api.requests(), 1, "exactly one request, not one per signal");
}

#[tokio::test]
async fn a_networkmanager_transition_is_what_drives_that() {
    let bus = private_bus!();
    let nm = serve_nm(&bus, 20).await;
    let api = StubApi::start(200, FORECAST).await;

    let connectivity = Connectivity::start(Some(bus.address().to_string()));
    // The service must see "offline" before it is started, or its first fetch
    // races the initial NetworkManager read.
    wait_until("the panel to notice it is offline", || {
        !connectivity.is_online()
    })
    .await;

    let weather = Weather::start_with(
        settings(),
        api.endpoints.clone(),
        None,
        connectivity.state(),
        Some(moscow()),
    );
    let mut state = weather.state();

    wait_for(&mut state, "the first frame", |state| {
        state.phase == Phase::Loading
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(api.requests(), 0);

    set_nm_state(&nm, 70).await;

    wait_for(&mut state, "a forecast", |state| {
        matches!(state.phase, Phase::Ready(_))
    })
    .await;
    assert_eq!(api.requests(), 1);
}

#[tokio::test]
async fn an_endpoint_that_goes_down_leaves_the_last_reading_on_screen() {
    let api = StubApi::start(200, FORECAST).await;
    let (_online, connectivity) = manual_connectivity(true);

    let weather = Weather::start_with(
        settings(),
        api.endpoints.clone(),
        None,
        connectivity,
        Some(moscow()),
    );
    let mut state = weather.state();
    let ready = wait_for(&mut state, "a forecast", |state| {
        matches!(state.phase, Phase::Ready(_))
    })
    .await;
    let good = ready.data().expect("a reading").clone();

    api.answer_with(500, "upstream is on fire");
    weather
        .handle()
        .refresh_now()
        .await
        .expect("the service is alive");

    let stale = wait_for(&mut state, "the reading to go stale", |state| {
        state.stale_since().is_some()
    })
    .await;
    assert_eq!(
        stale.data(),
        Some(&good),
        "a failed refresh keeps what was already on screen"
    );
    assert!(
        stale.stale_since().expect("a timestamp") <= std::time::SystemTime::now(),
        "the timestamp is when the reading was taken"
    );
}

#[tokio::test]
async fn a_first_fetch_that_fails_has_nothing_to_fall_back_on() {
    let api = StubApi::start(429, RATE_LIMIT).await;
    let (_online, connectivity) = manual_connectivity(true);

    let weather = Weather::start_with(
        settings(),
        api.endpoints.clone(),
        None,
        connectivity,
        Some(moscow()),
    );
    let mut state = weather.state();

    let failed = wait_for(&mut state, "an unavailable card", |state| {
        state.is_unavailable()
    })
    .await;
    assert!(failed.data().is_none(), "nothing has ever landed here");
    assert_eq!(
        failed
            .location
            .as_ref()
            .map(|location| location.label.as_str()),
        Some("Moscow"),
        "the location is known even when the forecast is not"
    );
}

#[tokio::test]
async fn a_search_comes_back_with_the_places_the_geocoder_found() {
    let api = StubApi::start(200, GEOCODING).await;
    let (_online, connectivity) = manual_connectivity(true);

    let weather = Weather::start_with(settings(), api.endpoints.clone(), None, connectivity, None);

    let results = weather
        .handle()
        .search("moscow".to_string())
        .await
        .expect("the stub answers");
    assert_eq!(results.len(), 5);
    assert_eq!(results[0].label, "Moscow — Moscow, Russia");
}

#[tokio::test]
async fn a_search_made_offline_fails_at_once_rather_than_timing_out() {
    let api = StubApi::start(200, GEOCODING).await;
    let (_online, connectivity) = manual_connectivity(false);

    let weather = Weather::start_with(settings(), api.endpoints.clone(), None, connectivity, None);

    let error = weather
        .handle()
        .search("moscow".to_string())
        .await
        .expect_err("there is no network to search over");
    assert!(matches!(error, SvcError::Http(_)));
    assert_eq!(api.requests(), 0);
}

#[tokio::test]
async fn saving_a_location_writes_it_down_and_fetches_for_it() {
    let api = StubApi::start(200, FORECAST).await;
    let (_online, connectivity) = manual_connectivity(true);

    let path = std::env::temp_dir()
        .join(format!("topbar-weather-{}", std::process::id()))
        .join("state.json");
    let _ = std::fs::remove_dir_all(path.parent().expect("a parent"));
    let (persisted, store) = StateStore::open_at(path.clone());
    assert_eq!(persisted.weather.location, None);

    let weather = Weather::start_with(
        settings(),
        api.endpoints.clone(),
        Some(store),
        connectivity,
        None,
    );
    let mut state = weather.state();
    wait_for(&mut state, "a panel with nowhere to look", |state| {
        state.phase == Phase::NeedsLocation
    })
    .await;

    weather
        .handle()
        .set_location(GeocodeResult {
            label: "Moscow — Moscow, Russia".to_string(),
            latitude: 55.75222,
            longitude: 37.61556,
        })
        .await
        .expect("the location is on Earth");

    let ready = wait_for(&mut state, "a forecast for the new place", |state| {
        matches!(state.phase, Phase::Ready(_))
    })
    .await;
    assert_eq!(
        ready
            .location
            .as_ref()
            .map(|location| location.label.as_str()),
        Some("Moscow — Moscow, Russia")
    );
    api.answer_with(200, FAHRENHEIT);
    weather
        .handle()
        .set_unit(TemperatureUnit::Fahrenheit)
        .await
        .expect("the popup choice is accepted");
    let fahrenheit = wait_for(&mut state, "a Fahrenheit reading", |state| {
        state.unit == TemperatureUnit::Fahrenheit
            && state.data().is_some_and(|data| data.unit == state.unit)
    })
    .await;
    assert_eq!(
        fahrenheit.data().expect("a reading").current.temperature,
        70.5
    );

    // And a panel started tomorrow skips the setup dialog entirely.
    wait_until("the choices to reach the state file", || {
        std::fs::read_to_string(&path).is_ok_and(|contents| {
            contents.contains("Moscow — Moscow, Russia")
                && contents.contains("\"unit\": \"fahrenheit\"")
        })
    })
    .await;

    let (reloaded, store) = StateStore::open_at(path.clone());
    let saved = reloaded.weather.location.expect("a saved location");
    assert_eq!(saved.label, "Moscow — Moscow, Russia");
    assert!((saved.latitude - 55.75222).abs() < 1e-9);
    assert_eq!(reloaded.weather.unit, Some(TemperatureUnit::Fahrenheit));

    let startup = startup_location(Some(saved), &WeatherConfig::default(), || {
        panic!("nothing to import")
    });
    assert!(
        startup.location.is_some(),
        "the second start has a location"
    );
    assert!(!startup.persist);

    // The saved popup choice takes precedence over a Celsius config at startup
    // and on a later reload; there is no Celsius data under a Fahrenheit label.
    let config = WeatherConfig::default();
    let (_, connectivity) = manual_connectivity(true);
    let restarted = Weather::spawn(
        Settings::from_config(&config),
        reloaded.weather.unit,
        api.endpoints.clone(),
        Some(store),
        connectivity,
        startup.location,
        true,
    );
    assert_eq!(restarted.state().borrow().unit, TemperatureUnit::Fahrenheit);
    let mut state = restarted.state();
    wait_for(&mut state, "the restarted Fahrenheit reading", |state| {
        state
            .data()
            .is_some_and(|data| data.unit == TemperatureUnit::Fahrenheit)
    })
    .await;
    let requests = api.requests();
    let mut changed = Settings::from_config(&config);
    changed.days = 4;
    restarted
        .handle()
        .configure(changed)
        .await
        .expect("config reload is accepted");
    wait_until("a reload request in the saved unit", || {
        api.requests() > requests
    })
    .await;
    let reading = wait_for(&mut state, "a post-reload Fahrenheit reading", |state| {
        state.unit == TemperatureUnit::Fahrenheit
            && state.data().is_some_and(|data| data.unit == state.unit)
    })
    .await;
    assert_eq!(reading.unit, TemperatureUnit::Fahrenheit);
}

#[tokio::test]
async fn choosing_a_unit_before_a_location_keeps_setup_available() {
    let api = StubApi::start(200, FAHRENHEIT).await;
    let (_online, connectivity) = manual_connectivity(true);
    let weather = Weather::start_with(settings(), api.endpoints.clone(), None, connectivity, None);
    let mut state = weather.state();
    weather
        .handle()
        .set_unit(TemperatureUnit::Fahrenheit)
        .await
        .expect("the popup can save a unit without coordinates");
    let chosen = wait_for(
        &mut state,
        "a Fahrenheit choice without a location",
        |state| state.unit == TemperatureUnit::Fahrenheit,
    )
    .await;
    assert_eq!(chosen.phase, Phase::NeedsLocation);
    assert_eq!(api.requests(), 0, "no place means no forecast request");
}

#[tokio::test]
async fn a_superseded_celsius_request_cannot_replace_fahrenheit_weather() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback is available");
    let address = listener.local_addr().expect("a bound port");
    let (old_requested, requested) = tokio::sync::oneshot::channel();
    let (release_old, released) = tokio::sync::oneshot::channel();
    let (old_sent, sent) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut old, _) = listener.accept().await.expect("first request");
        let mut scratch = [0_u8; 2048];
        let length = old.read(&mut scratch).await.expect("old request");
        assert!(String::from_utf8_lossy(&scratch[..length]).contains("temperature_unit=celsius"));
        old_requested.send(()).ok();

        let (mut new, _) = listener.accept().await.expect("replacement request");
        let length = new.read(&mut scratch).await.expect("new request");
        assert!(
            String::from_utf8_lossy(&scratch[..length]).contains("temperature_unit=fahrenheit")
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{FAHRENHEIT}",
            FAHRENHEIT.len()
        );
        new.write_all(response.as_bytes())
            .await
            .expect("new response");
        let _ = released.await;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{FORECAST}",
            FORECAST.len()
        );
        old.write_all(response.as_bytes())
            .await
            .expect("old response");
        old_sent.send(()).ok();
    });

    let (_online, connectivity) = manual_connectivity(true);
    let weather = Weather::start_with(
        settings(),
        Endpoints {
            forecast: format!("http://{address}/forecast"),
            geocoding: format!("http://{address}/search"),
        },
        None,
        connectivity,
        Some(moscow()),
    );
    let mut state = weather.state();
    tokio::time::timeout(PATIENCE, requested)
        .await
        .expect("the initial request started")
        .expect("the server answered");
    weather
        .handle()
        .set_unit(TemperatureUnit::Fahrenheit)
        .await
        .expect("the new unit is accepted");
    let loading = wait_for(&mut state, "the new unit before its reading", |state| {
        state.unit == TemperatureUnit::Fahrenheit && state.phase == Phase::Loading
    })
    .await;
    assert!(loading.data().is_none());
    let ready = wait_for(&mut state, "Fahrenheit weather", |state| {
        state
            .data()
            .is_some_and(|data| data.unit == TemperatureUnit::Fahrenheit)
    })
    .await;
    let data = ready.data().expect("the Fahrenheit response");
    assert_eq!(data.current.temperature, 70.5);
    assert_eq!(data.current.feels_like, 73.2);
    assert_eq!(data.days[0].high, 82.4);
    assert_eq!(data.days[0].low, 64.9);

    release_old.send(()).expect("the old request still exists");
    tokio::time::timeout(PATIENCE, sent)
        .await
        .expect("the stale response arrived")
        .expect("the server answered");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(state.borrow().data(), Some(data));
}
