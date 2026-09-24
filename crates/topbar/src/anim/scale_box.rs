//! A container that fakes a scale transform with a rounded clip.
//!
//! Ported from v1 `widgets/scale_box.rs`. Real GTK4 `transform: scale()` — in
//! CSS or in a render node — grows the render tree's cached surfaces without
//! bound when it is animated every frame, so the popover's grow-in is drawn
//! instead: the child is always allocated at full size and `snapshot()` clips
//! it to a rectangle that grows to the full bounds.
//!
//! One deliberate difference from v1: the clip is anchored at the **top**
//! rather than centred vertically, so a popover appears to grow out of the bar
//! it hangs from (`transform-origin: top center`) instead of out of thin air.
//!
//! The optional outline exists because a CSS border lives on the *child*, at
//! full size, and would therefore sit outside the clip for the whole
//! animation — the surface would look borderless until the last frame. The
//! outline is drawn on the clip boundary instead while a run is in flight; the
//! child hides its own border for that time.
//!
//! Scale changes only ever `queue_draw()`: no relayout, no CSS resolution.

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use std::cell::Cell;

mod imp {
    use super::*;

    pub struct ScaleBox {
        /// Current scale factor, `0.0..=1.0` (1.0 = full size).
        pub(super) scale: Cell<f64>,
        /// Corner radius of the clip, in pixels.
        pub(super) radius: Cell<f32>,
        /// Outline width drawn on the clip boundary. 0 disables it.
        pub(super) outline_width: Cell<f32>,
        /// The single child.
        pub(super) child: glib::WeakRef<gtk4::Widget>,
    }

    impl Default for ScaleBox {
        fn default() -> Self {
            Self {
                scale: Cell::new(1.0),
                radius: Cell::default(),
                outline_width: Cell::default(),
                child: glib::WeakRef::new(),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ScaleBox {
        const NAME: &'static str = "TopbarScaleBox";
        type Type = super::ScaleBox;
        type ParentType = gtk4::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("scale-box");
        }
    }

    impl ObjectImpl for ScaleBox {
        fn dispose(&self) {
            if let Some(child) = self.child.upgrade() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for ScaleBox {
        fn request_mode(&self) -> gtk4::SizeRequestMode {
            self.child
                .upgrade()
                .map_or(gtk4::SizeRequestMode::ConstantSize, |child| {
                    child.request_mode()
                })
        }

        fn measure(&self, orientation: gtk4::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            match self.child.upgrade() {
                Some(child) => child.measure(orientation, for_size),
                None => (0, 0, -1, -1),
            }
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            // Always the full allocation: the scale is purely a snapshot-time
            // clip, so nothing inside the child ever reflows mid-animation.
            if let Some(child) = self.child.upgrade() {
                child.allocate(width, height, baseline, None);
            }
        }

        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            let Some(child) = self.child.upgrade() else {
                return;
            };
            let scale = self.scale.get();
            if scale <= 0.0 {
                return;
            }

            let widget = self.obj();
            let width = widget.width() as f32;
            let height = widget.height() as f32;
            let radius = self.radius.get();

            if scale >= 1.0 {
                widget.snapshot_child(&child, snapshot);
                let full = gtk4::graphene::Rect::new(0.0, 0.0, width, height);
                self.snapshot_outline(snapshot, full, radius, &child);
                return;
            }

            // Grow from the top edge, centred horizontally.
            let clipped_width = width * scale as f32;
            let clipped_height = height * scale as f32;
            let rect = gtk4::graphene::Rect::new(
                (width - clipped_width) / 2.0,
                0.0,
                clipped_width,
                clipped_height,
            );

            snapshot.push_rounded_clip(&rounded(rect, radius));
            widget.snapshot_child(&child, snapshot);
            snapshot.pop();

            self.snapshot_outline(snapshot, rect, radius, &child);
        }
    }

    impl ScaleBox {
        fn snapshot_outline(
            &self,
            snapshot: &gtk4::Snapshot,
            rect: gtk4::graphene::Rect,
            radius: f32,
            child: &gtk4::Widget,
        ) {
            let width = self.outline_width.get();
            if width <= 0.0 {
                return;
            }
            let mut color = child.color();
            color.set_alpha(color.alpha() * 0.08);
            if color.alpha() <= 0.0 {
                return;
            }
            snapshot.append_border(&rounded(rect, radius), &[width; 4], &[color; 4]);
        }
    }

    /// A rounded rectangle with the same radius on all four corners.
    fn rounded(rect: gtk4::graphene::Rect, radius: f32) -> gtk4::gsk::RoundedRect {
        let corner = gtk4::graphene::Size::new(radius, radius);
        gtk4::gsk::RoundedRect::new(rect, corner, corner, corner, corner)
    }
}

glib::wrapper! {
    /// A one-child container that simulates `transform: scale()` by clipping.
    ///
    /// The child keeps its full allocation at every scale; only the drawn
    /// region shrinks. See the module docs for why this is not a real
    /// transform.
    pub struct ScaleBox(ObjectSubclass<imp::ScaleBox>)
        @extends gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl Default for ScaleBox {
    fn default() -> Self {
        Self::new()
    }
}

impl ScaleBox {
    /// Create a box at full scale with no child.
    pub fn new() -> Self {
        glib::Object::builder().build()
    }

    /// Set the drawn fraction of the child, clamped to `0.0..=1.0`.
    pub fn set_scale(&self, scale: f64) {
        let imp = self.imp();
        let scale = scale.clamp(0.0, 1.0);
        if (imp.scale.get() - scale).abs() < f64::EPSILON {
            return;
        }
        imp.scale.set(scale);
        self.queue_draw();
    }

    /// Set the corner radius of the clip, in pixels.
    pub fn set_radius(&self, radius: f32) {
        let imp = self.imp();
        if (imp.radius.get() - radius).abs() < f32::EPSILON {
            return;
        }
        imp.radius.set(radius);
        self.queue_draw();
    }

    /// Draw (or stop drawing) an outline on the clip boundary.
    ///
    /// Pass a width of `0.0` to turn it off.
    pub fn set_outline(&self, width: f32) {
        let imp = self.imp();
        let width = width.max(0.0);
        if (imp.outline_width.get() - width).abs() < f32::EPSILON {
            return;
        }
        imp.outline_width.set(width);
        self.queue_draw();
    }

    /// Parent `child`, replacing whatever was there.
    pub fn set_child(&self, child: &impl IsA<gtk4::Widget>) {
        let imp = self.imp();
        if let Some(previous) = imp.child.upgrade() {
            previous.unparent();
        }
        let child = child.as_ref();
        child.set_parent(self);
        imp.child.set(Some(child));
    }

    /// Unparent the current child, if any.
    ///
    /// The caller keeps its own reference: popover content is retained across
    /// open/close cycles and re-parented on the next open.
    pub fn remove_child(&self) {
        if let Some(child) = self.imp().child.upgrade() {
            child.unparent();
        }
        self.imp().child.set(None::<&gtk4::Widget>);
    }
}
