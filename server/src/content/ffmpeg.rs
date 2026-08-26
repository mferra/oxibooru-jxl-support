//! Running the bundled `FFmpeg` binary as a child process.
//!
//! Everything here exists because `FFmpeg` is a hostile subprocess: it has no timeout of its
//! own, it blocks forever once an undrained pipe fills, and a child that is never waited on
//! stays in the process table as a zombie for the lifetime of the server.

use crate::api::error::{ApiError, ApiResult};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use tracing::error;

pub const PATH: &str = "/opt/app/ffmpeg";

/// Cap on the `FFmpeg` log lines kept per run, so a file that warns on every frame can't grow
/// an unbounded error message. The tail is kept rather than the head, since that is where both
/// a fatal error and the output stream description show up.
pub const MAX_CAPTURED_LOG_LINES: usize = 64;

/// Reads a timeout in seconds from the `variable` environment variable, falling back to
/// `default_seconds`.
///
/// Every invocation is given a limit; the ceiling differs by an order of magnitude between
/// probing a file and re-encoding it, hence the parameter.
pub fn env_timeout(variable: &str, default_seconds: u64) -> Duration {
    let seconds = std::env::var(variable)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|&seconds| seconds > 0)
        .unwrap_or(default_seconds);
    Duration::from_secs(seconds)
}

/// What a successful `FFmpeg` run produced.
pub struct Output {
    /// Bytes written to stdout. Empty unless the command directed output there with `-`.
    pub stdout: Vec<u8>,
    /// The tail of stderr, for diagnostics and for scraping details `FFmpeg` reports nowhere
    /// else, such as the geometry of a raw frame.
    pub log_tail: Vec<String>,
}

/// Runs the bundled `FFmpeg` with `args`, returning what it wrote to stdout.
///
/// Both pipes are drained by worker threads for the whole run. That is not an optimization:
/// `FFmpeg` blocks as soon as a pipe buffer fills, and a single raw video frame is an order of
/// magnitude larger than the buffer, so an undrained stdout wedges the process permanently.
///
/// The process is never allowed to outlive `timeout` and is always waited on, so it can
/// neither block the caller indefinitely nor linger as a zombie. A non-zero exit is an error,
/// reported together with the captured log.
///
/// `description` is a participle phrase naming the work ("extracting a frame from ...").
pub fn run(description: &str, args: &[&str], timeout: Duration) -> ApiResult<Output> {
    let mut child = Command::new(PATH)
        .arg("-hide_banner")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| ApiError::FfmpegError(Box::new(err)))?;

    let mut stdout = child.stdout.take();
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(stdout) = stdout.as_mut() {
            let _ = stdout.read_to_end(&mut bytes);
        }
        bytes
    });

    let stderr = child.stderr.take();
    let stderr_reader = std::thread::spawn(move || {
        let mut log_tail = VecDeque::new();
        if let Some(stderr) = stderr {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if log_tail.len() == MAX_CAPTURED_LOG_LINES {
                    log_tail.pop_front();
                }
                log_tail.push_back(line);
            }
        }
        log_tail
    });

    // Killing the child closes both pipes, so neither reader can outlive the wait below.
    let status = wait_with_timeout(&mut child, description, timeout);
    let stdout = stdout_reader.join().unwrap_or_default();
    let log_tail: Vec<String> = stderr_reader.join().unwrap_or_default().into();

    match status {
        Some(status) if status.success() => Ok(Output { stdout, log_tail }),
        Some(status) => Err(ApiError::FfmpegError(
            format!("FFmpeg exited with {status} while {description}. {}", summarize(&log_tail)).into(),
        )),
        None => Err(ApiError::FfmpegError(
            format!(
                "FFmpeg was killed after failing to finish within {}s while {description}. {}",
                timeout.as_secs(),
                summarize(&log_tail)
            )
            .into(),
        )),
    }
}

/// Renders a captured log for inclusion in an error message.
pub fn summarize(log_tail: &[String]) -> String {
    if log_tail.is_empty() {
        "FFmpeg logged nothing.".to_owned()
    } else {
        format!("FFmpeg output: {}", log_tail.join("; "))
    }
}

/// Waits for `child` to exit, killing it if it outlives `timeout`. Returns [`None`] if it had
/// to be killed.
///
/// The process is always waited on, which is what removes it from the process table; a child
/// that is never reaped lingers as a zombie even after it has exited on its own.
fn wait_with_timeout(child: &mut Child, description: &str, timeout: Duration) -> Option<ExitStatus> {
    const POLL_INTERVAL: Duration = Duration::from_millis(50);

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            Ok(None) => {
                error!("FFmpeg timed out after {}s while {description}; killing it", timeout.as_secs());
                break;
            }
            Err(err) => {
                error!("Cannot wait on FFmpeg process while {description}: {err}; killing it");
                break;
            }
        }
    }

    if let Err(err) = child.kill() {
        error!("Cannot kill stuck FFmpeg process while {description}: {err}");
    }
    if let Err(err) = child.wait() {
        error!("Cannot reap FFmpeg process while {description}: {err}");
    }
    None
}
