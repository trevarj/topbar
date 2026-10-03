//! The sliders block: output volume, the microphone, and the backlight.
//!
//! One shape, three uses:
//!
//! ```text
//! [icon] ──────────●───────  [chevron]     the chevron only where there is
//!  ↑ mute                     ↑ output      more than one output to choose
//! ```
//!
//! Every one of them has the same trap in it. Service state arrives, the
//! slider is set from it, GTK emits `value-changed`, the handler sends a
//! command back to the service, the service publishes it, and round it goes.
//! The guard is a flag the render path raises ([`Slider::updating`]) plus
//! setters that compare before they write, so a value that did not move never
//! reaches the signal at all.
//!
//! Every command carries [`ChangeSource::Ui`], which is what keeps the OSD
//! capsule off the screen while the user is dragging the control that *is* the
//! feedback. Forgetting it puts a capsule under the pointer restating the
//! number the slider already shows.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Align, Button, Image, Label, Orientation, Scale};
use topbar_core::config::MicSlider;
use topbar_services::{AudioState, BrightnessState, ChangeSource, Services};

use crate::anim::ripple;
use crate::bridge::{self, BindingGuard};
use crate::style::{classes, icons};
use crate::surfaces::inline::{self, names};
use crate::surfaces::search::ChoiceFilter;
use crate::widgets::expander::{ROW_REVEAL_MS, Section};
use crate::widgets::quick_settings::model;
use crate::widgets::quick_settings::{attempt, set_icon};

/// Space between a slider's icon, its track and its chevron.
const ROW_SPACING: i32 = 6;

/// One slider row: an icon, a track, and optionally a chevron.
struct Slider {
    row: gtk4::Box,
    icon: Image,
    /// The mute button, on the two rows that have something to mute.
    ///
    /// `None` on the brightness row, where the icon is an image and not a
    /// control at all — see [`Slider::new`].
    button: Option<Button>,
    scale: Scale,
    /// Raised while the render path is writing into the scale, so the
    /// `value-changed` it causes is not mistaken for the user moving it.
    updating: Cell<bool>,
}

impl Slider {
    /// Build a row. `interactive_icon` makes the icon a mute button.
    fn new(css: &str, interactive_icon: bool) -> Rc<Self> {
        let row = gtk4::Box::new(Orientation::Horizontal, ROW_SPACING);
        row.add_css_class(classes::QS_SLIDER_ROW);
        row.add_css_class(css);

        let icon = Image::new();
        icon.add_css_class(classes::QS_ICON);

        // Brightness has nothing to mute, so its icon is an image and not a
        // button at all. It used to be an insensitive button, which was the
        // obvious way to say "nothing to press here" and the wrong one: GTK's
        // own theme draws an insensitive image at half strength however the
        // panel colours it, so the icon beside the one slider that always
        // works was the faintest thing in the block. An image in the same box
        // keeps the column and says nothing it should not.
        let button = interactive_icon.then(|| {
            let button = Button::new();
            button.add_css_class(classes::QS_SLIDER_ICON);
            button.set_child(Some(&icon));
            ripple::install(&button);
            button.set_valign(Align::Center);
            row.append(&button);
            button
        });
        if button.is_none() {
            icon.add_css_class(classes::QS_SLIDER_ICON);
            icon.set_valign(Align::Center);
            row.append(&icon);
        }

        let scale = Scale::with_range(Orientation::Horizontal, 0.0, 100.0, 1.0);
        scale.add_css_class(classes::QS_SLIDER);
        scale.set_draw_value(false);
        scale.set_hexpand(true);
        scale.set_valign(Align::Center);
        row.append(&scale);

        Rc::new(Self {
            row,
            icon,
            button,
            scale,
            updating: Cell::new(false),
        })
    }

    /// Write into the scale without waking the handler.
    fn quietly(&self, write: impl FnOnce()) {
        self.updating.set(true);
        write();
        self.updating.set(false);
    }

    /// Set the upper bound, only if it moved.
    fn set_ceiling(&self, ceiling: f64) {
        let adjustment = self.scale.adjustment();
        if (adjustment.upper() - ceiling).abs() > f64::EPSILON {
            adjustment.set_upper(ceiling);
        }
    }

    /// Set the value, only if it moved.
    fn set_value(&self, value: f64) {
        if (self.scale.value() - value).abs() > f64::EPSILON {
            self.scale.set_value(value);
        }
    }
}

/// The whole block, and everything keeping it alive.
pub struct Sliders {
    root: gtk4::Box,
    output: Rc<Slider>,
    microphone: Rc<Slider>,
    brightness: Rc<Slider>,
    /// The microphone row's slot, revealed while something is recording.
    mic_slot: Rc<Section>,
    /// When the microphone row is shown at all.
    mic: MicSlider,
    /// The output chooser's slot.
    chooser_slot: Rc<Section>,
    chooser_list: gtk4::Box,
    device_filter: Rc<ChoiceFilter>,
    device_rows: RefCell<Vec<(String, String, Image)>>,
    chooser_button: Button,
    /// The chooser's chevron, held rather than looked up.
    ///
    /// `ripple::install` puts the button's child inside an overlay of its own,
    /// so asking the button for its child hands back that overlay and a
    /// downcast to `Image` quietly fails — which is how this arrow spent its
    /// whole life pointing down, whether the list was open or not.
    chooser_icon: Image,
    /// Whether the chooser is open, so a rebuild does not close it.
    chooser_open: Cell<bool>,
    services: Services,
    _slots: Vec<inline::InlineSlot>,
    /// Filled in after construction, because a subscription needs a weak
    /// handle on the finished `Rc` to render through.
    bindings: std::cell::RefCell<Vec<BindingGuard>>,
}

impl Sliders {
    /// Build the block from the configuration's per-slider switches.
    pub fn new(
        services: &Services,
        show_audio: bool,
        mic: MicSlider,
        show_brightness: bool,
    ) -> Rc<Self> {
        let root = gtk4::Box::new(Orientation::Vertical, ROW_SPACING);
        root.add_css_class(classes::QS_SLIDERS);

        let output = Slider::new("qs-slider-output", true);
        let microphone = Slider::new("qs-slider-mic", true);
        let brightness = Slider::new("qs-slider-brightness", false);
        set_icon(&brightness.icon, icons::BRIGHTNESS);

        let chooser_button = Button::new();
        chooser_button.add_css_class(classes::QS_CHOOSER);
        let chooser_icon = Image::from_icon_name(icons::EXPAND);
        chooser_button.set_child(Some(&chooser_icon));
        ripple::install(&chooser_button);
        chooser_button.set_valign(Align::Center);
        chooser_button.set_visible(false);
        output.row.append(&chooser_button);

        let chooser_scope = gtk4::Box::new(Orientation::Vertical, 2);
        chooser_scope.add_css_class(classes::QS_DEVICE_LIST);
        let device_filter = ChoiceFilter::new(&chooser_scope, "Filter output devices");
        let chooser_list = gtk4::Box::new(Orientation::Vertical, 2);
        chooser_scope.append(&chooser_list);
        let chooser_slot = Section::new(&chooser_scope);

        let (volume_error, volume_slot) = inline::slot(names::VOLUME);
        let (mic_error, mic_error_slot) = inline::slot(names::MICROPHONE);
        let (brightness_error, brightness_slot) = inline::slot(names::BRIGHTNESS);

        // The output block: row, its failures, then the device list under both.
        if show_audio {
            root.append(&output.row);
            root.append(&volume_error);
            root.append(chooser_slot.root());
        }

        // The microphone row lives in a slot of its own so it can arrive and
        // leave with the recording rather than sitting there greyed out.
        let mic_column = gtk4::Box::new(Orientation::Vertical, 0);
        mic_column.append(&microphone.row);
        mic_column.append(&mic_error);
        let mic_slot = Section::with_duration(&mic_column, ROW_REVEAL_MS);
        if mic != MicSlider::Never {
            root.append(mic_slot.root());
        }

        if show_brightness {
            root.append(&brightness.row);
            root.append(&brightness_error);
        }

        let sliders = Rc::new(Self {
            root,
            output,
            microphone,
            brightness,
            mic_slot,
            mic,
            chooser_slot,
            chooser_list,
            device_filter,
            device_rows: RefCell::new(Vec::new()),
            chooser_button,
            chooser_icon,
            chooser_open: Cell::new(false),
            services: services.clone(),
            _slots: vec![volume_slot, mic_error_slot, brightness_slot],
            bindings: std::cell::RefCell::new(Vec::new()),
        });

        Self::wire(
            &sliders,
            show_audio,
            mic != MicSlider::Never,
            show_brightness,
        );
        sliders
    }

    /// The widget to put in the panel.
    pub fn root(&self) -> &gtk4::Box {
        &self.root
    }

    /// Re-render from current state. Runs every time the panel opens.
    pub fn refresh(&self) {
        self.render_audio(&self.services.audio.current());
        self.render_brightness(&self.services.brightness.current());
    }

    /// Close anything the sliders block opened.
    pub fn collapse(&self) {
        self.chooser_open.set(false);
        self.chooser_slot.collapse_now();
        set_icon(&self.chooser_icon, icons::EXPAND);
    }

    /// Connect every handler and subscription.
    ///
    /// Separate from the constructor because each closure needs a weak handle
    /// on the finished `Rc`, which does not exist until then.
    fn wire(sliders: &Rc<Self>, show_audio: bool, show_mic: bool, show_brightness: bool) {
        let audio = sliders.services.audio.handle().clone();

        if show_audio {
            sliders.output.scale.connect_value_changed({
                let sliders = Rc::downgrade(sliders);
                let audio = audio.clone();
                move |scale| {
                    let Some(sliders) = sliders.upgrade() else {
                        return;
                    };
                    if sliders.output.updating.get() {
                        return;
                    }
                    let percent = scale.value().round().max(0.0) as u32;
                    let audio = audio.clone();
                    attempt(names::VOLUME, async move {
                        audio.set_sink_volume(percent, ChangeSource::Ui).await
                    });
                }
            });

            if let Some(button) = &sliders.output.button {
                button.connect_clicked({
                    let audio = audio.clone();
                    move |_| {
                        let audio = audio.clone();
                        attempt(names::VOLUME, async move {
                            audio.toggle_sink_muted(ChangeSource::Ui).await
                        });
                    }
                });
            }

            sliders.chooser_button.connect_clicked({
                let sliders = Rc::downgrade(sliders);
                move |_| {
                    if let Some(sliders) = sliders.upgrade() {
                        sliders.toggle_chooser();
                    }
                }
            });
        }

        if show_mic {
            sliders.microphone.scale.connect_value_changed({
                let sliders = Rc::downgrade(sliders);
                let audio = audio.clone();
                move |scale| {
                    let Some(sliders) = sliders.upgrade() else {
                        return;
                    };
                    if sliders.microphone.updating.get() {
                        return;
                    }
                    let percent = scale.value().round().max(0.0) as u32;
                    let audio = audio.clone();
                    attempt(names::MICROPHONE, async move {
                        audio.set_source_volume(percent, ChangeSource::Ui).await
                    });
                }
            });

            if let Some(button) = &sliders.microphone.button {
                button.connect_clicked({
                    let audio = audio.clone();
                    move |_| {
                        let audio = audio.clone();
                        attempt(names::MICROPHONE, async move {
                            audio.toggle_source_muted(ChangeSource::Ui).await
                        });
                    }
                });
            }
        }

        if show_brightness {
            let backlight = sliders.services.brightness.handle().clone();
            sliders.brightness.scale.connect_value_changed({
                let sliders = Rc::downgrade(sliders);
                move |scale| {
                    let Some(sliders) = sliders.upgrade() else {
                        return;
                    };
                    if sliders.brightness.updating.get() {
                        return;
                    }
                    // Safe to post per frame: the brightness service throttles
                    // writes to the backlight itself.
                    let percent = scale.value().round().max(0.0) as u32;
                    let backlight = backlight.clone();
                    attempt(names::BRIGHTNESS, async move {
                        backlight.set(percent, ChangeSource::Ui).await
                    });
                }
            });
        }

        let audio_binding = bridge::bind_state(&sliders.root, sliders.services.audio.state(), {
            let sliders = Rc::downgrade(sliders);
            move |_: &gtk4::Box, state: &AudioState| {
                if let Some(sliders) = sliders.upgrade() {
                    sliders.render_audio(state);
                }
            }
        });
        let brightness_binding =
            bridge::bind_state(&sliders.root, sliders.services.brightness.state(), {
                let sliders = Rc::downgrade(sliders);
                move |_: &gtk4::Box, state: &BrightnessState| {
                    if let Some(sliders) = sliders.upgrade() {
                        sliders.render_brightness(state);
                    }
                }
            });

        sliders
            .bindings
            .borrow_mut()
            .extend([audio_binding, brightness_binding]);
    }

    /// Open or close the output-device list.
    fn toggle_chooser(self: &Rc<Self>) {
        let open = !self.chooser_open.get();
        self.chooser_open.set(open);
        self.chooser_slot.set_expanded(open);
        set_icon(
            &self.chooser_icon,
            if open {
                "pan-up-symbolic"
            } else {
                icons::EXPAND
            },
        );
    }

    /// Draw the audio state.
    fn render_audio(&self, state: &AudioState) {
        let ceiling = model::slider_ceiling(state);
        let volume = f64::from(model::clamp_volume(state.sink_volume_pct, state));

        self.output.quietly(|| {
            self.output.set_ceiling(ceiling);
            self.output.set_value(volume);
        });
        self.output.scale.set_sensitive(state.can_set_sink_volume());
        if let Some(button) = &self.output.button {
            button.set_sensitive(state.available);
        }
        set_icon(
            &self.output.icon,
            icons::volume(state.sink_volume_pct, state.sink_muted),
        );

        let source = f64::from(state.source_volume_pct.min(state.max_volume_pct.max(1)));
        self.microphone.quietly(|| {
            self.microphone.set_ceiling(ceiling);
            self.microphone.set_value(source);
        });
        self.microphone
            .scale
            .set_sensitive(state.can_set_source_volume());
        set_icon(
            &self.microphone.icon,
            icons::microphone(state.source_volume_pct, state.source_muted),
        );
        // GNOME shows the microphone slider only while something is listening,
        // which is both less clutter and a privacy signal in its own right.
        // `always` overrides that for anyone who wants the input volume at
        // hand whether or not something is recording.
        self.mic_slot.set_expanded(match self.mic {
            MicSlider::Always => true,
            MicSlider::Auto => state.source_in_use,
            MicSlider::Never => false,
        });

        self.chooser_button.set_visible(model::wants_chooser(state));
        if !model::wants_chooser(state) && self.chooser_open.get() {
            self.collapse();
        }
        self.rebuild_devices(state);
    }

    /// Draw the brightness state.
    fn render_brightness(&self, state: &BrightnessState) {
        // A machine with no backlight has no slider, rather than a dead one.
        self.brightness.row.set_visible(state.available);
        if !state.available {
            return;
        }
        self.brightness.quietly(|| {
            self.brightness.set_ceiling(100.0);
            self.brightness.set_value(f64::from(state.percent));
        });
        self.brightness.scale.set_sensitive(true);
    }

    /// Rebuild the output list, marking the one in use.
    fn rebuild_devices(&self, state: &AudioState) {
        let devices = model::choosable_devices(&state.sinks);
        let selected = model::selected_device(devices.clone());
        let rows = self.device_rows.borrow();
        if self.chooser_list.first_child().is_some()
            && rows
                .iter()
                .map(|(id, description, _)| (id.as_str(), description.as_str()))
                .eq(devices
                    .clone()
                    .map(|device| (device.id.as_str(), device.description.as_str())))
        {
            for (index, (_, _, mark)) in rows.iter().enumerate() {
                mark.set_opacity(if selected == Some(index) { 1.0 } else { 0.0 });
            }
            return;
        }
        drop(rows);
        self.device_rows.borrow_mut().clear();
        self.device_filter.clear_rows();
        while let Some(child) = self.chooser_list.first_child() {
            self.chooser_list.remove(&child);
        }

        if devices.clone().next().is_none() {
            let empty = Label::new(Some("No output devices"));
            empty.add_css_class(classes::QS_HINT);
            empty.set_xalign(0.0);
            self.chooser_list.append(&empty);
            return;
        }

        for (index, device) in devices.enumerate() {
            let row = Button::new();
            row.add_css_class(classes::QS_DEVICE_ROW);

            let line = gtk4::Box::new(Orientation::Horizontal, ROW_SPACING);

            // Leading icon, like every other list in the panel. Without it
            // these were the only rows whose text started at the row's own
            // padding rather than 24px further in, and the chooser read as a
            // different kind of list from the three under the pills.
            let icon = Image::from_icon_name(icons::OUTPUT_DEVICE);
            icon.add_css_class(classes::QS_ICON);
            line.append(&icon);

            let name = Label::new(Some(&device.description));
            name.add_css_class(classes::QS_DEVICE_NAME);
            name.set_xalign(0.0);
            name.set_hexpand(true);
            name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            line.append(&name);

            let mark = Image::from_icon_name(icons::SELECTED);
            mark.add_css_class(classes::QS_DEVICE_MARK);
            // Reserved rather than absent, so the names do not shift when the
            // default moves from one device to another.
            mark.set_opacity(if selected == Some(index) { 1.0 } else { 0.0 });
            line.append(&mark);

            row.set_child(Some(&line));
            ripple::install(&row);
            row.connect_clicked({
                let audio = self.services.audio.handle().clone();
                let id = device.id.clone();
                move |_| {
                    let audio = audio.clone();
                    let id = id.clone();
                    attempt(
                        names::VOLUME,
                        async move { audio.set_default_sink(id).await },
                    );
                }
            });
            self.chooser_list.append(&row);
            self.device_filter
                .add(&row, device.description.clone(), None);
            self.device_rows.borrow_mut().push((
                device.id.clone(),
                device.description.clone(),
                mark,
            ));
        }
        self.device_filter.apply();
    }
}
