//! One panel at a time, and the socket the CLI reaches it through.
//!
//! Two pieces that belong together because they guard each other:
//!
//! - [`InstanceLock`] — an exclusive `flock` on `$XDG_RUNTIME_DIR/topbar.lock`,
//!   taken before anything else starts. A second panel loses it and says so
//!   instead of quietly fighting the first one for the notification name, the
//!   layer surfaces and this very socket.
//! - [`Ipc`] — a `SOCK_STREAM` listener on `$XDG_RUNTIME_DIR/topbar.sock`
//!   speaking the framed protocol in [`topbar_core::ipc`]. Because the lock is
//!   held first, a socket file already sitting on that path *cannot* belong to
//!   a live panel, so it is unlinked and rebound rather than treated as a
//!   conflict. That is the whole answer to v1's stale-socket problem, and it
//!   is an ordering rather than a heuristic.
//!
//! Requests do not get answered here. Almost everything the CLI asks for is
//! something only the GTK thread can do — raise an OSD, open a popover, hide a
//! bar — so a decoded request is forwarded with a one-shot to answer on, and
//! this crate stays free of any idea of what a widget is. The one exception is
//! the version handshake, which is about the protocol rather than the panel.

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use topbar_core::ipc::{
    HEADER_LEN, IpcRequest, IpcResponse, LOCK_NAME, MAX_FRAME_LEN, PROTOCOL_VERSION, SOCKET_NAME,
    decode_frame, encode_frame,
};
use tracing::{debug, info, warn};

/// How many requests may be waiting on the GTK thread before a client waits.
const QUEUE: usize = 32;

/// How long a client's request may sit unanswered before the panel gives up.
///
/// A stuck main thread must not leave connections accumulating; a client that
/// waited this long has an answer of its own to give.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(5);

/// The message a second panel prints before exiting.
pub const ALREADY_RUNNING: &str = "topbar is already running";

/// A decoded request, and where to send the answer.
#[derive(Debug)]
pub struct Envelope {
    /// What the client asked for.
    pub request: IpcRequest,
    /// Where the answer goes. Dropping it answers with an error.
    pub reply: oneshot::Sender<IpcResponse>,
}

impl Envelope {
    /// Answer the client.
    pub fn answer(self, response: IpcResponse) {
        let _ = self.reply.send(response);
    }
}

/// Why the lock could not be taken.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// Another panel holds it.
    #[error("{ALREADY_RUNNING}")]
    Busy,
    /// There is nowhere to put a lock file.
    #[error("XDG_RUNTIME_DIR is not set, so there is nowhere to take a lock")]
    NoRuntimeDir,
    /// The lock file could not be opened.
    #[error("could not open the lock file: {0}")]
    Io(#[from] std::io::Error),
}

/// An exclusive claim on being *the* panel.
///
/// The lock lives as long as this value, which `main` keeps until it exits.
/// The kernel releases it when the process ends however it ends, so a crashed
/// panel never leaves a lock behind for the next one to trip over.
#[derive(Debug)]
pub struct InstanceLock {
    file: std::fs::File,
    path: PathBuf,
}

/// Advisory ownership of keyboard input across panel and standalone dialogs.
///
/// A process may hold this while one modal has keyboard focus. Dropping it or
/// exiting releases the kernel lock; the file is deliberately never unlinked.
#[derive(Debug)]
pub struct InputLock {
    _file: std::fs::File,
}

impl InputLock {
    /// Attempt to claim keyboard input without waiting.
    pub fn try_acquire() -> Result<Option<Self>, LockError> {
        let dir = runtime_dir().ok_or(LockError::NoRuntimeDir)?;
        Self::try_acquire_in(&dir)
    }

    /// Attempt to claim keyboard input in a chosen runtime directory.
    pub fn try_acquire_in(dir: &Path) -> Result<Option<Self>, LockError> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(dir.join("topbar-input.lock"))?;
        // SAFETY: `file` owns a live descriptor for the duration of the call.
        let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if taken == 0 {
            return Ok(Some(Self { _file: file }));
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(None)
        } else {
            Err(LockError::Io(error))
        }
    }

    /// Wait for the current modal to release input, on a blocking worker.
    pub async fn acquire() -> Result<Self, LockError> {
        tokio::task::spawn_blocking(|| {
            let dir = runtime_dir().ok_or(LockError::NoRuntimeDir)?;
            let file = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(dir.join("topbar-input.lock"))?;
            // SAFETY: `file` owns a live descriptor until this task returns.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(LockError::Io(std::io::Error::last_os_error()));
            }
            Ok(Self { _file: file })
        })
        .await
        .map_err(|error| LockError::Io(std::io::Error::other(error)))?
    }
}

impl InstanceLock {
    /// Take the lock in `$XDG_RUNTIME_DIR`.
    pub fn acquire() -> Result<Self, LockError> {
        let dir = runtime_dir().ok_or(LockError::NoRuntimeDir)?;
        Self::acquire_in(&dir)
    }

    /// Take the lock in `dir`. The tests use this; `main` uses [`Self::acquire`].
    pub fn acquire_in(dir: &Path) -> Result<Self, LockError> {
        let path = dir.join(LOCK_NAME);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;

        // SAFETY: a valid descriptor owned by `file`, which outlives the call.
        // `flock` is per open file description, so this is released when the
        // descriptor is closed — including by the kernel, on any exit.
        let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if taken != 0 {
            let error = std::io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(libc::EWOULDBLOCK) => Err(LockError::Busy),
                _ => Err(LockError::Io(error)),
            };
        }

        debug!("holding the single-instance lock at {}", path.display());
        Ok(Self { file, path })
    }

    /// Where the lock is held.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Unlocking is implicit in closing the descriptor; the file itself is
        // left in place, because unlinking it would let a second panel create a
        // fresh one and lock that instead while the first still runs.
        let _ = self.file.sync_all();
    }
}

/// The panel's IPC listener.
#[derive(Clone)]
pub struct Ipc {
    /// The receiving end, handed to the GTK side exactly once.
    requests: Arc<Mutex<Option<mpsc::Receiver<Envelope>>>>,
}

impl Ipc {
    /// Bind the socket and start serving.
    ///
    /// Only ever called by a process holding the [`InstanceLock`], which is
    /// what makes unlinking whatever is on the path safe.
    pub(crate) fn start() -> Self {
        Self::serve(socket_path())
    }

    /// Bind at an explicit path.
    ///
    /// The panel uses [`Self::start`]; this exists so an end-to-end test can
    /// put a socket somewhere it is allowed to write and point the CLI's own
    /// client at it, rather than exercising a re-implementation of the client
    /// and calling that end to end.
    pub fn serve(path: Option<PathBuf>) -> Self {
        let (sender, receiver) = mpsc::channel(QUEUE);
        tokio::spawn(listen(sender, path));
        Self {
            requests: Arc::new(Mutex::new(Some(receiver))),
        }
    }

    /// Take the request stream. Answers `None` on every call after the first.
    pub fn take_requests(&self) -> Option<mpsc::Receiver<Envelope>> {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

/// Where the socket lives.
pub fn socket_path() -> Option<PathBuf> {
    runtime_dir().map(|dir| dir.join(SOCKET_NAME))
}

/// `$XDG_RUNTIME_DIR`, if the session set one.
fn runtime_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)
}

/// Accept connections until the panel stops.
async fn listen(sender: mpsc::Sender<Envelope>, path: Option<PathBuf>) {
    let Some(path) = path else {
        warn!("XDG_RUNTIME_DIR is not set; `topbar` commands cannot reach this panel");
        return;
    };

    // Holding the lock means nothing else is serving this path, so anything on
    // it is a leftover — including one left by a `SIGKILL`ed panel, which is
    // the case a bind-and-hope would fail on for ever.
    match std::fs::remove_file(&path) {
        Ok(()) => debug!("removed a stale socket at {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => warn!("could not clear {}: {error}", path.display()),
    }

    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(error) => {
            warn!("could not listen on {}: {error}", path.display());
            return;
        }
    };
    info!("listening for `topbar` commands on {}", path.display());

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(serve(stream, sender.clone()));
            }
            Err(error) => {
                warn!("the IPC socket stopped accepting: {error}");
                return;
            }
        }
    }
}

/// Serve one client until it hangs up.
async fn serve(mut stream: UnixStream, sender: mpsc::Sender<Envelope>) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        // Answer everything already buffered before asking for more: a client
        // may pipeline, and a partial frame must not be mistaken for a hang-up.
        while let Some(request) = next_request(&mut buffer) {
            let response = match request {
                Ok(request) => answer(request, &sender).await,
                Err(message) => IpcResponse::Error { message },
            };
            match encode_frame(&response) {
                Ok(frame) => {
                    if stream.write_all(&frame).await.is_err() {
                        return;
                    }
                }
                Err(error) => {
                    warn!("could not encode an IPC response: {error}");
                    return;
                }
            }
        }

        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    }
}

/// Pull one request off the front of `buffer`, if a whole one is there.
///
/// `Some(Err(_))` is a frame that arrived intact and made no sense, which is
/// worth telling the client about; `None` means "more bytes, please".
fn next_request(buffer: &mut Vec<u8>) -> Option<Result<IpcRequest, String>> {
    if buffer.len() >= HEADER_LEN {
        let len = u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
        if len > MAX_FRAME_LEN {
            // Nothing can resynchronise a stream whose length prefix is wrong,
            // so the buffer is dropped and the client told why.
            buffer.clear();
            return Some(Err(format!(
                "frame of {len} bytes exceeds the {MAX_FRAME_LEN}-byte maximum"
            )));
        }
    }

    match decode_frame::<IpcRequest>(buffer) {
        Ok((request, consumed)) => {
            buffer.drain(..consumed);
            Some(Ok(request))
        }
        Err(topbar_core::ipc::FrameError::Incomplete { .. }) => None,
        Err(error) => {
            buffer.clear();
            Some(Err(error.to_string()))
        }
    }
}

/// Answer one request, forwarding it to the panel when it needs one.
async fn answer(request: IpcRequest, sender: &mpsc::Sender<Envelope>) -> IpcResponse {
    // The handshake is about the protocol, not the panel, so it is answered
    // here — which also means a version check never waits on the main thread.
    if let IpcRequest::Hello { version } = request {
        if version != PROTOCOL_VERSION {
            debug!("a client speaking IPC v{version} met a panel speaking v{PROTOCOL_VERSION}");
        }
        return IpcResponse::Hello {
            version: PROTOCOL_VERSION,
        };
    }

    let (reply, answer) = oneshot::channel();
    if sender.send(Envelope { request, reply }).await.is_err() {
        return IpcResponse::Error {
            message: "the panel is shutting down".to_string(),
        };
    }

    match tokio::time::timeout(ANSWER_TIMEOUT, answer).await {
        Ok(Ok(response)) => response,
        Ok(Err(_)) => IpcResponse::Error {
            message: "the panel did not answer".to_string(),
        },
        Err(_) => IpcResponse::Error {
            message: format!("the panel did not answer within {ANSWER_TIMEOUT:?}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use topbar_core::ipc::{DumpTarget, VisibilityAction};

    /// A directory of this test's own.
    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("topbar-ipc-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a writable temp dir");
        dir
    }

    #[test]
    fn the_lock_is_taken_once_and_released_on_drop() {
        let dir = scratch("lock");
        let lock = InstanceLock::acquire_in(&dir).expect("a free lock");
        assert!(lock.path().exists());
        drop(lock);
        // Freed: the same process can take it again.
        let _again = InstanceLock::acquire_in(&dir).expect("the lock came back");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn modal_input_lock_is_nonblocking_and_released_on_drop() {
        let dir = scratch("input-lock");
        let first = InputLock::try_acquire_in(&dir)
            .unwrap()
            .expect("first modal owns input");
        assert!(InputLock::try_acquire_in(&dir).unwrap().is_none());
        drop(first);
        assert!(InputLock::try_acquire_in(&dir).unwrap().is_some());
    }

    #[test]
    fn a_missing_runtime_dir_is_its_own_failure() {
        let error = InstanceLock::acquire_in(Path::new("/nonexistent-topbar-dir"))
            .expect_err("no such directory");
        assert!(matches!(error, LockError::Io(_)));
    }

    #[test]
    fn a_second_process_is_refused() {
        let dir = scratch("second");
        let _held = InstanceLock::acquire_in(&dir).expect("a free lock");

        // `flock` is per open file description, not per process, so the second
        // claim has to come from a second process to prove anything. The test
        // binary re-runs itself at the ignored helper below.
        let exe = std::env::current_exe().expect("the test binary knows its own path");
        let output = std::process::Command::new(exe)
            .args([
                "--ignored",
                "--exact",
                "--nocapture",
                "ipc::tests::lock_helper",
            ])
            .env("TOPBAR_LOCK_DIR", &dir)
            .output()
            .expect("the helper runs");

        let printed = String::from_utf8_lossy(&output.stdout);
        assert!(
            printed.contains("busy"),
            "the second process was not refused: {printed}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The second process of [`a_second_process_is_refused`].
    #[test]
    #[ignore = "spawned by a_second_process_is_refused"]
    fn lock_helper() {
        let dir = std::env::var("TOPBAR_LOCK_DIR").expect("the parent points us at a directory");
        match InstanceLock::acquire_in(Path::new(&dir)) {
            Ok(_) => println!("acquired"),
            Err(LockError::Busy) => println!("busy: {}", LockError::Busy),
            Err(error) => println!("failed: {error}"),
        }
    }

    #[test]
    fn the_refusal_says_what_a_user_needs_to_hear() {
        assert_eq!(LockError::Busy.to_string(), ALREADY_RUNNING);
    }

    #[test]
    fn a_buffer_yields_its_frames_in_order_and_keeps_the_remainder() {
        let mut buffer = encode_frame(&IpcRequest::Reload).expect("encodes");
        buffer.extend(
            encode_frame(&IpcRequest::Bar {
                action: VisibilityAction::Toggle,
            })
            .expect("encodes"),
        );
        // A third frame, cut in half.
        let partial = encode_frame(&IpcRequest::Dump {
            target: DumpTarget::State,
            json: false,
        })
        .expect("encodes");
        buffer.extend_from_slice(&partial[..3]);

        assert_eq!(
            next_request(&mut buffer)
                .expect("a whole frame")
                .expect("valid"),
            IpcRequest::Reload
        );
        assert_eq!(
            next_request(&mut buffer)
                .expect("a whole frame")
                .expect("valid"),
            IpcRequest::Bar {
                action: VisibilityAction::Toggle
            }
        );
        assert!(
            next_request(&mut buffer).is_none(),
            "the tail is incomplete"
        );
        assert_eq!(buffer.len(), 3, "and is kept for the next read");
    }

    #[test]
    fn an_oversized_frame_is_refused_rather_than_allocated() {
        let mut buffer = (MAX_FRAME_LEN + 1).to_le_bytes().to_vec();
        buffer.extend_from_slice(b"{}");
        let error = next_request(&mut buffer)
            .expect("an answer")
            .expect_err("not a request");
        assert!(error.contains("exceeds"), "{error}");
        assert!(buffer.is_empty(), "the stream cannot be resynchronised");
    }

    #[test]
    fn a_frame_that_is_not_a_request_is_refused() {
        let mut buffer =
            encode_frame(&serde_json::json!({"request": "nonsense"})).expect("encodes");
        assert!(next_request(&mut buffer).expect("an answer").is_err());
        assert!(buffer.is_empty());
    }

    #[tokio::test]
    async fn the_handshake_is_answered_without_troubling_the_panel() {
        let (sender, mut receiver) = mpsc::channel(1);
        let response = answer(
            IpcRequest::Hello {
                version: PROTOCOL_VERSION,
            },
            &sender,
        )
        .await;
        assert_eq!(
            response,
            IpcResponse::Hello {
                version: PROTOCOL_VERSION
            }
        );
        assert!(receiver.try_recv().is_err(), "nothing was forwarded");
    }

    #[tokio::test]
    async fn a_client_speaking_another_version_still_learns_ours() {
        let (sender, _receiver) = mpsc::channel(1);
        let response = answer(IpcRequest::Hello { version: 999 }, &sender).await;
        assert_eq!(
            response,
            IpcResponse::Hello {
                version: PROTOCOL_VERSION
            },
            "the client compares the two and explains the mismatch"
        );
    }

    #[tokio::test]
    async fn a_request_reaches_the_panel_and_its_answer_comes_back() {
        let (sender, mut receiver) = mpsc::channel::<Envelope>(1);
        tokio::spawn(async move {
            let envelope = receiver.recv().await.expect("a request");
            assert_eq!(envelope.request, IpcRequest::Reload);
            envelope.answer(IpcResponse::Value {
                text: "reloaded".to_string(),
            });
        });
        assert_eq!(
            answer(IpcRequest::Reload, &sender).await,
            IpcResponse::Value {
                text: "reloaded".to_string()
            }
        );
    }

    #[tokio::test]
    async fn a_panel_that_never_answers_does_not_hold_the_client_for_ever() {
        let (sender, mut receiver) = mpsc::channel(1);
        tokio::spawn(async move {
            // Received and then dropped: the reply channel closes.
            let _envelope = receiver.recv().await;
        });
        match answer(IpcRequest::Reload, &sender).await {
            IpcResponse::Error { message } => assert!(message.contains("did not answer")),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_shut_down_panel_says_so() {
        let (sender, receiver) = mpsc::channel(1);
        drop(receiver);
        match answer(IpcRequest::Reload, &sender).await {
            IpcResponse::Error { message } => assert!(message.contains("shutting down")),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_stale_socket_file_is_replaced_rather_than_fought_over() {
        let dir = scratch("stale");
        let path = dir.join(SOCKET_NAME);
        std::fs::write(&path, b"not a socket").expect("a writable temp file");

        let (sender, _receiver) = mpsc::channel(1);
        let listening = tokio::spawn(listen(sender, Some(path.clone())));
        // The listener binds before answering anything, so a successful
        // connection is proof the leftover was cleared.
        let connected = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if UnixStream::connect(&path).await.is_ok() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            connected.is_ok(),
            "nothing ever bound to {}",
            path.display()
        );

        listening.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
