//! The indicator strip: one custom widget that draws every workspace.
//!
//! This is the fix for v1's per-frame relayout. There, each indicator was its
//! own widget and the active-pill animation drove `set_size_request` sixty
//! times a second, so every frame of a workspace switch re-ran the whole bar's
//! layout — the cause of the last four workspace bugfixes before the rewrite.
//!
//! Here the widget's [`measure`](gtk4::subclass::prelude::WidgetImpl::measure)
//! result depends only on the *set* of visible workspaces, never on which one
//! is active (see [`model::total_width`]). Everything that moves is drawn in
//! [`snapshot`](gtk4::subclass::prelude::WidgetImpl::snapshot) from
//! interpolated rectangles, so a frame of animation costs one `queue_draw` and
//! no layout at all.

use std::cell::{Cell, OnceCell, RefCell};

use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{gdk, glib, graphene, gsk, pango};
use topbar_services::WorkspaceId;

use crate::anim::{Animation, AnimationParams, Easing};
use crate::style::classes;
use crate::widgets::workspaces::model::{
    self, ACTIVE_DELTA, DOT_SIZE, Slot, SlotRect, inactive_width,
};

/// Alpha applied to the foreground color for an inactive indicator.
const INACTIVE_ALPHA: f32 = 0.35;
/// Duration of the active-pill transfer, in milliseconds.
const TRANSFER_MS: u64 = 200;
/// Duration of a slot fading in after the workspace set changed.
const APPEAR_MS: u64 = 150;
/// Length of one urgency pulse, in milliseconds.
const PULSE_MS: u64 = 500;
/// How many times an urgent indicator pulses before going steady.
const PULSE_CYCLES: f64 = 2.0;
/// Peak size increase of a pulsing indicator.
const PULSE_AMPLITUDE: f32 = 0.5;
/// Vertical padding above and below a labelled indicator.
const LABEL_PAD_Y: f32 = 4.0;

/// Colors the strip cannot get from CSS because it paints them itself.
#[derive(Debug, Clone, Copy)]
pub struct StripColors {
    /// Fill for an urgent indicator.
    pub urgent: gdk::RGBA,
    /// Text color on top of the active pill.
    pub on_active: gdk::RGBA,
    /// Contrast-preserving text on urgent pills.
    pub on_urgent: gdk::RGBA,
}

impl Default for StripColors {
    fn default() -> Self {
        Self {
            urgent: gdk::RGBA::RED,
            on_active: gdk::RGBA::BLACK,
            on_urgent: gdk::RGBA::BLACK,
        }
    }
}

/// One indicator, resolved to what the strip needs to draw it.
#[derive(Debug, Clone)]
struct DrawSlot {
    id: WorkspaceId,
    layout: Option<pango::Layout>,
    /// Natural width while inactive.
    width: f32,
    is_active: bool,
    is_urgent: bool,
    /// Whether this slot arrived with the last update and is still fading in.
    is_new: bool,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct WorkspaceStrip {
        pub(super) slots: RefCell<Vec<DrawSlot>>,
        /// Layout the current transfer started from.
        pub(super) from: RefCell<Vec<SlotRect>>,
        /// Layout the current transfer is heading to.
        pub(super) to: RefCell<Vec<SlotRect>>,
        pub(super) progress: Cell<f64>,
        pub(super) appear: Cell<f64>,
        pub(super) pulse: Cell<f64>,
        pub(super) colors: Cell<StripColors>,
        pub(super) animate: Cell<bool>,
        pub(super) transfer: OnceCell<Animation>,
        pub(super) appear_anim: OnceCell<Animation>,
        pub(super) pulse_anim: OnceCell<Animation>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for WorkspaceStrip {
        const NAME: &'static str = "TopbarWorkspaceStrip";
        type Type = super::WorkspaceStrip;
        type ParentType = gtk4::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name(classes::WORKSPACE_STRIP);
        }
    }

    impl ObjectImpl for WorkspaceStrip {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            self.progress.set(1.0);
            self.appear.set(1.0);
            self.animate.set(true);
            let _ = self.transfer.set(Animation::new(&*obj));
            let _ = self.appear_anim.set(Animation::new(&*obj));
            let _ = self.pulse_anim.set(Animation::new(&*obj));
        }
    }

    impl WidgetImpl for WorkspaceStrip {
        /// Width from the workspace *set* alone; height from the tallest label.
        ///
        /// Nothing here reads the active slot, which is what keeps a workspace
        /// switch from touching layout.
        fn measure(&self, orientation: gtk4::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let slots = self.slots.borrow();
            if orientation == gtk4::Orientation::Horizontal {
                let widths: Vec<f32> = slots.iter().map(|slot| slot.width).collect();
                let width = model::total_width(&widths).ceil() as i32;
                return (width, width, -1, -1);
            }

            let height = slots
                .iter()
                .filter_map(|slot| slot.layout.as_ref())
                .map(|layout| layout.pixel_size().1 as f32 + 2.0 * LABEL_PAD_Y)
                .fold(DOT_SIZE, f32::max)
                .ceil() as i32;
            (height, height, -1, -1)
        }

        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            let obj = self.obj();
            let slots = self.slots.borrow();
            if slots.is_empty() {
                return;
            }

            let rects = self.current_rects();
            let colors = self.colors.get();
            let foreground = obj.color();
            let inactive = with_alpha(foreground, foreground.alpha() * INACTIVE_ALPHA);
            let allocated = obj.height() as f32;
            let labelled = slots.iter().any(|slot| slot.layout.is_some());
            let base_height = if labelled { allocated } else { DOT_SIZE };
            let appear = self.appear.get() as f32;
            let pulse = self.pulse.get() as f32;

            for (slot, rect) in slots.iter().zip(rects) {
                // A slot that has just appeared grows and fades into place; an
                // urgent one breathes for a couple of cycles and then holds.
                let growth = if slot.is_new { appear } else { 1.0 };
                let swell = if slot.is_urgent {
                    1.0 + PULSE_AMPLITUDE * pulse
                } else {
                    1.0
                };
                let height = (base_height * growth * swell).min(allocated);
                let width = (rect.width * growth * swell).max(0.0);
                let x = rect.x + (rect.width - width) / 2.0;
                let y = (allocated - height) / 2.0;

                // How far along this slot is between dot and pill. Deriving it
                // from the width the frame is already drawing keeps colour and
                // geometry on exactly the same clock: a pill that is halfway
                // out is halfway dimmed, so a transfer reads as one motion
                // rather than an instant recolour plus a slide.
                let activeness = activeness(rect.width, slot.width);
                let fill = if slot.is_urgent {
                    colors.urgent
                } else {
                    with_alpha(
                        foreground,
                        foreground.alpha() * lerp(INACTIVE_ALPHA, 1.0, activeness),
                    )
                };
                let fill = with_alpha(fill, fill.alpha() * growth);

                // A labelled slot only paints a background when it is the one
                // you are on; the others are text alone.
                if slot.layout.is_none() || activeness > 0.0 || slot.is_urgent {
                    let bounds = graphene::Rect::new(x, y, width, height);
                    snapshot.push_rounded_clip(&gsk::RoundedRect::from_rect(bounds, height / 2.0));
                    snapshot.append_color(&fill, &bounds);
                    snapshot.pop();
                }

                let Some(layout) = &slot.layout else {
                    continue;
                };
                let (text_width, text_height) = layout.pixel_size();
                let text_color = if slot.is_urgent {
                    colors.on_urgent
                } else {
                    blend(inactive, colors.on_active, activeness)
                };
                snapshot.save();
                snapshot.translate(&graphene::Point::new(
                    x + (width - text_width as f32) / 2.0,
                    y + (height - text_height as f32) / 2.0,
                ));
                snapshot
                    .append_layout(layout, &with_alpha(text_color, text_color.alpha() * growth));
                snapshot.restore();
            }
        }
    }

    impl WorkspaceStrip {
        /// The layout as of this frame.
        pub(super) fn current_rects(&self) -> Vec<SlotRect> {
            model::lerp_rects(
                &self.from.borrow(),
                &self.to.borrow(),
                self.progress.get() as f32,
            )
        }
    }
}

/// How far a slot is from dot (`0.0`) to pill (`1.0`).
///
/// `drawn` is the width this frame is painting and `resting` the slot's width
/// while inactive; the difference is always [`ACTIVE_DELTA`] at the ends.
fn activeness(drawn: f32, resting: f32) -> f32 {
    ((drawn - resting) / ACTIVE_DELTA).clamp(0.0, 1.0)
}

/// Interpolate between two numbers.
fn lerp(from: f32, to: f32, progress: f32) -> f32 {
    from + (to - from) * progress
}

/// Interpolate between two colors, channel by channel.
fn blend(from: gdk::RGBA, to: gdk::RGBA, progress: f32) -> gdk::RGBA {
    gdk::RGBA::new(
        lerp(from.red(), to.red(), progress),
        lerp(from.green(), to.green(), progress),
        lerp(from.blue(), to.blue(), progress),
        lerp(from.alpha(), to.alpha(), progress),
    )
}

/// Replace `color`'s alpha channel.
fn with_alpha(color: gdk::RGBA, alpha: f32) -> gdk::RGBA {
    gdk::RGBA::new(
        color.red(),
        color.green(),
        color.blue(),
        alpha.clamp(0.0, 1.0),
    )
}

glib::wrapper! {
    /// The workspace indicators, drawn as one widget.
    pub struct WorkspaceStrip(ObjectSubclass<imp::WorkspaceStrip>)
        @extends gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl Default for WorkspaceStrip {
    fn default() -> Self {
        glib::Object::builder().build()
    }
}

impl WorkspaceStrip {
    /// Create an empty strip.
    pub fn new(colors: StripColors, animate: bool) -> Self {
        let strip = Self::default();
        strip.imp().colors.set(colors);
        strip.imp().animate.set(animate);
        strip
    }

    /// Show `slots`, animating from whatever is on screen right now.
    pub fn set_slots(&self, slots: &[Slot]) {
        let imp = self.imp();
        let previous_ids: Vec<WorkspaceId> =
            imp.slots.borrow().iter().map(|slot| slot.id).collect();
        let structural = previous_ids.len() != slots.len()
            || previous_ids
                .iter()
                .zip(slots)
                .any(|(id, slot)| *id != slot.id);
        let was_urgent = imp.slots.borrow().iter().any(|slot| slot.is_urgent);

        let draw_slots: Vec<DrawSlot> = slots
            .iter()
            .map(|slot| {
                let layout = slot
                    .label
                    .as_deref()
                    .map(|text| self.create_pango_layout(Some(text)));
                let width = inactive_width(layout.as_ref().map(|l| l.pixel_size().0 as f32));
                DrawSlot {
                    id: slot.id,
                    layout,
                    width,
                    is_active: slot.is_active,
                    is_urgent: slot.is_urgent,
                    is_new: structural && model::is_appearance(&previous_ids, slot.id),
                }
            })
            .collect();

        let widths: Vec<f32> = draw_slots.iter().map(|slot| slot.width).collect();
        let active = draw_slots.iter().position(|slot| slot.is_active);
        let target = model::slot_rects(&widths, active);
        // A workspace opening or closing changes the widget's width, so the
        // pill cannot slide there from an old layout that no longer exists;
        // the new set snaps into place and the new slot fades in instead.
        let start = if structural {
            target.clone()
        } else {
            imp.current_rects()
        };

        // Nothing to slide: the snapshot changed in some way that does not
        // move an indicator (a window opened on a workspace that was already
        // shown). Restarting the transfer would buy twelve frames of redraw
        // that paint the same thing.
        let settled = !structural && *imp.to.borrow() == target && imp.progress.get() >= 1.0;

        let is_urgent = draw_slots.iter().any(|slot| slot.is_urgent);
        let anything_new = draw_slots.iter().any(|slot| slot.is_new);
        *imp.slots.borrow_mut() = draw_slots;
        *imp.from.borrow_mut() = start;
        *imp.to.borrow_mut() = target;

        if structural {
            imp.progress.set(1.0);
            self.queue_resize();
            if anything_new {
                // Seed the fade before its first frame, or a new slot flashes
                // at full size for one frame and then shrinks in.
                imp.appear.set(0.0);
                self.grow_in();
            } else {
                imp.appear.set(1.0);
            }
        } else if !settled {
            imp.progress.set(0.0);
            self.transfer();
        }

        if is_urgent && !was_urgent {
            self.pulse();
        }
        self.queue_draw();
    }

    /// Slide the pill to its new slot, from wherever it is right now.
    ///
    /// Retargeting mid-flight is why the start layout is the *interpolated*
    /// one: a fast run of switches reads as one continuous slide instead of a
    /// series of jumps back to the previous slot.
    fn transfer(&self) {
        let Some(animation) = self.imp().transfer.get() else {
            return;
        };
        let widget = self.clone();
        animation.start(
            AnimationParams::new(self.duration(TRANSFER_MS)).with_easing(Easing::EaseOutCubic),
            Box::new(move |progress| {
                widget.imp().progress.set(progress);
                widget.queue_draw();
            }),
            None,
        );
    }

    /// Fade and grow the slots that were not there a moment ago.
    fn grow_in(&self) {
        let Some(animation) = self.imp().appear_anim.get() else {
            return;
        };
        let widget = self.clone();
        animation.start(
            AnimationParams::new(self.duration(APPEAR_MS)).with_easing(Easing::EaseOutCubic),
            Box::new(move |progress| {
                widget.imp().appear.set(progress);
                widget.queue_draw();
            }),
            None,
        );
    }

    /// A duration of zero means the animator jumps straight to the end state.
    fn duration(&self, milliseconds: u64) -> u64 {
        if self.imp().animate.get() {
            milliseconds
        } else {
            0
        }
    }

    /// Start the urgency pulse: two cycles, then steady.
    fn pulse(&self) {
        let imp = self.imp();
        let Some(animation) = imp.pulse_anim.get() else {
            return;
        };
        let duration = self.duration(PULSE_MS * PULSE_CYCLES as u64);

        let widget = self.clone();
        animation.start(
            AnimationParams::new(duration).with_easing(Easing::Linear),
            Box::new(move |progress| {
                // Starts and ends at zero, so the indicator settles at its
                // normal size however the run is cut short.
                let phase = progress * std::f64::consts::TAU * PULSE_CYCLES;
                widget.imp().pulse.set((1.0 - phase.cos()) / 2.0);
                widget.queue_draw();
            }),
            None,
        );
    }

    /// Where each indicator is right now, for hit testing.
    pub fn rects(&self) -> Vec<SlotRect> {
        self.imp().current_rects()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_follows_the_width_between_dot_and_pill() {
        assert_eq!(activeness(DOT_SIZE, DOT_SIZE), 0.0);
        assert_eq!(activeness(DOT_SIZE + ACTIVE_DELTA, DOT_SIZE), 1.0);
        assert_eq!(activeness(DOT_SIZE + ACTIVE_DELTA / 2.0, DOT_SIZE), 0.5);
        // A pulsing indicator is drawn wider than any layout asked for.
        assert_eq!(activeness(1_000.0, DOT_SIZE), 1.0);
        assert_eq!(activeness(0.0, DOT_SIZE), 0.0);
    }

    #[test]
    fn blending_hits_both_ends_exactly() {
        let black = gdk::RGBA::BLACK;
        let white = gdk::RGBA::WHITE;
        assert_eq!(blend(black, white, 0.0), black);
        assert_eq!(blend(black, white, 1.0), white);
        assert_eq!(blend(black, white, 0.5).red(), 0.5);
    }

    #[test]
    fn the_pulse_starts_and_ends_at_rest() {
        let phase = |progress: f64| {
            let phase = progress * std::f64::consts::TAU * PULSE_CYCLES;
            (1.0 - phase.cos()) / 2.0
        };
        assert!(phase(0.0).abs() < 1e-9, "no jump when urgency arrives");
        assert!(phase(1.0).abs() < 1e-9, "settles at its normal size");
        assert!((phase(0.25) - 1.0).abs() < 1e-9, "peaks mid-cycle");
        for step in 0..=100 {
            let value = phase(f64::from(step) / 100.0);
            assert!((0.0..=1.0).contains(&value), "{value} out of range");
        }
    }

    #[test]
    fn the_inactive_fill_is_dimmer_than_the_active_one() {
        let white = gdk::RGBA::WHITE;
        let inactive = with_alpha(white, white.alpha() * INACTIVE_ALPHA);
        assert!(inactive.alpha() < white.alpha());
        assert_eq!(inactive.red(), white.red());
    }

    #[test]
    fn alpha_is_clamped() {
        assert_eq!(with_alpha(gdk::RGBA::WHITE, 2.0).alpha(), 1.0);
        assert_eq!(with_alpha(gdk::RGBA::WHITE, -1.0).alpha(), 0.0);
    }

    #[test]
    fn the_active_indicator_is_three_dots_wide() {
        assert_eq!(ACTIVE_DELTA, DOT_SIZE * 2.0);
    }
}
