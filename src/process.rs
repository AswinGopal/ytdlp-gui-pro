//! Spawning and killing yt-dlp *process trees*.
//!
//! yt-dlp is rarely alone: section downloads and merges run ffmpeg as a
//! grandchild, and the Windows one-file build is a launcher plus a child.
//! Killing only the direct child leaves those running, so every spawn here
//! goes through `process-wrap`: a new process group on Unix, a job object on
//! Windows. `kill` then takes down the whole tree.

use std::io::{self, Read};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

#[cfg(windows)]
use process_wrap::std::{CreationFlags, JobObject};
#[cfg(unix)]
use process_wrap::std::ProcessGroup;
use process_wrap::std::{ChildWrapper, CommandWrap};

/// How often a waiting thread re-checks its `CancelToken`. This bounds the
/// latency between pressing pause / stop and the kill being issued.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// A child whose `kill` terminates its whole process tree.
pub type Child = Box<dyn ChildWrapper>;

/// Cooperative cancellation flag shared between the thread that owns a child
/// process and whoever may want it dead.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug)]
pub enum Exit {
    Done(ExitStatus),
    /// The token fired and the process tree was killed.
    Cancelled,
}

/// Spawns `command` as the leader of a new process group (Unix) or inside a
/// new job object (Windows).
pub fn spawn(command: Command) -> io::Result<Child> {
    let mut wrap = CommandWrap::from(command);
    #[cfg(unix)]
    wrap.wrap(ProcessGroup::leader());
    #[cfg(windows)]
    {
        use windows::Win32::System::Threading::CREATE_NO_WINDOW;
        // `JobObject` starts the process suspended and so replaces the
        // command's own creation flags; the no-console flag therefore has to
        // be handed to its `CreationFlags` sibling, which process-wrap merges
        // into the job's flags. process-wrap before 10.0 dropped them, which
        // gave every yt-dlp call a visible console window. Registration order
        // does not matter.
        wrap.wrap(CreationFlags(CREATE_NO_WINDOW));
        wrap.wrap(JobObject);
    }
    wrap.spawn()
}

/// Blocks until `child` exits or `token` fires. On cancellation the whole
/// process tree is killed and reaped before returning, so the caller may
/// immediately delete files the tree was writing.
pub fn wait(child: &mut Child, token: &CancelToken) -> io::Result<Exit> {
    loop {
        if token.is_cancelled() {
            kill_tree(child)?;
            return Ok(Exit::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Exit::Done(status)),
            Ok(None) => thread::sleep(POLL_INTERVAL),
            Err(e) => {
                // Don't leave a tree running that nobody is watching anymore.
                let _ = kill_tree(child);
                return Err(e);
            }
        }
    }
}

fn kill_tree(child: &mut Child) -> io::Result<()> {
    if let Err(e) = child.start_kill() {
        // The leader may have exited between the last poll and the kill
        // (the group is then already gone); that is success, not failure.
        if child.try_wait()?.is_none() {
            return Err(e);
        }
    }
    child.wait().map(drop)
}

/// Runs `command` to completion capturing stdout and stderr, killing the
/// process tree if `token` fires first (`Ok(None)`).
///
/// Both pipes are drained on their own threads: a child that fills one
/// while the parent blocks on the other would otherwise deadlock.
pub fn capture(mut command: Command, token: &CancelToken) -> io::Result<Option<Output>> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = spawn(command)?;

    let stdout = child.stdout().take().expect("stdout is piped");
    let stderr = child.stderr().take().expect("stderr is piped");
    let out_reader = thread::spawn(move || read_all(stdout));
    let err_reader = thread::spawn(move || read_all(stderr));

    let exit = wait(&mut child, token);

    // The readers see EOF once every process holding a pipe end is gone,
    // which `wait` guarantees on both the exit and the cancel path.
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();

    match exit? {
        Exit::Done(status) => Ok(Some(Output { status, stdout, stderr })),
        Exit::Cancelled => Ok(None),
    }
}

fn read_all(mut reader: impl Read) -> Vec<u8> {
    let mut buf = Vec::new();
    // A read error just ends the capture; whatever arrived is still useful.
    let _ = reader.read_to_end(&mut buf);
    buf
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Instant;

    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", script]);
        c
    }

    #[test]
    fn capture_returns_output() {
        let out = capture(sh("echo hi; echo oops >&2; exit 3"), &CancelToken::new())
            .unwrap()
            .unwrap();
        assert_eq!(out.stdout, b"hi\n");
        assert_eq!(out.stderr, b"oops\n");
        assert_eq!(out.status.code(), Some(3));
    }

    #[test]
    fn capture_survives_output_larger_than_a_pipe_buffer() {
        let out = capture(sh("head -c 1000000 /dev/zero; head -c 1000000 /dev/zero >&2"), &CancelToken::new())
            .unwrap()
            .unwrap();
        assert_eq!((out.stdout.len(), out.stderr.len()), (1_000_000, 1_000_000));
    }

    #[test]
    fn cancel_kills_grandchildren_too() {
        let marker = format!("sleep {}", 5000 + std::process::id() % 1000);
        let token = CancelToken::new();
        let t = token.clone();
        let m = marker.clone();
        let handle = thread::spawn(move || capture(sh(&format!("{m} & wait")), &t));
        thread::sleep(Duration::from_millis(400));
        let started = Instant::now();
        token.cancel();
        assert!(handle.join().unwrap().unwrap().is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
        let alive = Command::new("pgrep").args(["-f", &marker]).output().unwrap();
        assert!(alive.stdout.is_empty(), "grandchild survived the kill");
    }
}
