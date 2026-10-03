//! The one place the panel starts a process.
//!
//! Everything the user can put a shell command in — `on_click`,
//! `on_click_right`, `on_click_middle`, and from M10 the custom-\* widgets —
//! comes through here, for two reasons.
//!
//! **Nothing is left behind.** A child that is spawned and forgotten becomes a
//! zombie the moment it exits, because nobody waited on it; a panel that runs
//! a click command sixty times an hour would accumulate sixty entries in the
//! process table. Every child started here is handed to a task whose only job
//! is to wait on it, so it is reaped whenever it ends, however it ends.
//!
//! **A command that fails says so.** `sh -c` starts successfully whatever
//! nonsense it is given, and reports the truth in an exit status a moment
//! later — 127 for a command that does not exist. So the runner gives the
//! child [`GRACE`] to fall over: if it does, the caller gets an error it can
//! show, and if it is still running it is detached and left alone, because a
//! click that opened a terminal has succeeded and must not be waited on.
//!
//! ## Two shapes, and why
//!
//! [`run`] is fire-and-forget: a click command whose *output* nobody wants.
//! [`capture`] waits for a program to finish and reads what it printed, which
//! is what the updates service does to every package manager it asks. They are
//! separate functions because the safety rules differ — a captured command has
//! a hard timeout and a hard output cap, because `dnf check-update` on a slow
//! mirror is a minute of nothing and `apt-get -s upgrade` on a neglected
//! machine is a megabyte of text, and neither may be allowed to sit in the
//! panel's memory or its runtime for ever.
//!
//! [`CmdSpec::argv`] takes an **argument vector**, not a string: nothing the
//! panel deduces for itself goes near a shell, so a mount point or a package
//! name with a space in it cannot become two arguments. The one exception is
//! [`CmdSpec::shell`], for the user's own `update_count_command` — that key has
//! always been a shell command line, pipes and all, and turning it into an
//! argv would break every configuration that has one.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::{debug, warn};

use crate::error::SvcError;

/// How long a command is watched before it is assumed to have started.
///
/// Long enough for a shell to report "command not found", short enough that a
/// click never feels like it hung. Everything slower than this is a program
/// that is running, which is what the user asked for.
const GRACE: Duration = Duration::from_millis(250);

/// How much of a failing command's stderr is quoted back.
const STDERR_CAP: usize = 200;

/// How long a captured command may run before it is killed.
///
/// Generous, because `dnf check-update` against a cold mirror really does take
/// twenty seconds — but bounded, because the alternative is a package manager
/// waiting on a lock file holding a task open until the session ends.
pub const CAPTURE_TIMEOUT: Duration = Duration::from_secs(45);

/// How much of a captured command's output is kept.
///
/// `apt-get -s upgrade` on a machine that has not been updated in a year runs
/// to hundreds of kilobytes. The count is on the first lines; the rest is
/// dropped rather than carried around in a snapshot.
pub const CAPTURE_CAP: usize = 256 * 1024;

/// One program to run and read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdSpec {
    /// The program and its arguments. `argv[0]` is looked up on `PATH`.
    pub argv: Vec<String>,
    /// How long it may run.
    pub timeout: Duration,
    /// How much of its standard output is kept.
    pub max_output_bytes: usize,
}

impl CmdSpec {
    /// A program run directly, with no shell between.
    ///
    /// The form everything the panel decides for itself uses.
    pub fn argv<I, S>(argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            argv: argv.into_iter().map(Into::into).collect(),
            timeout: CAPTURE_TIMEOUT,
            max_output_bytes: CAPTURE_CAP,
        }
    }

    /// A command line run through `sh -c`.
    ///
    /// **The documented exception.** `[updates] update_count_command` has been
    /// a shell command line since v1 — the live configuration's own value is a
    /// pipeline — and rewriting it as an argument vector would break every
    /// configuration that has one. It is the user's own string, from the user's
    /// own file, and it is the only caller.
    pub fn shell(command: &str) -> Self {
        Self::argv(["sh", "-c", command])
    }

    /// The same, with a different ceiling on how long it may take.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// What to call this in a message.
    fn label(&self) -> String {
        self.argv.join(" ")
    }
}

/// What a captured command left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    /// Its exit status, or `None` if a signal ended it.
    pub code: Option<i32>,
    /// Standard output, capped at [`CmdSpec::max_output_bytes`].
    pub stdout: String,
    /// Standard error, capped the same way.
    pub stderr: String,
}

impl Captured {
    /// Whether the command exited zero.
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

/// Run `spec` to completion and read what it printed.
///
/// An error means the program could not be started or did not finish in time;
/// a program that ran and exited non-zero is a [`Captured`] with the status in
/// it, because "non-zero" is a *result* for several of the callers here —
/// `dnf check-update` says "there are updates" with exit 100.
pub async fn capture(spec: &CmdSpec) -> Result<Captured, SvcError> {
    let Some((program, arguments)) = spec.argv.split_first() else {
        return Err(SvcError::Command {
            command: String::new(),
            reason: "the command is empty".to_string(),
        });
    };

    debug!("capturing `{}`", spec.label());
    let mut child = Command::new(program)
        .args(arguments)
        // No stdin, for the same reason `run` gives it none: a package manager
        // that asks a question nobody can see would hold the task until it
        // timed out.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The child dies with the task rather than outliving the panel: a
        // service that is torn down mid-check must not leave `apt-get` behind.
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| SvcError::Command {
            command: spec.label(),
            reason: error.to_string(),
        })?;

    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let cap = spec.max_output_bytes;

    // The pipes are drained *while* the child runs. Waiting first and reading
    // afterwards deadlocks the moment a command prints more than a pipe buffer.
    let collect = async {
        let (stdout, stderr, status) = tokio::join!(
            read_capped(&mut out, cap),
            read_capped(&mut err, cap),
            child.wait(),
        );
        status.map(|status| (status, stdout, stderr))
    };

    match tokio::time::timeout(spec.timeout, collect).await {
        Ok(Ok((status, stdout, stderr))) => Ok(Captured {
            code: status.code(),
            stdout,
            stderr,
        }),
        Ok(Err(error)) => Err(SvcError::Command {
            command: spec.label(),
            reason: error.to_string(),
        }),
        Err(_) => {
            // Kill and wait explicitly: a bounded effect leaves no child behind.
            let _ = child.kill().await;
            Err(SvcError::Command {
                command: spec.label(),
                reason: format!("timed out after {:?}", spec.timeout),
            })
        }
    }
}

/// Read a pipe to its end, or to `cap` bytes, whichever comes first.
async fn read_capped<R>(pipe: &mut Option<R>, cap: usize) -> String
where
    R: tokio::io::AsyncRead + Unpin,
{
    match pipe.as_mut() {
        Some(pipe) => read_stream(pipe, cap).await,
        None => String::new(),
    }
}

/// The same, for a stream that is definitely there.
async fn read_stream(reader: &mut (impl tokio::io::AsyncRead + Unpin), cap: usize) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let room = cap.saturating_sub(buffer.len());
                if room == 0 {
                    // Keep draining so the child is never blocked on a full
                    // pipe; simply stop keeping what comes out.
                    continue;
                }
                buffer.extend_from_slice(&chunk[..read.min(room)]);
            }
        }
    }
    String::from_utf8_lossy(&buffer).into_owned()
}

/// Run `command` through a shell, reaping it whatever it does.
///
/// Returns as soon as the command has either failed or been running for
/// [`GRACE`]. An error carries the reason a row can show; the log line carries
/// the rest.
pub async fn run(command: &str) -> Result<(), SvcError> {
    if command.trim().is_empty() {
        return Err(SvcError::Command {
            command: command.to_string(),
            reason: "the command is empty".to_string(),
        });
    }

    run_argv(&["sh".to_string(), "-c".to_string(), command.to_string()]).await
}

/// Start an argument vector without a shell, and reap the process on exit.
pub async fn run_argv(argv: &[String]) -> Result<(), SvcError> {
    let Some((program, arguments)) = argv.split_first() else {
        return Err(SvcError::Command {
            command: String::new(),
            reason: "the command is empty".to_string(),
        });
    };
    let name = program.clone();
    debug!("starting `{name}`");
    let mut child = Command::new(program)
        .args(arguments)
        // No stdin: a command that asks a question the user cannot see would
        // hang for ever holding a reaper task open.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| SvcError::Command {
            command: name.clone(),
            reason: error.to_string(),
        })?;

    match tokio::time::timeout(GRACE, child.wait()).await {
        // It fell over inside the grace period: that is a failure the user
        // asked for feedback on.
        Ok(Ok(status)) if !status.success() => {
            let detail = stderr(&mut child).await;
            Err(SvcError::Command {
                command: name.clone(),
                reason: describe(status.code(), &detail),
            })
        }
        // It finished, successfully, before the grace period was up.
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(SvcError::Command {
            command: name.clone(),
            reason: error.to_string(),
        }),
        // Still running, which is the normal case: hand it to a reaper and
        // stop caring.
        Err(_) => {
            tokio::spawn(async move {
                match child.wait().await {
                    Ok(status) if !status.success() => {
                        warn!("`{name}` ended with {status}");
                    }
                    Ok(_) => debug!("`{name}` finished"),
                    Err(error) => warn!("could not wait for `{name}`: {error}"),
                }
            });
            Ok(())
        }
    }
}

/// Whatever the child managed to say before it died, capped.
async fn stderr(child: &mut tokio::process::Child) -> String {
    use tokio::io::AsyncReadExt;

    let Some(mut pipe) = child.stderr.take() else {
        return String::new();
    };
    let mut buffer = Vec::new();
    let _ = pipe.read_to_end(&mut buffer).await;
    let text = String::from_utf8_lossy(&buffer);
    let trimmed = text.trim();
    if trimmed.chars().count() <= STDERR_CAP {
        return trimmed.to_string();
    }
    trimmed.chars().take(STDERR_CAP).collect::<String>() + "…"
}

/// Turn an exit status and its complaint into one line.
fn describe(code: Option<i32>, detail: &str) -> String {
    let status = match code {
        // What a shell says when the program is not on `PATH`.
        Some(127) => "command not found".to_string(),
        Some(126) => "command not executable".to_string(),
        Some(code) => format!("exited with status {code}"),
        None => "killed by a signal".to_string(),
    };
    if detail.is_empty() {
        status
    } else {
        format!("{status}: {detail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_program_is_named_as_one() {
        assert_eq!(describe(Some(127), ""), "command not found");
        assert_eq!(
            describe(Some(127), "sh: nope: not found"),
            "command not found: sh: nope: not found"
        );
        assert_eq!(describe(Some(2), ""), "exited with status 2");
        assert_eq!(describe(None, ""), "killed by a signal");
        assert_eq!(describe(Some(126), ""), "command not executable");
    }

    #[tokio::test]
    async fn an_empty_command_is_refused_rather_than_run() {
        let error = run("   ").await.expect_err("nothing to run");
        assert_eq!(error.user_message(), "That command could not be run");
    }

    #[tokio::test]
    async fn a_command_that_works_reports_success() {
        run("true").await.expect("`true` is true");
    }

    #[tokio::test]
    async fn a_command_that_does_not_exist_is_caught_within_the_grace_period() {
        let error = run("topbar-no-such-program-exists")
            .await
            .expect_err("there is no such program");
        assert!(
            error.to_string().contains("not found"),
            "unhelpful message: {error}"
        );
        assert_eq!(error.user_message(), "That command could not be run");
    }

    #[tokio::test]
    async fn a_command_that_fails_quickly_says_what_it_said() {
        let error = run("echo trouble >&2; exit 3")
            .await
            .expect_err("exit 3 is a failure");
        let text = error.to_string();
        assert!(text.contains("status 3"), "{text}");
        assert!(text.contains("trouble"), "{text}");
    }

    #[tokio::test]
    async fn a_long_running_command_is_detached_rather_than_waited_on() {
        let started = std::time::Instant::now();
        run("sleep 30").await.expect("it started");
        assert!(
            started.elapsed() < GRACE * 4,
            "a click that opens an application must not block on it"
        );
    }

    #[tokio::test]
    async fn a_detached_command_is_still_reaped() {
        // Short enough to finish while the test is still running, long enough
        // to outlive the grace period — so it takes the reaper path rather
        // than being waited on inline.
        run("sleep 0.4").await.expect("it started");
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert_eq!(
            zombie_children(),
            0,
            "a click command must not leave a defunct process behind"
        );
    }

    #[tokio::test]
    async fn a_captured_command_hands_back_what_it_printed() {
        let captured = capture(&CmdSpec::argv(["echo", "7"]))
            .await
            .expect("echo runs");
        assert!(captured.ok());
        assert_eq!(captured.stdout.trim(), "7");
        assert!(captured.stderr.is_empty());
    }

    #[tokio::test]
    async fn a_non_zero_exit_is_a_result_rather_than_an_error() {
        // `dnf check-update` says "there are updates" with exit 100, so the
        // runner has to hand the status back rather than throwing it away.
        let captured = capture(&CmdSpec::shell("echo hi; exit 100"))
            .await
            .expect("the shell ran");
        assert_eq!(captured.code, Some(100));
        assert_eq!(captured.stdout.trim(), "hi");
    }

    #[tokio::test]
    async fn a_program_that_is_not_installed_is_an_error_not_a_status() {
        // `checkupdates` on a machine without pacman-contrib: the updates
        // service reads this as "no way to count here", which is different
        // from "zero updates".
        let error = capture(&CmdSpec::argv(["topbar-no-such-program-exists"]))
            .await
            .expect_err("there is no such program");
        assert!(matches!(error, SvcError::Command { .. }));
    }

    #[tokio::test]
    async fn direct_launch_preserves_argument_boundaries() {
        let result = run_argv(&[
            "sh".into(),
            "-c".into(),
            "test \"$1\" = 'two words'".into(),
            "sh".into(),
            "two words".into(),
        ])
        .await;
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn an_empty_argv_is_refused_rather_than_run() {
        let error = capture(&CmdSpec::argv(Vec::<String>::new()))
            .await
            .expect_err("nothing to run");
        assert!(matches!(error, SvcError::Command { .. }));
    }

    #[tokio::test]
    async fn a_command_that_never_finishes_is_killed_at_the_timeout() {
        let spec = CmdSpec::argv(["sleep", "30"]).with_timeout(Duration::from_millis(300));
        let started = std::time::Instant::now();
        let error = capture(&spec).await.expect_err("it should time out");
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the timeout did not fire"
        );
    }

    #[tokio::test]
    async fn output_past_the_cap_is_dropped_rather_than_kept() {
        // The child keeps printing well past the cap; the point is that it
        // finishes at all — a runner that stopped reading would wedge it on a
        // full pipe — and that only the cap is retained.
        let spec = CmdSpec {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "i=0; while [ $i -lt 400 ]; do echo 0123456789012345678901234567890123456789012345678901234567890123; i=$((i+1)); done".into(),
            ],
            timeout: Duration::from_secs(10),
            max_output_bytes: 512,
        };
        let captured = capture(&spec).await.expect("it ran");
        assert!(captured.ok(), "the child finished rather than blocking");
        assert!(
            captured.stdout.len() <= 512,
            "kept {} bytes",
            captured.stdout.len()
        );
    }

    #[test]
    fn the_users_own_command_line_is_the_only_thing_that_reaches_a_shell() {
        assert_eq!(
            CmdSpec::shell("pacman -Qu | wc -l").argv,
            ["sh", "-c", "pacman -Qu | wc -l"]
        );
        // Everything the panel decides for itself is an argument vector, so a
        // path with a space in it stays one argument.
        assert_eq!(
            CmdSpec::argv(["dnf", "-q", "check-update"]).argv,
            ["dnf", "-q", "check-update"]
        );
    }

    /// How many of this process's children the kernel is still holding.
    ///
    /// Reads `/proc` directly rather than shelling out to `ps`, so the test
    /// does not depend on a tool being installed to prove a tool is reaped.
    fn zombie_children() -> usize {
        let ours = std::process::id();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return 0;
        };
        entries
            .flatten()
            .filter(|entry| {
                let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else {
                    return false;
                };
                let zombie = status
                    .lines()
                    .any(|line| line.starts_with("State:") && line.contains('Z'));
                let child = status
                    .lines()
                    .find_map(|line| line.strip_prefix("PPid:"))
                    .and_then(|pid| pid.trim().parse::<u32>().ok())
                    == Some(ours);
                zombie && child
            })
            .count()
    }
}
