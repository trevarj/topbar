//! Command-line surface.
//!
//! The flags and subcommands mirror v1 exactly so existing niri keybinds keep
//! working. Every subcommand talks to the running panel over the framed IPC
//! socket described in [`topbar_core::ipc`].

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// topbar - GNOME Shell-inspired GTK4 top bar for niri
#[derive(Debug, Parser)]
#[command(name = "topbar", version, about, long_about = None)]
pub struct Cli {
    /// Path to the configuration file (uses the XDG lookup chain if omitted)
    #[arg(short, long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Increase verbosity (-v info, -vv debug, -vvv trace)
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Print the example configuration and exit
    #[arg(long)]
    pub print_example_config: bool,

    /// Validate the configuration and exit
    #[arg(long)]
    pub check_config: bool,

    /// Treat configuration warnings as errors
    #[arg(long)]
    pub strict: bool,

    /// Subcommand to run instead of starting the panel
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Subcommands that control an already-running panel.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Control screen brightness
    Brightness {
        /// Brightness operation
        #[command(subcommand)]
        action: BrightnessAction,
    },
    /// Control audio volume
    Volume {
        /// Volume operation
        #[command(subcommand)]
        action: VolumeAction,
    },
    /// Control the idle/sleep inhibitor
    Inhibit {
        /// Inhibitor operation
        #[command(subcommand)]
        action: InhibitAction,
    },
    /// Control media playback (MPRIS)
    Media {
        /// Playback operation
        #[command(subcommand)]
        action: MediaAction,
    },
    /// Control bar visibility
    Bar {
        /// Visibility operation
        #[command(subcommand)]
        action: VisibilityAction,
    },
    /// Control widget popovers
    Popover {
        /// Popover operation
        #[command(subcommand)]
        action: PopoverAction,
    },
    /// Control the application, window, and file launcher
    Launcher {
        /// Launcher visibility operation
        #[command(subcommand)]
        action: VisibilityAction,
    },
    /// Choose one structured item from standard input
    Choose {
        /// Picker layout
        #[arg(long, value_enum, default_value_t = ChooseLayout::List)]
        layout: ChooseLayout,
        /// Dialog heading
        #[arg(long)]
        title: String,
        /// Optional explanation shown under the heading
        #[arg(long)]
        message: Option<String>,
        /// ID of the currently applied item
        #[arg(long)]
        selected: Option<String>,
        /// Executable providing on-demand Wallhaven search and save (wallpapers only)
        #[arg(long, requires = "layout")]
        wallpaper_provider: Option<std::path::PathBuf>,
    },
    /// Serve the GnuPG pinentry protocol on standard input and output.
    Pinentry,
    /// Re-read the configuration and apply what changed
    Reload,
    /// Dump panel state or built-in defaults
    Dump {
        /// What to dump (everything, when omitted)
        #[command(subcommand)]
        action: Option<DumpAction>,

        /// Print JSON rather than the human-readable form
        ///
        /// Global so it may be written either side of the target: `dump --json
        /// state` and `dump state --json` are the same command, and a user
        /// should not have to remember which.
        #[arg(long, global = true)]
        json: bool,
    },
}

/// Structured chooser presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ChooseLayout {
    /// Compact textual rows.
    List,
    /// Theme palette cards.
    Themes,
    /// Wallpaper image cards.
    Wallpapers,
}

/// Brightness operations.
#[derive(Debug, Subcommand)]
pub enum BrightnessAction {
    /// Print the current brightness percentage
    Get,
    /// Set brightness to a percentage
    Set {
        /// Brightness percentage (0-100)
        #[arg(value_parser = clap::value_parser!(u32).range(0..=100))]
        percent: u32,
    },
    /// Increase brightness
    Inc {
        /// Percentage points to add
        #[arg(default_value = "5")]
        amount: u32,
    },
    /// Decrease brightness
    Dec {
        /// Percentage points to subtract
        #[arg(default_value = "5")]
        amount: u32,
    },
}

/// Volume operations.
#[derive(Debug, Subcommand)]
pub enum VolumeAction {
    /// Print the current volume percentage
    Get,
    /// Set volume to a percentage
    Set {
        /// Volume percentage, capped at 100 unless audio.allow_overdrive is set
        percent: u32,
    },
    /// Increase volume
    Inc {
        /// Percentage points to add
        #[arg(default_value = "5")]
        amount: u32,
    },
    /// Decrease volume
    Dec {
        /// Percentage points to subtract
        #[arg(default_value = "5")]
        amount: u32,
    },
    /// Mute the default sink
    Mute,
    /// Unmute the default sink
    Unmute,
    /// Toggle the mute state
    ToggleMute,
}

/// Idle inhibitor operations.
#[derive(Debug, Subcommand)]
pub enum InhibitAction {
    /// Toggle the inhibitor
    Toggle,
}

/// Media playback operations.
#[derive(Debug, Subcommand)]
pub enum MediaAction {
    /// Toggle play/pause
    PlayPause,
    /// Skip to the next track
    Next,
    /// Go to the previous track
    Previous,
    /// Stop playback
    Stop,
    /// Print the current playback status
    Status,
}

/// Show/hide/toggle operations.
#[derive(Debug, Subcommand)]
pub enum VisibilityAction {
    /// Show
    Show,
    /// Hide
    Hide,
    /// Toggle
    Toggle,
}

/// Popover operations.
#[derive(Debug, Subcommand)]
pub enum PopoverAction {
    /// Open a widget's popover
    Show {
        /// Widget name (e.g. clock, quick_settings)
        widget: String,
    },
    /// Close a popover, or the active one when no widget is given
    Hide {
        /// Widget name
        widget: Option<String>,
    },
    /// Toggle a widget's popover
    Toggle {
        /// Widget name
        widget: String,
    },
}

/// Dump targets.
#[derive(Debug, Subcommand)]
pub enum DumpAction {
    /// Print the built-in example configuration
    DefaultConfig,
    /// Print the configuration the running panel is using
    Config,
    /// Print a snapshot of live service state
    State,
}
