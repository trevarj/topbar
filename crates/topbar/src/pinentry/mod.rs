//! Standalone graphical pinentry entrypoint.
//!
//! It deliberately starts before panel services. The protocol worker owns
//! stdin/stdout on a background thread while GTK only receives typed prompt
//! requests on its main context.

mod dialog;
mod protocol;

use std::cell::RefCell;
use std::io::{self, BufReader, BufWriter};
use std::path::Path;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use gtk4::prelude::*;
use gtk4::{Application, gdk, gio, glib};
use topbar_core::config::Config;
use topbar_core::ipc::IpcRequest;
use topbar_services::Runtime;
use topbar_services::ipc::InputLock;

use crate::anim;
use crate::ipc_client;
use crate::style;
use crate::surfaces::modal;

use protocol::{Prompt, PromptResult, Worker};

/// Messages sent from the blocking Assuan worker to the GTK main thread.
pub(super) enum Event {
    /// Render a request after the worker acquired the advisory input lock.
    Show {
        /// Monotonic request identity used to cancel a queued prompt safely.
        generation: u64,
        /// Prompt contents.
        prompt: Box<Prompt>,
        /// Deadline including time spent acquiring the lock.
        deadline: Option<Instant>,
        /// Receives exactly one user result.
        response: Sender<PromptResult>,
    },
    /// Close a timed-out request before releasing its transferred input lock.
    Cancel {
        /// Request identity to close.
        generation: u64,
        /// Keeps other dialogs out until GTK closes this request's surfaces.
        lock: InputLock,
    },
    /// The Assuan connection ended, so GTK may leave its event loop.
    Finished,
}

type ActiveDialog = Rc<RefCell<Option<(u64, Rc<dialog::DialogState>)>>>;

/// Run `topbar pinentry` without starting panel ownership or services.
pub fn run(config_path: Option<&Path>) -> ExitCode {
    disable_core_dumps();
    let config = Config::find_and_load(config_path)
        .map(|load| load.config)
        // A malformed panel config must not block a signing operation. The
        // protocol is still served with the compiled palette in that case.
        .unwrap_or_default();
    let _ = ipc_client::request(&IpcRequest::DismissTransient);
    force_wayland_backend();
    let focused_output = modal::focused_output_connector();

    let failure: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let app = Application::builder()
        .application_id("io.github.trevarj.topbar.pinentry")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate({
        let failure = failure.clone();
        move |app| {
            let Some(display) = gdk::Display::default() else {
                *failure.borrow_mut() = Some("no Wayland display is available".to_string());
                app.quit();
                return;
            };
            if !gtk4_layer_shell::is_supported() {
                *failure.borrow_mut() =
                    Some("the compositor does not support layer-shell".to_string());
                app.quit();
                return;
            }
            if let Some(settings) = gtk4::Settings::default() {
                settings.set_gtk_icon_theme_name(Some(&config.theme.icons.theme));
            }
            anim::set_animations_enabled(config.theme.animations);
            style::apply(&config);
            start_worker(app.clone(), display, focused_output.clone());
        }
    });
    // The first request arrives from the protocol worker after activation.
    // Hold the application through that gap, when no dialog window exists yet.
    let _hold = app.hold();
    let status = app.run_with_args::<&str>(&[]);
    if failure.borrow().is_some() || status != glib::ExitCode::SUCCESS {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn start_worker(app: Application, display: gdk::Display, focused_output: Option<String>) {
    let (events, receiver) = mpsc::channel();
    let generation = AtomicU64::new(1);
    thread::Builder::new()
        .name("topbar-pinentry-assuan".to_string())
        .spawn(move || {
            let stdin = io::stdin();
            let stdout = io::stdout();
            let mut reader = BufReader::new(stdin.lock());
            let mut writer = BufWriter::new(stdout.lock());
            let mut worker = Worker::default();
            let _ = worker.serve(&mut reader, &mut writer, |prompt| {
                request_prompt(&events, &generation, prompt)
            });
            let _ = events.send(Event::Finished);
        })
        .expect("pinentry protocol worker should start");
    install_dispatcher(app, display, focused_output, receiver);
}

fn request_prompt(events: &Sender<Event>, generation: &AtomicU64, prompt: Prompt) -> PromptResult {
    let deadline = if prompt.timeout_seconds == 0 {
        None
    } else {
        Instant::now().checked_add(Duration::from_secs(prompt.timeout_seconds))
    };
    if prompt.timeout_seconds != 0 && deadline.is_none() {
        return PromptResult::Failed;
    }
    let lock = match deadline {
        Some(deadline) => Runtime::handle().block_on(async {
            tokio::time::timeout(
                deadline.saturating_duration_since(Instant::now()),
                InputLock::acquire(),
            )
            .await
            .map_err(|_| ())
            .and_then(|lock| lock.map_err(|_| ()))
        }),
        None => Runtime::handle()
            .block_on(InputLock::acquire())
            .map_err(|_| ()),
    };
    let Ok(lock) = lock else {
        return if deadline.is_some_and(|at| Instant::now() >= at) {
            PromptResult::TimedOut
        } else {
            PromptResult::Failed
        };
    };
    let generation = generation.fetch_add(1, Ordering::Relaxed);
    let (reply, answer) = mpsc::channel();
    if events
        .send(Event::Show {
            generation,
            prompt: Box::new(prompt),
            deadline,
            response: reply,
        })
        .is_err()
    {
        return PromptResult::Failed;
    }
    let result = match deadline {
        Some(deadline) => {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                PromptResult::TimedOut
            } else {
                answer
                    .recv_timeout(remaining)
                    .unwrap_or(PromptResult::TimedOut)
            }
        }
        None => answer.recv().unwrap_or(PromptResult::Failed),
    };
    if matches!(result, PromptResult::TimedOut) {
        // The queued event owns the lock while GTK is stalled. The Assuan
        // worker can report the timeout now; GTK drops the lock only after
        // it has closed any surfaces for this generation.
        let _ = events.send(Event::Cancel { generation, lock });
        return result;
    }
    drop(lock);
    result
}

fn install_dispatcher(
    app: Application,
    display: gdk::Display,
    focused_output: Option<String>,
    receiver: Receiver<Event>,
) {
    let active: ActiveDialog = Rc::new(RefCell::new(None));
    glib::timeout_add_local(Duration::from_millis(10), move || {
        loop {
            if active
                .borrow()
                .as_ref()
                .is_some_and(|(_, dialog)| dialog.is_finished())
            {
                active.borrow_mut().take();
            }
            match receiver.try_recv() {
                Ok(Event::Show {
                    generation,
                    prompt,
                    deadline,
                    response,
                }) => {
                    let live = dialog::present(
                        &app,
                        &display,
                        focused_output.as_deref(),
                        *prompt,
                        deadline,
                        response,
                    );
                    *active.borrow_mut() = Some((generation, live));
                }
                Ok(Event::Cancel {
                    generation,
                    lock: _lock,
                }) => {
                    let active_dialog = active.borrow().as_ref().and_then(|(current, dialog)| {
                        (*current == generation).then(|| dialog.clone())
                    });
                    if let Some(dialog) = active_dialog {
                        dialog.cancel_for_timeout();
                    }
                    // `_lock` drops after the matching surface has closed.
                }
                Ok(Event::Finished) | Err(mpsc::TryRecvError::Disconnected) => {
                    app.quit();
                    return glib::ControlFlow::Break;
                }
                Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
            }
        }
    });
}

/// Disable core files before a GTK or protocol object can contain a secret.
fn disable_core_dumps() {
    // SAFETY: setrlimit only affects this process and uses two plain zero values.
    unsafe {
        let limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        let _ = libc::setrlimit(libc::RLIMIT_CORE, &limit);
    }
}

/// Layer-shell is Wayland-only, and this runs before GTK initialises.
fn force_wayland_backend() {
    if std::env::var_os("GDK_BACKEND").is_none() {
        // SAFETY: this is before GTK and its worker threads are created.
        unsafe { std::env::set_var("GDK_BACKEND", "wayland") };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_timeout_keeps_input_owned_until_gtk_consumes_it() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "topbar-pinentry-input-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir(&dir).expect("make isolated runtime directory");
        let lock = InputLock::try_acquire_in(&dir)
            .expect("open input lock")
            .expect("claim input lock");
        let (send, receive) = mpsc::channel();
        assert!(
            send.send(Event::Cancel {
                generation: 1,
                lock
            })
            .is_ok()
        );
        assert!(
            InputLock::try_acquire_in(&dir)
                .expect("check queued input lock")
                .is_none()
        );
        drop(receive.recv().expect("receive queued cancellation"));
        assert!(
            InputLock::try_acquire_in(&dir)
                .expect("claim input after cleanup")
                .is_some()
        );
        std::fs::remove_dir_all(dir).expect("remove isolated runtime directory");
    }
}
