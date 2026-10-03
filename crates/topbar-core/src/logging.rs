//! Logging setup using `tracing`.
//!
//! Verbosity comes from repeated `-v` flags; `RUST_LOG` is layered on top so a
//! user-provided filter always wins over the flag-derived default level.

use tracing::Level;
use tracing_subscriber::{EnvFilter, fmt};

/// Map a `-v` repeat count to a tracing level.
///
/// `0` = warn, `1` = info, `2` = debug, `3+` = trace.
pub fn level_for_verbosity(verbosity: u8) -> Level {
    match verbosity {
        0 => Level::WARN,
        1 => Level::INFO,
        2 => Level::DEBUG,
        _ => Level::TRACE,
    }
}

/// Initialize the global tracing subscriber.
///
/// The flag-derived level is installed as the base directive and `RUST_LOG`
/// (when set) is overlaid on top of it, so `RUST_LOG=topbar_services=trace`
/// works without also passing `-vvv`.
///
/// # Example
/// ```
/// topbar_core::logging::init(1); // info level
/// ```
pub fn init(verbosity: u8) {
    let level = level_for_verbosity(verbosity);
    let filter = EnvFilter::from_default_env().add_directive(level.into());

    let _ = fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbosity_maps_to_levels() {
        assert_eq!(level_for_verbosity(0), Level::WARN);
        assert_eq!(level_for_verbosity(1), Level::INFO);
        assert_eq!(level_for_verbosity(2), Level::DEBUG);
        assert_eq!(level_for_verbosity(3), Level::TRACE);
        assert_eq!(level_for_verbosity(9), Level::TRACE);
    }
}
