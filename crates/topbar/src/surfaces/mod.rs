//! Free-floating layer-shell surfaces the bar puts on screen.
//!
//! M1 shipped the tooltip, M3 the popover host, M4 the notification banners,
//! and M8 the volume/brightness capsule.

#[cfg(debug_assertions)]
pub mod dump;
pub mod inline;
pub mod launcher;
pub mod layer_popover;
pub mod modal;
pub mod osd;
pub mod osd_bar;
pub mod popovers;
pub mod search;
pub mod toast;
pub mod tooltip;
