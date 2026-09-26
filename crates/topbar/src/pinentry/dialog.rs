//! GTK rendering for one typed pinentry request.

use std::cell::{Cell, RefCell};
use std::ffi::CStr;
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use gtk4::gdk::prelude::*;
use gtk4::glib::translate::ToGlibPtr;
use gtk4::prelude::*;
use gtk4::{Align, Application, Button, Label, Orientation, PasswordEntry, Window, gdk, glib};
use gtk4_layer_shell::{KeyboardMode, LayerShell};

use crate::anim::{Animation, AnimationParams, Easing};
use crate::pinentry::protocol::{Prompt, PromptKind, PromptResult, Secret};
use crate::style::classes;
use crate::surfaces::modal;

const WIDTH: i32 = 480;
/// The dialog's 24px CSS blur plus its 8px offset must remain inside the
/// transparent layer surface, otherwise GTK clips it into a square shadow.
const SHADOW_MARGIN: i32 = 36;

/// Present one request. The backdrop deliberately has no click controller:
/// authentication requests can only be cancelled deliberately.
pub fn present(
    _app: &Application,
    display: &gdk::Display,
    focused_output: Option<&str>,
    prompt: Prompt,
    deadline: Option<Instant>,
    response: Sender<PromptResult>,
) -> Rc<DialogState> {
    let monitor = modal::standalone_monitor(display, focused_output);
    let monitor_width = monitor
        .as_ref()
        .map(|monitor| monitor.geometry().width())
        .unwrap_or(WIDTH + 56);
    let monitor_height = monitor
        .as_ref()
        .map(|monitor| monitor.geometry().height())
        .unwrap_or(720);
    let dialog_width = WIDTH.min(monitor_width.saturating_sub(2 * SHADOW_MARGIN).max(240));
    let content_height = monitor_height.saturating_sub(128).max(120);
    let backdrop = modal::backdrop(
        monitor.as_ref(),
        "topbar-pinentry-backdrop",
        classes::PINENTRY_BACKDROP,
    );
    let window = modal::centered_window(monitor.as_ref(), "topbar-pinentry");
    window.add_css_class(classes::PINENTRY_WINDOW);
    let root = gtk4::Box::new(Orientation::Vertical, 12);
    root.set_size_request(dialog_width, -1);

    let title = Label::new(Some(&text_or(&prompt.context.title, "Authentication")));
    title.add_css_class(classes::PINENTRY_TITLE);
    title.set_xalign(0.0);
    title.set_wrap(true);
    root.append(&title);
    if !prompt.context.user_data.is_empty() {
        let context = Label::new(Some(&format!(
            "Application: {}",
            text(&prompt.context.user_data)
        )));
        context.add_css_class(classes::PINENTRY_CONTEXT);
        context.set_xalign(0.0);
        context.set_wrap(true);
        context.set_selectable(true);
        root.append(&context);
    }
    if !prompt.context.description.is_empty() {
        let description = Label::new(Some(&text(&prompt.context.description)));
        description.add_css_class(classes::PINENTRY_DESCRIPTION);
        description.set_xalign(0.0);
        description.set_wrap(true);
        // A selectable label can retain a full-text highlight after the
        // password entry takes focus, obscuring long instructions.
        root.append(&description);
    }
    if !prompt.context.error.is_empty() {
        let error = Label::new(Some(&text(&prompt.context.error)));
        error.add_css_class(classes::PINENTRY_ERROR);
        error.set_xalign(0.0);
        error.set_wrap(true);
        root.append(&error);
    }

    let password =
        matches!(prompt.kind, PromptKind::Password | PromptKind::Repeat).then(PasswordEntry::new);
    if let Some(entry) = &password {
        entry.add_css_class(classes::PINENTRY_ENTRY);
        entry.set_placeholder_text(Some(&text_or(&prompt.context.prompt, "Passphrase")));
        entry.set_show_peek_icon(true);
        root.append(entry);
        let caps = Label::new(Some("Caps Lock is on"));
        caps.add_css_class(classes::PINENTRY_CAPS_LOCK);
        caps.set_xalign(0.0);
        caps.set_visible(false);
        root.append(&caps);
        let keys = gtk4::EventControllerKey::new();
        keys.connect_key_pressed(move |_, _, _, state| {
            caps.set_visible(state.contains(gdk::ModifierType::LOCK_MASK));
            glib::Propagation::Proceed
        });
        entry.add_controller(keys);
    }

    let state = Rc::new(DialogState {
        window: window.clone(),
        backdrop: backdrop.clone(),
        password: password.clone(),
        response: RefCell::new(Some(response)),
        finished: Cell::new(false),
        container: root.clone(),
        container_motion: Animation::new(&root),
    });
    let actions = gtk4::Box::new(Orientation::Horizontal, 8);
    actions.add_css_class(classes::PINENTRY_ACTIONS);
    actions.set_halign(Align::End);
    let accept = Button::with_label(&text_or(&prompt.context.ok, "OK"));
    accept.add_css_class(classes::DIALOG_BUTTON);
    accept.add_css_class(classes::DIALOG_BUTTON_PRIMARY);
    install_interaction_feedback(&accept);
    let needs_cancel = !matches!(
        prompt.kind,
        PromptKind::Message | PromptKind::Confirmation { one_button: true }
    );
    if needs_cancel {
        let cancel = Button::with_label(&text_or(&prompt.context.cancel, "Cancel"));
        cancel.add_css_class(classes::DIALOG_BUTTON);
        install_interaction_feedback(&cancel);
        cancel.connect_clicked({
            let state = state.clone();
            let kind = prompt.kind;
            move |_| state.finish(cancel_result(kind))
        });
        actions.append(&cancel);
    }
    accept.connect_clicked({
        let state = state.clone();
        let kind = prompt.kind;
        move |_| state.accept(kind)
    });
    actions.append(&accept);
    let scroll = gtk4::ScrolledWindow::new();
    scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    scroll.set_max_content_height(content_height);
    scroll.set_propagate_natural_height(true);
    scroll.set_child(Some(&root));
    let frame = gtk4::Box::new(Orientation::Vertical, 12);
    frame.add_css_class(classes::PINENTRY_DIALOG);
    frame.set_size_request(dialog_width, -1);
    frame.set_margin_start(SHADOW_MARGIN);
    frame.set_margin_end(SHADOW_MARGIN);
    frame.set_margin_top(SHADOW_MARGIN);
    frame.set_margin_bottom(SHADOW_MARGIN);
    frame.append(&scroll);
    frame.append(&actions);
    window.set_child(Some(&frame));
    window.set_default_widget(Some(&accept));

    if let Some(entry) = &password {
        entry.connect_activate({
            let state = state.clone();
            let kind = prompt.kind;
            move |_| state.accept(kind)
        });
    }
    let keys = gtk4::EventControllerKey::new();
    // Escape must reach the dialog before a focused password entry handles it.
    keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
    keys.connect_key_pressed({
        let state = state.clone();
        let kind = prompt.kind;
        move |_, key, _, _| {
            if key == gdk::Key::Escape {
                state.finish(cancel_result(kind));
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    });
    window.add_controller(keys);
    window.connect_close_request({
        let state = state.clone();
        let kind = prompt.kind;
        move |_| {
            if !state.is_finished() {
                state.finish(cancel_result(kind));
            }
            // Let GTK complete the close after `finish` cleared the secret
            // and hid the companion backdrop.
            glib::Propagation::Proceed
        }
    });
    window.connect_notify_local(Some("is-active"), {
        let state = state.clone();
        move |window, _| {
            if !window.is_active() {
                // GTK's password entry owns its protected internal buffer;
                // clearing it on focus loss also resets any revealed value.
                if let Some(entry) = &state.password {
                    entry.set_text("");
                }
            }
        }
    });
    display.monitors().connect_items_changed({
        let state = Rc::downgrade(&state);
        let target = monitor.clone();
        move |monitors, _, _, _| {
            let target_is_present = target.as_ref().is_none_or(|target| {
                (0..monitors.n_items())
                    .filter_map(|index| monitors.item(index).and_downcast::<gdk::Monitor>())
                    .any(|candidate| candidate == *target)
            });
            if !target_is_present && let Some(state) = state.upgrade() {
                state.finish(PromptResult::Cancelled);
            }
        }
    });
    if let Some(deadline) = deadline {
        let delay = deadline.saturating_duration_since(Instant::now());
        let state = state.clone();
        glib::timeout_add_local_once(delay.max(Duration::from_millis(1)), move || {
            state.finish(PromptResult::TimedOut);
        });
    }
    backdrop.set_visible(true);
    // Keep the secret prompt focused even if its backdrop is clicked; the
    // input lock guarantees no other topbar modal competes for this seat.
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    window.present();
    state.animate_open();
    if let Some(entry) = password {
        entry.grab_focus();
    }
    state
}

pub(super) struct DialogState {
    window: Window,
    backdrop: Window,
    password: Option<PasswordEntry>,
    response: RefCell<Option<Sender<PromptResult>>>,
    finished: Cell<bool>,
    container: gtk4::Box,
    container_motion: Animation,
}

impl DialogState {
    fn accept(&self, kind: PromptKind) {
        let result = match kind {
            PromptKind::Password | PromptKind::Repeat => {
                let Some(entry) = &self.password else {
                    return self.finish(PromptResult::Failed);
                };
                // `EditableExt::text()` produces a GString allocation we
                // cannot scrub. Read GTK's borrowed internal NUL-terminated
                // storage directly and move the sole owned copy into Secret.
                let secret = Secret::new(password_bytes(entry));
                entry.set_text("");
                PromptResult::Password(secret)
            }
            PromptKind::Confirmation { .. } | PromptKind::Message => PromptResult::Accepted,
        };
        self.finish(result);
    }

    fn finish(&self, result: PromptResult) {
        if self.finished.replace(true) {
            return;
        }
        if let Some(entry) = &self.password {
            entry.set_text("");
        }
        self.window.set_visible(false);
        self.backdrop.set_visible(false);
        self.window.close();
        self.backdrop.close();
        // Both layer surfaces have received their close requests before the
        // worker can release its InputLock or emit an Assuan terminal reply.
        if let Some(response) = self.response.borrow_mut().take() {
            let _ = response.send(result);
        }
    }

    pub(super) fn cancel_for_timeout(&self) {
        self.finish(PromptResult::TimedOut);
    }

    pub(super) fn is_finished(&self) -> bool {
        self.finished.get()
    }

    fn animate_open(&self) {
        self.container.set_opacity(0.0);
        let container = self.container.clone();
        self.container_motion.start(
            AnimationParams::new(200).with_easing(Easing::EaseOutCubic),
            Box::new(move |progress| container.set_opacity(progress)),
            None,
        );
    }
}

fn install_interaction_feedback(button: &Button) {
    let animation = Animation::new(button);
    let gesture = gtk4::GestureClick::new();
    gesture.set_button(0);
    gesture.connect_pressed({
        let animation = animation.clone();
        let button = button.clone();
        move |_, _, _, _| {
            animation.start(
                AnimationParams::new(120).with_easing(Easing::EaseOutCubic),
                Box::new({
                    let button = button.clone();
                    move |progress| button.set_opacity(1.0 - 0.12 * progress)
                }),
                None,
            )
        }
    });
    gesture.connect_released({
        let animation = animation.clone();
        let button = button.clone();
        move |_, _, _, _| {
            animation.start(
                AnimationParams::new(120).with_easing(Easing::EaseOutCubic),
                Box::new({
                    let button = button.clone();
                    move |progress| button.set_opacity(0.88 + 0.12 * progress)
                }),
                None,
            )
        }
    });
    button.add_controller(gesture);
}

fn password_bytes(entry: &PasswordEntry) -> Vec<u8> {
    // SAFETY: GTK owns this pointer for the duration of the call. The entry
    // accepts no embedded NUL, which is also the Assuan pinentry convention.
    unsafe {
        let password_entry: *mut gtk4::ffi::GtkPasswordEntry = entry.to_glib_none().0;
        let editable = password_entry.cast::<gtk4::ffi::GtkEditable>();
        let pointer = gtk4::ffi::gtk_editable_get_text(editable);
        if pointer.is_null() {
            Vec::<u8>::new()
        } else {
            CStr::from_ptr(pointer).to_bytes().to_vec()
        }
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn text_or(bytes: &[u8], fallback: &str) -> String {
    if bytes.is_empty() {
        fallback.to_string()
    } else {
        text(bytes)
    }
}

fn cancel_result(kind: PromptKind) -> PromptResult {
    match kind {
        PromptKind::Confirmation { one_button: false } => PromptResult::Denied,
        PromptKind::Password
        | PromptKind::Repeat
        | PromptKind::Confirmation { one_button: true }
        | PromptKind::Message => PromptResult::Cancelled,
    }
}
