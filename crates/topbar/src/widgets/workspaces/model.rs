//! The workspaces widget's arithmetic, with no GTK in it.
//!
//! Everything here is a pure function of a snapshot plus configuration, which
//! is what lets the widget itself be a thin drawing shell. Three groups:
//!
//! - [`visible_slots`] — which workspaces this bar shows, in order.
//! - [`slot_rects`] and friends — where each indicator sits. The total width
//!   is **independent of which slot is active**, so activating a different
//!   workspace never resizes the widget and never relayouts the bar.
//! - [`ScrollAccumulator`] — turning a stream of scroll deltas into at most
//!   one workspace step per debounce window.

use std::time::{Duration, Instant};

use topbar_services::{WorkspaceId, WorkspaceView, WorkspacesSnapshot};

/// Diameter of an inactive indicator, in pixels.
pub const DOT_SIZE: f32 = 8.0;
/// Width of the active pill: three dots wide, GNOME Activities style.
pub const ACTIVE_WIDTH: f32 = DOT_SIZE * 3.0;
/// Gap between two indicators, in pixels.
pub const SLOT_GAP: f32 = 8.0;
/// Horizontal padding inside a labelled slot, per side.
pub const LABEL_PAD_X: f32 = 7.0;
/// How much wider the active indicator is than the same slot inactive.
///
/// Constant across slot kinds on purpose: it is what makes the widget's total
/// width the same whichever slot is active.
pub const ACTIVE_DELTA: f32 = ACTIVE_WIDTH - DOT_SIZE;

/// How a slot is labelled, from `widgets.workspaces.label_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LabelType {
    /// Dots and a pill, no text.
    #[default]
    None,
    /// The workspace's index on its output.
    Index,
    /// The workspace's name, falling back to its index.
    Name,
}

impl LabelType {
    /// Parse a configured value. Validation has already rejected anything else.
    pub fn parse(value: &str) -> Self {
        match value {
            "index" => Self::Index,
            "name" => Self::Name,
            _ => Self::None,
        }
    }

    /// The text a slot shows, or `None` when the widget draws dots.
    fn label(self, view: &WorkspaceView) -> Option<String> {
        match self {
            Self::None => None,
            Self::Index => Some(view.idx.to_string()),
            Self::Name => Some(view.name.clone().unwrap_or_else(|| view.idx.to_string())),
        }
    }
}

/// One indicator, as the widget draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    /// Workspace id, used to focus it.
    pub id: WorkspaceId,
    /// Text to draw, or `None` for a dot.
    pub label: Option<String>,
    /// Whether this slot gets the pill.
    pub is_active: bool,
    /// Only the globally focused slot may ignore a click.
    pub is_focused: bool,
    /// Whether this slot wants attention.
    pub is_urgent: bool,
}

/// What the widget's configuration says about which workspaces to show.
#[derive(Debug, Clone, Copy)]
pub struct SlotOptions<'a> {
    /// This bar's connector name.
    pub connector: &'a str,
    /// Show only workspaces on this bar's output.
    pub filter_by_output: bool,
    /// Show workspaces that hold no windows.
    pub show_unoccupied: bool,
    /// How slots are labelled.
    pub label_type: LabelType,
}

/// The slots this bar shows, left to right.
///
/// An empty workspace is hidden unless it is the active one — hiding the
/// workspace you are looking at would be absurd — or it is asking for
/// attention, which is the whole point of urgency.
///
/// A *named* workspace is never hidden. niri's named workspaces are declared in
/// the compositor's configuration and exist whether or not anything is open on
/// them, so they are part of the layout the user arranged rather than something
/// that comes and goes with its windows: a "chat" dot that vanishes the moment
/// the chat window closes is a row that renumbers itself under the pointer.
pub fn visible_slots(snapshot: &WorkspacesSnapshot, options: SlotOptions) -> Vec<Slot> {
    let views: Vec<&WorkspaceView> = if options.filter_by_output {
        snapshot.for_output(options.connector).iter().collect()
    } else {
        snapshot.all().collect()
    };

    views
        .into_iter()
        .filter_map(|view| {
            // With every output on show, "active" has to mean the one focused
            // workspace: otherwise each monitor contributes its own pill.
            let is_active = if options.filter_by_output {
                view.is_active
            } else {
                view.is_focused
            };
            let keep = options.show_unoccupied
                || view.name.is_some()
                || view.is_persistent
                || view.has_windows
                || is_active
                || view.is_urgent;
            keep.then(|| Slot {
                id: view.id,
                label: options.label_type.label(view),
                is_active,
                is_focused: view.is_focused,
                is_urgent: view.is_urgent,
            })
        })
        .collect()
}

/// Where one indicator sits inside the widget.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SlotRect {
    /// Left edge, in pixels from the widget's origin.
    pub x: f32,
    /// Painted width, in pixels.
    pub width: f32,
}

/// The inactive width of a slot whose label measures `label_width` pixels.
///
/// `None` — a dot — is the minimum; a label widens the slot but never narrows
/// it below a dot.
pub fn inactive_width(label_width: Option<f32>) -> f32 {
    match label_width {
        None => DOT_SIZE,
        Some(width) => (width + 2.0 * LABEL_PAD_X).max(DOT_SIZE),
    }
}

/// Lay `widths` out left to right, widening the active slot.
///
/// `active` is an index into `widths`. When it is `None` the active slot's
/// extra width is still reserved, as trailing space, so a bar whose active
/// workspace has scrolled out of view does not twitch narrower.
pub fn slot_rects(widths: &[f32], active: Option<usize>) -> Vec<SlotRect> {
    let mut rects = Vec::with_capacity(widths.len());
    let mut x = 0.0;
    for (index, &width) in widths.iter().enumerate() {
        let width = if Some(index) == active {
            width + ACTIVE_DELTA
        } else {
            width
        };
        rects.push(SlotRect { x, width });
        x += width + SLOT_GAP;
    }
    rects
}

/// Total width for `widths`, whichever slot is active.
pub fn total_width(widths: &[f32]) -> f32 {
    if widths.is_empty() {
        return 0.0;
    }
    let sum: f32 = widths.iter().sum();
    sum + ACTIVE_DELTA + SLOT_GAP * (widths.len() - 1) as f32
}

/// Interpolate between two layouts.
///
/// Both layouts always have the same total width, so interpolating each slot
/// independently cannot leave the row wider or narrower mid-flight.
pub fn lerp_rects(from: &[SlotRect], to: &[SlotRect], progress: f32) -> Vec<SlotRect> {
    to.iter()
        .enumerate()
        .map(|(index, target)| match from.get(index) {
            Some(start) => SlotRect {
                x: start.x + (target.x - start.x) * progress,
                width: start.width + (target.width - start.width) * progress,
            },
            None => *target,
        })
        .collect()
}

/// Whether the slot for `id` is a workspace *arriving*, given what was shown
/// before.
///
/// The first population of an empty strip is the widget appearing, not
/// workspaces appearing inside it. Treating it as an arrival would fade every
/// indicator in from nothing — which makes the panel's first paint depend on
/// an animation having run, and a frame clock is not something a first paint
/// may depend on.
pub fn is_appearance(previous_ids: &[WorkspaceId], id: WorkspaceId) -> bool {
    !previous_ids.is_empty() && !previous_ids.contains(&id)
}

/// The slot at `x`, or the nearest one when the click landed in a gap.
///
/// Gaps are as wide as a dot, so swallowing clicks that land in one would make
/// the widget feel unreliable for a two-pixel miss. Distance is measured to the
/// nearest *edge*, not the centre: next to a 24 px pill, a centre measurement
/// would hand a click in the gap to the small dot on the other side.
pub fn hit_test(rects: &[SlotRect], x: f32) -> Option<usize> {
    let mut nearest = None;
    let mut best = f32::INFINITY;
    for (index, rect) in rects.iter().enumerate() {
        let distance = (rect.x - x).max(x - (rect.x + rect.width)).max(0.0);
        if distance == 0.0 {
            return Some(index);
        }
        if distance < best {
            best = distance;
            nearest = Some(index);
        }
    }
    nearest
}

/// The slot `steps` away from `current`, clamped to the ends.
///
/// Clamped rather than wrapping: GNOME's workspace switcher stops at the ends,
/// and wrapping past the last workspace on a fast scroll is disorienting.
pub fn step_target(current: usize, steps: i32, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let last = (len - 1) as i32;
    let target = (current as i32 + steps).clamp(0, last) as usize;
    (target != current).then_some(target)
}

/// How long scroll deltas are pooled before they become one step.
pub const SCROLL_DEBOUNCE: Duration = Duration::from_millis(150);
/// Accumulated delta that counts as one workspace step.
const SCROLL_STEP: f64 = 1.0;

/// Turns a stream of scroll deltas into workspace steps.
///
/// Two things it has to get right: a high-resolution touchpad sends many small
/// deltas that must add up to one step rather than none, and a mouse wheel
/// sends discrete `1.0`s faster than the compositor can switch. The debounce
/// window covers both — the first notch moves immediately, the rest of the
/// flick pools until the window is over.
#[derive(Debug, Default)]
pub struct ScrollAccumulator {
    pending: f64,
    last_step: Option<Instant>,
}

impl ScrollAccumulator {
    /// Feed one scroll delta; returns the number of steps to take, if any.
    pub fn feed(&mut self, delta: f64, now: Instant) -> Option<i32> {
        self.pending += delta;

        if let Some(last) = self.last_step
            && now.duration_since(last) < SCROLL_DEBOUNCE
        {
            return None;
        }

        let steps = (self.pending / SCROLL_STEP).trunc();
        if steps == 0.0 {
            return None;
        }

        self.pending -= steps * SCROLL_STEP;
        self.last_step = Some(now);
        Some(steps as i32)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn view(id: u64, idx: usize, occupied: bool) -> WorkspaceView {
        WorkspaceView {
            id: WorkspaceId::Niri(id),
            idx,
            name: None,
            is_active: false,
            is_focused: false,
            is_urgent: false,
            has_windows: occupied,
            is_persistent: false,
        }
    }

    fn snapshot(outputs: &[(&str, Vec<WorkspaceView>)]) -> WorkspacesSnapshot {
        let mut map = BTreeMap::new();
        for (connector, views) in outputs {
            map.insert((*connector).to_string(), views.clone());
        }
        WorkspacesSnapshot {
            connected: true,
            outputs: map,
            focused_output: None,
        }
    }

    fn options(connector: &str) -> SlotOptions<'_> {
        SlotOptions {
            connector,
            filter_by_output: true,
            show_unoccupied: false,
            label_type: LabelType::None,
        }
    }

    #[test]
    fn empty_workspaces_are_hidden_but_the_active_one_never_is() {
        let mut views = vec![view(1, 1, true), view(2, 2, false), view(3, 3, false)];
        views[1].is_active = true;

        let snapshot = snapshot(&[("eDP-1", views)]);
        let slots = visible_slots(&snapshot, options("eDP-1"));
        assert_eq!(
            slots.iter().map(|slot| slot.id).collect::<Vec<_>>(),
            vec![WorkspaceId::Niri(1), WorkspaceId::Niri(2)],
            "the empty active workspace stays, the empty inactive one goes"
        );
    }

    #[test]
    fn an_urgent_empty_workspace_is_shown() {
        let mut views = vec![view(1, 1, true), view(2, 2, false)];
        views[1].is_urgent = true;

        let slots = visible_slots(&snapshot(&[("eDP-1", views)]), options("eDP-1"));
        assert_eq!(slots.len(), 2);
        assert!(slots[1].is_urgent);
    }

    #[test]
    fn a_named_workspace_is_shown_while_it_is_empty() {
        let mut views = vec![view(1, 1, true), view(2, 2, false), view(3, 3, false)];
        views[1].name = Some("chat".into());

        let slots = visible_slots(&snapshot(&[("eDP-1", views)]), options("eDP-1"));
        assert_eq!(
            slots.iter().map(|slot| slot.id).collect::<Vec<_>>(),
            vec![WorkspaceId::Niri(1), WorkspaceId::Niri(2)],
            "the empty named workspace stays; the empty nameless one goes"
        );
    }

    #[test]
    fn persistent_numbered_and_named_native_workspaces_keep_identity_and_focus() {
        let mut numbered = view(3, 300, false);
        numbered.id = WorkspaceId::Hyprland(3);
        numbered.is_persistent = true;
        numbered.is_active = true;
        let mut named = view(4, 301, false);
        named.id = WorkspaceId::Hyprland(-9);
        named.name = Some("notes".into());
        let mut options = options("DP-1");
        options.label_type = LabelType::Index;
        let slots = visible_slots(&snapshot(&[("DP-1", vec![numbered, named])]), options);
        assert_eq!(slots[0].label.as_deref(), Some("300"));
        assert!(
            slots[0].is_active && !slots[0].is_focused,
            "an active workspace on another monitor must accept a click"
        );
        assert_eq!(slots[1].id, WorkspaceId::Hyprland(-9));
    }

    #[test]
    fn show_unoccupied_keeps_everything() {
        let views = vec![view(1, 1, true), view(2, 2, false), view(3, 3, false)];
        let mut options = options("eDP-1");
        options.show_unoccupied = true;

        assert_eq!(
            visible_slots(&snapshot(&[("eDP-1", views)]), options).len(),
            3
        );
    }

    #[test]
    fn filtering_by_output_ignores_the_other_monitors() {
        let snapshot = snapshot(&[
            ("DP-2", vec![view(11, 1, true), view(12, 2, true)]),
            ("eDP-1", vec![view(1, 1, true)]),
        ]);

        let mine = visible_slots(&snapshot, options("eDP-1"));
        assert_eq!(
            mine.iter().map(|slot| slot.id).collect::<Vec<_>>(),
            vec![WorkspaceId::Niri(1)]
        );

        let mut all = options("eDP-1");
        all.filter_by_output = false;
        assert_eq!(
            visible_slots(&snapshot, all)
                .iter()
                .map(|slot| slot.id)
                .collect::<Vec<_>>(),
            vec![
                WorkspaceId::Niri(11),
                WorkspaceId::Niri(12),
                WorkspaceId::Niri(1)
            ],
            "connector order, then index order"
        );
    }

    #[test]
    fn a_bar_on_an_unknown_output_shows_nothing() {
        let snapshot = snapshot(&[("eDP-1", vec![view(1, 1, true)])]);
        assert!(visible_slots(&snapshot, options("HDMI-A-9")).is_empty());
    }

    #[test]
    fn showing_every_output_follows_the_focused_workspace() {
        let mut internal = vec![view(1, 1, true)];
        internal[0].is_active = true;
        let mut external = vec![view(11, 1, true)];
        external[0].is_active = true;
        external[0].is_focused = true;

        let snapshot = snapshot(&[("DP-2", external), ("eDP-1", internal)]);
        let mut options = options("eDP-1");
        options.filter_by_output = false;

        let slots = visible_slots(&snapshot, options);
        let active: Vec<WorkspaceId> = slots
            .iter()
            .filter(|slot| slot.is_active)
            .map(|slot| slot.id)
            .collect();
        assert_eq!(
            active,
            vec![WorkspaceId::Niri(11)],
            "exactly one pill across all outputs"
        );
    }

    #[test]
    fn labels_follow_the_label_type() {
        let mut views = vec![view(1, 1, true)];
        views[0].name = Some("browser".into());
        let snapshot = snapshot(&[("eDP-1", views)]);

        let label = |label_type| {
            let mut options = options("eDP-1");
            options.label_type = label_type;
            visible_slots(&snapshot, options)[0].label.clone()
        };

        assert_eq!(label(LabelType::None), None);
        assert_eq!(label(LabelType::Index), Some("1".to_string()));
        assert_eq!(label(LabelType::Name), Some("browser".to_string()));
    }

    #[test]
    fn a_nameless_workspace_falls_back_to_its_index() {
        let snapshot = snapshot(&[("eDP-1", vec![view(1, 4, true)])]);
        let mut options = options("eDP-1");
        options.label_type = LabelType::Name;
        assert_eq!(
            visible_slots(&snapshot, options)[0].label.as_deref(),
            Some("4")
        );
    }

    #[test]
    fn label_types_parse_from_config_values() {
        assert_eq!(LabelType::parse("none"), LabelType::None);
        assert_eq!(LabelType::parse("index"), LabelType::Index);
        assert_eq!(LabelType::parse("name"), LabelType::Name);
        assert_eq!(LabelType::parse("nonsense"), LabelType::None);
    }

    /// The whole point of the geometry: `measure()` must not depend on which
    /// workspace is active, or every switch would relayout the bar.
    #[test]
    fn total_width_does_not_depend_on_the_active_slot() {
        let widths = vec![DOT_SIZE; 4];
        let expected = total_width(&widths);

        for active in 0..widths.len() {
            let rects = slot_rects(&widths, Some(active));
            let last = rects.last().expect("four slots");
            assert!(
                (last.x + last.width - expected).abs() < f32::EPSILON,
                "active {active} changed the total width"
            );
        }
    }

    #[test]
    fn width_is_reserved_even_with_no_active_slot() {
        let widths = vec![DOT_SIZE; 3];
        let rects = slot_rects(&widths, None);
        let last = rects.last().expect("three slots");
        assert!(last.x + last.width < total_width(&widths));
        assert!(rects.iter().all(|rect| rect.width == DOT_SIZE));
    }

    #[test]
    fn the_active_slot_is_the_wide_one() {
        let rects = slot_rects(&[DOT_SIZE; 3], Some(1));
        assert_eq!(rects[0].width, DOT_SIZE);
        assert_eq!(rects[1].width, ACTIVE_WIDTH);
        assert_eq!(rects[2].width, DOT_SIZE);
        assert_eq!(rects[1].x, DOT_SIZE + SLOT_GAP);
        assert_eq!(rects[2].x, DOT_SIZE + SLOT_GAP + ACTIVE_WIDTH + SLOT_GAP);
    }

    #[test]
    fn labelled_slots_widen_but_keep_the_active_delta() {
        let widths = [
            inactive_width(Some(20.0)),
            inactive_width(Some(6.0)),
            inactive_width(None),
        ];
        assert_eq!(widths[0], 20.0 + 2.0 * LABEL_PAD_X);
        assert_eq!(widths[2], DOT_SIZE);

        let total = total_width(&widths);
        for active in 0..widths.len() {
            let rects = slot_rects(&widths, Some(active));
            let last = rects.last().expect("three slots");
            assert!(
                (last.x + last.width - total).abs() < 0.001,
                "active {active}"
            );
        }
    }

    #[test]
    fn an_empty_widget_has_no_width() {
        assert_eq!(total_width(&[]), 0.0);
        assert!(slot_rects(&[], None).is_empty());
        assert!(hit_test(&[], 4.0).is_none());
    }

    #[test]
    fn interpolation_starts_at_from_and_lands_on_to() {
        let from = slot_rects(&[DOT_SIZE; 3], Some(0));
        let to = slot_rects(&[DOT_SIZE; 3], Some(2));

        assert_eq!(lerp_rects(&from, &to, 0.0), from);
        assert_eq!(lerp_rects(&from, &to, 1.0), to);

        let half = lerp_rects(&from, &to, 0.5);
        assert_eq!(half[0].width, (DOT_SIZE + ACTIVE_WIDTH) / 2.0);
        assert_eq!(half[2].width, (DOT_SIZE + ACTIVE_WIDTH) / 2.0);
        // The row never changes total width, mid-flight included.
        let end = half.last().expect("three slots");
        assert!((end.x + end.width - total_width(&[DOT_SIZE; 3])).abs() < 0.001);
    }

    #[test]
    fn interpolating_onto_a_longer_layout_uses_the_new_slots_directly() {
        let from = slot_rects(&[DOT_SIZE; 2], Some(0));
        let to = slot_rects(&[DOT_SIZE; 3], Some(0));
        let half = lerp_rects(&from, &to, 0.5);
        assert_eq!(half.len(), 3);
        assert_eq!(half[2], to[2], "a brand-new slot appears at its target");
    }

    #[test]
    fn the_first_paint_is_not_an_appearance() {
        // Nothing was shown before: this is the widget arriving, and it has to
        // be drawn at once rather than faded in.
        assert!(!is_appearance(&[], WorkspaceId::Niri(1)));
        assert!(!is_appearance(&[], WorkspaceId::Hyprland(-7)));

        // Once something is on screen, a workspace that was not there is.
        assert!(is_appearance(
            &[WorkspaceId::Niri(1), WorkspaceId::Niri(2)],
            WorkspaceId::Hyprland(1)
        ));
        assert!(!is_appearance(
            &[WorkspaceId::Niri(1), WorkspaceId::Niri(2)],
            WorkspaceId::Niri(2)
        ));
    }

    #[test]
    fn clicks_land_on_the_slot_under_them() {
        let rects = slot_rects(&[DOT_SIZE; 3], Some(1));
        assert_eq!(hit_test(&rects, 2.0), Some(0));
        assert_eq!(hit_test(&rects, rects[1].x + 2.0), Some(1));
        assert_eq!(hit_test(&rects, rects[2].x + 4.0), Some(2));
    }

    #[test]
    fn clicks_in_a_gap_pick_the_nearer_slot() {
        let rects = slot_rects(&[DOT_SIZE; 3], Some(1));
        let gap_start = rects[0].x + rects[0].width;
        assert_eq!(hit_test(&rects, gap_start + 1.0), Some(0));
        // Nearest edge, not nearest centre: this point is one pixel from the
        // pill and seven from the dot.
        assert_eq!(hit_test(&rects, rects[1].x - 1.0), Some(1));
        // Past both ends, the nearest end wins.
        assert_eq!(hit_test(&rects, -50.0), Some(0));
        assert_eq!(hit_test(&rects, 9_000.0), Some(2));
    }

    #[test]
    fn stepping_clamps_at_both_ends() {
        assert_eq!(step_target(0, 1, 3), Some(1));
        assert_eq!(step_target(2, -1, 3), Some(1));
        assert_eq!(step_target(0, -1, 3), None, "already at the first");
        assert_eq!(step_target(2, 1, 3), None, "already at the last");
        assert_eq!(step_target(0, 5, 3), Some(2), "a fast flick clamps");
        assert_eq!(step_target(0, 1, 0), None);
        assert_eq!(step_target(0, 0, 3), None);
    }

    #[test]
    fn one_notch_steps_immediately() {
        let mut scroll = ScrollAccumulator::default();
        assert_eq!(scroll.feed(1.0, Instant::now()), Some(1));
    }

    #[test]
    fn a_burst_inside_the_window_yields_one_step() {
        let mut scroll = ScrollAccumulator::default();
        let start = Instant::now();
        assert_eq!(scroll.feed(1.0, start), Some(1));

        for tick in 1..=5 {
            let now = start + Duration::from_millis(10 * tick);
            assert_eq!(scroll.feed(1.0, now), None, "still inside the window");
        }

        // Once the window is over, the pooled deltas are spent in one go.
        let after = start + SCROLL_DEBOUNCE + Duration::from_millis(1);
        assert_eq!(scroll.feed(0.0, after), Some(5));
    }

    #[test]
    fn touchpad_fractions_accumulate_into_a_step() {
        let mut scroll = ScrollAccumulator::default();
        let start = Instant::now();
        for _ in 0..4 {
            assert_eq!(scroll.feed(0.2, start), None, "0.8 is not a step yet");
        }
        assert_eq!(scroll.feed(0.2, start), Some(1));
    }

    #[test]
    fn scrolling_back_the_other_way_cancels_out() {
        let mut scroll = ScrollAccumulator::default();
        let start = Instant::now();
        assert_eq!(scroll.feed(0.5, start), None);
        assert_eq!(scroll.feed(-0.5, start), None, "back to nothing pending");
        assert_eq!(scroll.feed(-0.6, start), None, "still short of a step");
        assert_eq!(scroll.feed(-0.5, start), Some(-1));
    }
}
