use std::ffi::OsString;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread;

use regex::Regex;

use crate::process::{self, CancelToken, Exit};
use crate::types::{DlOpts, ExpectedStream, ProgressInfo, Result};

pub type ProgressCb = Option<Box<dyn Fn(ProgressInfo) + Send + Sync + 'static>>;

/// How a `run` ended when it did not fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    Completed,
    /// The `CancelToken` fired and the process tree was killed. Not an error,
    /// and the caller must not treat it as a finished download.
    Cancelled,
}

/// The file a download would write is already on disk.
///
/// yt-dlp skips an existing file and still exits 0, which would be recorded
/// as a finished download that never happened. The queue therefore asks
/// `planned_path` first and refuses with this error, which it recognises by
/// type so that the row can be unlocked for changes (nothing was fetched).
#[derive(Debug)]
pub struct OutputExists(pub PathBuf);

impl fmt::Display for OutputExists {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = self.0.file_name().map_or_else(|| self.0.to_string_lossy(), |n| n.to_string_lossy());
        write!(
            f,
            "File already exists: {name}. Choose a different quality or range, or another folder, then retry."
        )
    }
}

impl std::error::Error for OutputExists {}

/// Marker that prefixes every structured progress line yt-dlp prints for us,
/// so it can never be confused with ordinary log output.
const PROGRESS_MARKER: &str = "ytdlp-gui-progress";

/// yt-dlp prints this once per progress tick. Raw numbers (rather than the
/// human-formatted `[download]  12.3% of …` line) keep sizes and speed free of
/// unit parsing; a field yt-dlp does not know prints as `NA`. The format ID
/// comes last, being the one field that may hold a space: it names the stream
/// the numbers belong to, which a download of video and audio needs to tell
/// apart.
const PROGRESS_TEMPLATE: &str = "download:ytdlp-gui-progress \
    %(progress.downloaded_bytes)s %(progress.total_bytes)s \
    %(progress.total_bytes_estimate)s %(progress.speed)s %(info.format_id)s";

/// ` [1080p]`: the height of the format yt-dlp finally selects or merges.
/// Written in yt-dlp's replacement syntax (`&…|`), so a stream without a
/// height (audio-only) leaves nothing behind instead of printing `NA`.
const HEIGHT_TAG: &str = "%(height& [{}p]|)s";

/// Longest line the output readers keep. Anything beyond is dropped, so a
/// pathological line with no newline cannot grow memory without bound.
const MAX_LINE_BYTES: usize = 64 * 1024;

/// `-x` / `--extract-audio`, as the audio presets pass them.
fn extracts_audio(opts: &DlOpts) -> bool {
    opts.extra.iter().any(|a| a == "-x" || a == "--extract-audio")
}

/// The extension audio extraction leaves the file with: the `--audio-format`
/// the presets spell out. `None` when nothing is extracted, or when the
/// target is yt-dlp's own pick (`best`), whose extension is not known upfront.
fn extracted_audio_ext(opts: &DlOpts) -> Option<&str> {
    if !extracts_audio(opts) {
        return None;
    }
    let at = opts.extra.iter().position(|a| a == "--audio-format")?;
    opts.extra.get(at + 1).map(String::as_str).filter(|format| *format != "best")
}

/// The output file name template (without a directory — `-P` supplies that).
///
/// Every video name carries the resolution it holds (`Title [1080p].mp4`), so
/// the same video fetched at two resolutions lands in one folder as two files
/// instead of the second being skipped. Audio carries no tag: an extracted
/// track says nothing about the height of the stream it came from.
fn output_template(opts: &DlOpts) -> String {
    let tag = if extracts_audio(opts) { "" } else { HEIGHT_TAG };
    match &opts.output_name {
        Some(name) => format!("{name}{tag}.%(ext)s"),
        None if opts.sections.is_some() => {
            format!("%(title)s-%(section_start)s-%(section_end)s{tag}.%(ext)s")
        }
        None => format!("%(title)s{tag}.%(ext)s"),
    }
}

/// The arguments that decide *which file comes out*: destination, name, range
/// and format selection. The download and `planned_path` share this one list,
/// so the name the pre-flight reports is the name the download writes.
fn target_args(opts: &DlOpts, template: &str) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();

    // `home:` / `temp:` are spelled out so a Windows drive letter can never be
    // mistaken for a path-type prefix. OsString keeps non-UTF-8 paths intact.
    let mut home = OsString::from("home:");
    home.push(&opts.dest_dir);
    let mut temp = OsString::from("temp:");
    temp.push(&opts.temp_dir);
    args.extend(["-P".into(), home, "-P".into(), temp, "-o".into(), template.into()]);

    if let Some(sec) = &opts.sections {
        args.extend(["--download-sections".into(), sec.into()]);
    }
    if let Some(fmt) = &opts.format {
        args.extend(["-f".into(), fmt.into()]);
    }
    args.extend(opts.extra.iter().map(OsString::from));
    // On Windows the legacy MAX_PATH limit (260 chars) can be hit when a long
    // download directory is combined with a long video title.  Trim filenames
    // to 200 characters — conservative enough to keep the full path under the
    // limit for all but the most deeply-nested download directories.
    #[cfg(windows)]
    args.extend(["--trim-filenames".into(), "200".into()]);
    args
}

pub fn deno_in_path() -> bool {
    let target = if cfg!(windows) { "deno.exe" } else { "deno" };
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths)
                .any(|dir| dir.join(target).is_file())
        })
        .unwrap_or(false)
}

/// Parses a timestamp string into total seconds.
///
/// Accepts both `HH:MM:SS[.ss]` (produced by `seconds_to_hms` for chapters)
/// and `MM:SS[.ss]`. A bare `SS[.ss]` token is also tolerated.
fn hms_to_secs(s: &str) -> Option<f64> {
    let parts: Vec<&str> = s.splitn(3, ':').collect();
    match parts.as_slice() {
        [h, m, sec] => {
            let h: f64 = h.parse().ok()?;
            let m: f64 = m.parse().ok()?;
            let sec: f64 = sec.parse().ok()?;
            Some(h * 3600.0 + m * 60.0 + sec)
        }
        [m, sec] => {
            let m: f64 = m.parse().ok()?;
            let sec: f64 = sec.parse().ok()?;
            Some(m * 60.0 + sec)
        }
        [sec] => {
            let sec: f64 = sec.parse().ok()?;
            Some(sec)
        }
        _ => None,
    }
}

/// Returns the duration in seconds implied by a `--download-sections` specifier
/// of the form `*<start>-<end>` where each timestamp is `HH:MM:SS` or `MM:SS`.
fn parse_section_secs(sections: &str) -> Option<f64> {
    let s = sections.trim_start_matches('*');
    let (start_str, end_str) = s.split_once('-')?;
    let duration = hms_to_secs(end_str)? - hms_to_secs(start_str)?;
    Some(duration.max(0.0))
}

fn finite_non_negative(token: Option<&str>) -> Option<f64> {
    token
        .and_then(|t| t.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
}

/// One parsed progress line.
#[derive(Debug, PartialEq)]
struct Tick {
    /// Progress of the file being fetched.
    info: ProgressInfo,
    /// The `format_id` of the stream `info` belongs to, when the line says.
    stream: Option<String>,
    /// yt-dlp found the stream's file already complete on disk (a resumed
    /// run) and fetched nothing: it reports the size and no byte count.
    /// `info` then reads as all of it downloaded.
    already_complete: bool,
}

impl Tick {
    /// A tick that cannot say which stream it belongs to.
    fn of(info: ProgressInfo) -> Self {
        Self { info, stream: None, already_complete: false }
    }
}

/// Splits the first whitespace-delimited field off `s`.
fn split_field(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    s.split_at(s.find(char::is_whitespace).unwrap_or(s.len()))
}

/// Parses the fields of a `PROGRESS_TEMPLATE` line (marker already stripped).
fn parse_template_line(rest: &str) -> Option<Tick> {
    let (downloaded, rest) = split_field(rest);
    let (total, rest) = split_field(rest);
    let (estimate, rest) = split_field(rest);
    let (speed, rest) = split_field(rest);
    // What is left is the format ID, kept whole so one holding a space survives.
    let stream = Some(rest.trim())
        .filter(|id| !id.is_empty() && *id != "NA")
        .map(str::to_owned);

    let downloaded = finite_non_negative(Some(downloaded));
    let total = finite_non_negative(Some(total)).filter(|t| *t > 0.0);
    let estimate = finite_non_negative(Some(estimate)).filter(|t| *t > 0.0);
    let speed = finite_non_negative(Some(speed));

    let (total, total_estimated) = match (total, estimate) {
        (Some(t), _) => (Some(t), false),
        (None, Some(e)) => (Some(e), true),
        (None, None) => (None, false),
    };
    // A stream that is already complete on disk reports its size and no
    // byte count (a resumed run skips it).
    let already_complete =
        downloaded.is_none() && total.is_some() && !total_estimated && stream.is_some();
    let downloaded = if already_complete { total } else { downloaded };
    downloaded?;

    let mut pct = match (downloaded, total) {
        (Some(d), Some(t)) => (d / t * 100.0).clamp(0.0, 100.0),
        _ => 0.0,
    };
    if total_estimated {
        // An estimate can run below the real size; never claim "done" from it.
        pct = pct.min(99.0);
    }
    Some(Tick {
        info: ProgressInfo {
            pct,
            speed,
            downloaded: downloaded.map(|d| d as u64),
            total: total.map(|t| t as u64),
            total_estimated,
        },
        stream,
        already_complete,
    })
}

/// Parses one ffmpeg `-stats` line (`size=… time=HH:MM:SS.ss …`). ffmpeg
/// knows neither the output's final size nor a download speed, so the total
/// is extrapolated from how much of the clip has been written so far.
fn parse_ffmpeg_line(line: &str, section_secs: f64) -> Option<ProgressInfo> {
    static TIME_RE: OnceLock<Regex> = OnceLock::new();
    static SIZE_RE: OnceLock<Regex> = OnceLock::new();

    if section_secs <= 0.0 {
        return None;
    }
    let time_re = TIME_RE.get_or_init(|| {
        Regex::new(r"\btime=(\d+):(\d+):([\d.]+)").expect("TIME_RE is valid")
    });
    let caps = time_re.captures(line)?;
    let h: f64 = caps[1].parse().ok()?;
    let m: f64 = caps[2].parse().ok()?;
    let s: f64 = caps[3].parse().ok()?;
    let fraction = ((h * 3600.0 + m * 60.0 + s) / section_secs).clamp(0.0, 1.0);

    // ffmpeg's `kB` / `KiB` are both 1024-based.
    let size_re = SIZE_RE.get_or_init(|| {
        Regex::new(r"\bsize=\s*(\d+(?:\.\d+)?)\s*([kKmMgG]?)i?B").expect("SIZE_RE is valid")
    });
    let downloaded = size_re.captures(line).and_then(|c| {
        let n: f64 = c[1].parse().ok()?;
        let unit = match &c[2] {
            "k" | "K" => 1024.0,
            "m" | "M" => 1024.0 * 1024.0,
            "g" | "G" => 1024.0 * 1024.0 * 1024.0,
            _ => 1.0,
        };
        Some(n * unit)
    });

    // Too early in the clip, the extrapolation is mostly noise.
    let total = downloaded.filter(|_| fraction >= 0.02).map(|d| d / fraction);

    Some(ProgressInfo {
        pct: (fraction * 100.0).min(99.0),
        speed: None,
        downloaded: downloaded.map(|d| d as u64),
        total: total.map(|t| t as u64),
        total_estimated: total.is_some(),
    })
}

/// Attempts to extract progress from a single line of yt-dlp output (stdout)
/// or ffmpeg output (stderr, section downloads only).
fn parse_progress(line: &str, section_secs: Option<f64>) -> Option<Tick> {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix(PROGRESS_MARKER) {
        return parse_template_line(rest);
    }
    parse_ffmpeg_line(line, section_secs?).map(Tick::of)
}

// ── Whole-job progress ───────────────────────────────────────────────────────

/// One stream of a download that fetches several.
#[derive(Debug)]
struct Slot {
    /// The `format_id` the probe expected here; once a stream has claimed the
    /// slot, that stream's own.
    format_id: String,
    claimed: bool,
    /// Best known size: the probe's, until yt-dlp reports the real one.
    total: Option<u64>,
    total_estimated: bool,
    downloaded: u64,
    /// Known to be complete, so all of `total` counts as downloaded.
    finished: bool,
}

/// Folds the ticks of a download that fetches several streams into progress
/// for the whole job.
///
/// yt-dlp downloads video and audio as separate files, one after the other,
/// and restarts its byte counters for each. Passed on as they come, the bar
/// would run from 0 to 100 once per stream. Here every expected stream holds
/// a slot, sized by the probe until yt-dlp reports the real size, and the
/// job's progress is the sum of the slots.
///
/// A tick is matched to its slot by `format_id`; a stream the probe did not
/// name (a preset is only an approximation of yt-dlp's own choice) takes the
/// first slot nobody claimed, and a stream beyond the expected ones gets a
/// slot of its own. While some stream has no size at all, the job's total is
/// unknown and the stream's own progress is reported instead.
#[derive(Debug)]
struct Combiner {
    slots: Vec<Slot>,
    /// The slot of the stream fetched last.
    current: Option<usize>,
}

impl Combiner {
    /// `None` below two streams: a lone stream's own progress is the job's.
    fn new(expected: &[ExpectedStream]) -> Option<Self> {
        (expected.len() >= 2).then(|| Self {
            slots: expected
                .iter()
                .map(|e| Slot {
                    format_id: e.format_id.clone(),
                    claimed: false,
                    total: e.bytes.filter(|b| *b > 0),
                    total_estimated: !e.exact,
                    downloaded: 0,
                    finished: false,
                })
                .collect(),
            current: None,
        })
    }

    /// Progress of the whole job after `tick`.
    fn fold(&mut self, tick: Tick) -> ProgressInfo {
        let Some(id) = tick.stream.as_deref() else {
            // Which stream this is cannot be told: report it as it is.
            return tick.info;
        };
        let at = self.slot_for(id);
        // Streams are fetched one after another, so a tick for another
        // stream means the previous one is complete.
        if let Some(previous) = self.current.replace(at).filter(|&p| p != at) {
            self.slots[previous].finished = true;
        }

        let slot = &mut self.slots[at];
        if let Some(total) = tick.info.total {
            slot.total = Some(total);
            slot.total_estimated = tick.info.total_estimated;
        }
        slot.downloaded = tick.info.downloaded.unwrap_or(slot.downloaded);
        slot.finished = tick.already_complete;

        self.whole(tick.info.speed).unwrap_or(tick.info)
    }

    /// The slot of stream `id`, claimed by its first tick.
    fn slot_for(&mut self, id: &str) -> usize {
        let at = self
            .slots
            .iter()
            .position(|s| s.format_id == id)
            .or_else(|| self.slots.iter().position(|s| !s.claimed))
            .unwrap_or_else(|| {
                self.slots.push(Slot {
                    format_id: id.to_owned(),
                    claimed: true,
                    total: None,
                    total_estimated: true,
                    downloaded: 0,
                    finished: false,
                });
                self.slots.len() - 1
            });
        let slot = &mut self.slots[at];
        slot.claimed = true;
        slot.format_id = id.to_owned();
        at
    }

    /// The whole job's progress, or `None` while some stream's size is unknown.
    fn whole(&self, speed: Option<f64>) -> Option<ProgressInfo> {
        let (mut downloaded, mut total, mut estimated) = (0u64, 0u64, false);
        for slot in &self.slots {
            let size = slot.total?;
            total = total.saturating_add(size);
            downloaded =
                downloaded.saturating_add(if slot.finished { size } else { slot.downloaded });
            estimated |= slot.total_estimated;
        }
        if total == 0 {
            return None;
        }

        let mut pct = (downloaded as f64 / total as f64 * 100.0).clamp(0.0, 100.0);
        if estimated {
            // An estimate can run below the real size; never claim "done" from it.
            pct = pct.min(99.0);
        }
        Some(ProgressInfo {
            pct,
            speed,
            downloaded: Some(downloaded),
            total: Some(total),
            total_estimated: estimated,
        })
    }
}

/// Turns output lines into progress callbacks.
///
/// Both output threads feed one `Reporter`, so a download of several streams
/// is folded into whole-job progress whichever pipe a line arrives on.
struct Reporter {
    callback: Box<dyn Fn(ProgressInfo) + Send + Sync + 'static>,
    section_secs: Option<f64>,
    combiner: Mutex<Option<Combiner>>,
}

impl Reporter {
    fn new(
        callback: Box<dyn Fn(ProgressInfo) + Send + Sync + 'static>,
        section_secs: Option<f64>,
        streams: &[ExpectedStream],
    ) -> Self {
        Self { callback, section_secs, combiner: Mutex::new(Combiner::new(streams)) }
    }

    fn on_line(&self, line: &str) {
        let Some(tick) = parse_progress(line, self.section_secs) else { return };
        let info = {
            let mut combiner = self.combiner.lock().unwrap_or_else(PoisonError::into_inner);
            match combiner.as_mut() {
                Some(combiner) => combiner.fold(tick),
                // A stream found complete on disk fetched nothing, so with
                // nothing to fold it is not a download tick.
                None if tick.already_complete => return,
                None => tick.info,
            }
        };
        // The lock is released: the callback may take its time.
        (self.callback)(info);
    }
}

/// Feeds `on_line` every `\r`- or `\n`-terminated line of `reader` until EOF.
/// Both terminators matter: yt-dlp and ffmpeg redraw progress in place with `\r`.
fn for_each_line(mut reader: impl Read, mut on_line: impl FnMut(&str)) {
    let mut line = Vec::<u8>::new();
    let mut buf = [0u8; 4096];

    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
            Ok(n) => {
                for &b in &buf[..n] {
                    if b == b'\r' || b == b'\n' {
                        if !line.is_empty() {
                            on_line(&String::from_utf8_lossy(&line));
                            line.clear();
                        }
                    } else if line.len() < MAX_LINE_BYTES {
                        line.push(b);
                    }
                }
            }
        }
    }
    if !line.is_empty() {
        on_line(&String::from_utf8_lossy(&line));
    }
}

/// Runs yt-dlp for `opts`, reporting progress through `progress`.
///
/// With two or more `opts.streams`, the progress covers the whole job (see
/// `Combiner`) instead of one stream at a time.
///
/// Returns `Ok(Cancelled)` when `token` fires; the whole process tree has
/// been killed and reaped by then, so partial files are safe to delete.
/// History is *not* written here — recording a completion is the queue's job.
pub fn run(opts: DlOpts, progress: ProgressCb, token: &CancelToken) -> Result<RunOutcome> {
    let template = output_template(&opts);

    // Derive section duration once so the progress reporter can use it
    // without borrowing `opts` across the thread boundary (`Option<f64>` is
    // Copy).
    let section_secs: Option<f64> = opts.sections.as_deref().and_then(parse_section_secs);

    let mut args: Vec<OsString> = Vec::new();
    let mut push = |a: &str| args.push(a.into());
    push("--no-warnings");
    push("--no-playlist");
    push("--color");
    push("never");
    push("--newline");
    push("--progress");
    push("--progress-template");
    push(PROGRESS_TEMPLATE);

    args.extend(target_args(&opts, &template));

    if opts.sections.is_some() {
        // --download-sections forces yt-dlp to use ffmpeg as the downloader
        // (rather than its native HTTP downloader), so this is the only path
        // where ffmpeg's own stderr verbosity is in play. Lowering its
        // loglevel to "error" suppresses per-segment protocol chatter
        // (`Opening '...' for reading`, `Input #0/#1`, `Stream mapping`,
        // `Output #0`, ...) which can otherwise emit one multi-KB signed-URL
        // line per HLS fragment. `-stats` is re-specified explicitly so the
        // periodic `size=...time=...` lines `parse_ffmpeg_line` depends on
        // keep printing — confirmed empirically, since ffmpeg ties periodic
        // stats output to the same `info`-level gate as the chatter being
        // suppressed, separately from the `-stats` flag itself. ffmpeg's own
        // error/fatal diagnostics on real failures are unaffected, as
        // "error" is the threshold being kept, not dropped.
        args.extend(["--downloader-args".into(), "ffmpeg:-loglevel error -stats".into()]);
    }
    // Everything after `--` is positional, so a URL beginning with `-` can
    // never be read as an option.
    args.extend(["--".into(), opts.url.into()]);

    let mut cmd = crate::util::ytdlp();
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = process::spawn(cmd)?;

    let stdout = child.stdout().take().expect("capture stdout");
    let stderr = child.stderr().take().expect("capture stderr");

    // Both I/O threads report through one `Reporter`; wrap it in Arc so
    // ownership can be shared without requiring the callback to be Clone.
    let reporter: Option<Arc<Reporter>> =
        progress.map(|cb| Arc::new(Reporter::new(cb, section_secs, &opts.streams)));

    // ── stdout thread ─────────────────────────────────────────────────────────
    // Handles the structured progress lines yt-dlp emits for its own
    // downloader, including the final line it prints on success.
    let (reporter_stdout, token_stdout) = (reporter.clone(), token.clone());
    let stdout_handle = thread::spawn(move || {
        for_each_line(stdout, |line| {
            if token_stdout.is_cancelled() {
                return;
            }
            if let Some(reporter) = &reporter_stdout {
                reporter.on_line(line);
            }
        });
    });

    // ── stderr thread ─────────────────────────────────────────────────────────
    // Handles ffmpeg `-stats` lines (section downloads) and keeps only the
    // last `ERROR:` line for error reporting rather than the whole stream,
    // which can grow large over a long ffmpeg run.
    let (reporter_stderr, token_stderr) = (reporter, token.clone());
    let stderr_handle = thread::spawn(move || {
        let mut last_error: Option<String> = None;
        for_each_line(stderr, |line| {
            if !token_stderr.is_cancelled()
                && let Some(reporter) = &reporter_stderr
            {
                reporter.on_line(line);
            }
            if let Some(msg) = line.trim_start().strip_prefix("ERROR:") {
                last_error = Some(msg.trim().to_string());
            }
        });
        last_error
    });

    let exit = process::wait(&mut child, token);

    // `wait` guarantees the whole tree is gone on every path, so both pipes
    // hit EOF and these joins cannot hang.
    stdout_handle.join().unwrap_or(());
    let last_error = stderr_handle.join().unwrap_or_default();

    let status = match exit? {
        Exit::Cancelled => return Ok(RunOutcome::Cancelled),
        Exit::Done(status) => status,
    };
    anyhow::ensure!(
        status.success(),
        last_error.unwrap_or_else(|| match status.code() {
            Some(code) => format!("yt-dlp failed (exit code {code})"),
            None => "yt-dlp failed (terminated by signal)".to_string(),
        }),
    );
    Ok(RunOutcome::Completed)
}

/// The file `run(opts, …)` would write, found without fetching any media.
///
/// yt-dlp is asked for its own answer (`--print filename`, with the arguments
/// of the download), so the resolution tag, a merge that switches the
/// container (MP4 → MKV) and clip fields resolve exactly as in the real run.
/// The one exception is audio extraction: yt-dlp prints the name of the file
/// *before* extraction, so the extension is swapped for the target format.
///
/// Costs one extraction. `Ok(None)` means `token` fired.
pub fn planned_path(opts: &DlOpts, token: &CancelToken) -> Result<Option<PathBuf>> {
    let template = output_template(opts);

    let mut cmd = crate::util::ytdlp();
    cmd.args(["--print", "filename", "--skip-download", "--no-playlist", "--no-warnings", "-q"])
        .args(target_args(opts, &template))
        // Everything after `--` is positional (see `run`).
        .args(["--", opts.url.as_str()]);

    let Some(output) = process::capture(cmd, token)? else {
        return Ok(None);
    };
    anyhow::ensure!(
        output.status.success(),
        crate::util::extract_ytdlp_error(&String::from_utf8_lossy(&output.stderr), &output.status),
    );
    parse_planned_path(&String::from_utf8_lossy(&output.stdout), opts).map(Some)
}

/// Reads the first name yt-dlp printed. A clip prints exactly one.
fn parse_planned_path(stdout: &str, opts: &DlOpts) -> Result<PathBuf> {
    let line = stdout
        .lines()
        .find(|l| !l.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("yt-dlp did not report the output file name."))?;
    let path = PathBuf::from(line);
    Ok(match extracted_audio_ext(opts) {
        Some(ext) => path.with_extension(ext),
        None => path,
    })
}

/// Fails with `OutputExists` when `path` is already on disk.
pub fn ensure_absent(path: &Path) -> Result<()> {
    // A location that cannot be inspected is left for yt-dlp to report in its
    // own words rather than failing the download here.
    if path.try_exists().unwrap_or(false) {
        return Err(OutputExists(path.to_owned()).into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> DlOpts {
        DlOpts {
            url: "u".into(),
            format: None,
            sections: None,
            extra: vec![],
            output_name: None,
            dest_dir: "d".into(),
            temp_dir: "t".into(),
            streams: vec![],
        }
    }

    #[test]
    fn template_line_with_exact_total() {
        let info = parse_progress("ytdlp-gui-progress 512 1024 NA 2048.5", None).unwrap().info;
        assert_eq!(info.pct, 50.0);
        assert_eq!((info.downloaded, info.total, info.total_estimated), (Some(512), Some(1024), false));
        assert_eq!(info.speed, Some(2048.5));
    }

    #[test]
    fn template_line_with_estimated_total_never_reaches_100() {
        let info = parse_progress("ytdlp-gui-progress 900 NA 800 None", None).unwrap().info;
        assert_eq!(info.pct, 99.0);
        assert!(info.total_estimated);
        assert_eq!(info.speed, None);
    }

    #[test]
    fn template_line_without_any_total_still_reports_bytes() {
        let info = parse_progress("ytdlp-gui-progress 900 NA NA NA", None).unwrap().info;
        assert_eq!((info.pct, info.total, info.downloaded), (0.0, None, Some(900)));
        assert!(parse_progress("ytdlp-gui-progress NA NA NA NA", None).is_none());
    }

    fn tick(line: &str) -> Tick {
        parse_progress(line, None).unwrap()
    }

    #[test]
    fn template_line_names_its_stream() {
        let t = tick("ytdlp-gui-progress 512 1024 NA 2048.5 137");
        assert_eq!(t.stream.as_deref(), Some("137"));
        assert_eq!((t.info.pct, t.already_complete), (50.0, false));

        // The ID is the rest of the line, so one holding a space survives.
        assert_eq!(tick("ytdlp-gui-progress 1 2 NA NA hls 720p").stream.as_deref(), Some("hls 720p"));
        // yt-dlp prints NA for an ID it does not know; older lines have none.
        assert_eq!(tick("ytdlp-gui-progress 1 2 NA NA NA").stream, None);
        assert_eq!(tick("ytdlp-gui-progress 1 2 NA NA").stream, None);
    }

    #[test]
    fn a_stream_complete_on_disk_is_reported_by_its_size_alone() {
        let t = tick("ytdlp-gui-progress NA 1000 NA NA 137");
        assert!(t.already_complete);
        assert_eq!(t.stream.as_deref(), Some("137"));
        assert_eq!((t.info.downloaded, t.info.total, t.info.pct), (Some(1000), Some(1000), 100.0));

        // Without an ID, or with only an estimated size, there is nothing to
        // go on: such a line is still dropped.
        assert!(parse_progress("ytdlp-gui-progress NA 1000 NA NA NA", None).is_none());
        assert!(parse_progress("ytdlp-gui-progress NA 1000 NA NA", None).is_none());
        assert!(parse_progress("ytdlp-gui-progress NA NA 1000 NA 137", None).is_none());
    }

    #[test]
    fn ordinary_yt_dlp_output_is_ignored() {
        assert!(parse_progress("[download] Destination: x.mp4", None).is_none());
        assert!(parse_progress("[download]  12.0% of 5MiB", Some(60.0)).is_none());
    }

    #[test]
    fn ffmpeg_line_gives_percent_size_and_extrapolated_total() {
        let line = "size=    1024KiB time=00:00:15.00 bitrate=559.2kbits/s speed=30x";
        let info = parse_progress(line, Some(60.0)).unwrap().info;
        assert_eq!(info.pct, 25.0);
        assert_eq!(info.downloaded, Some(1024 * 1024));
        assert_eq!(info.total, Some(4 * 1024 * 1024));
        assert!(info.total_estimated);
    }

    #[test]
    fn ffmpeg_line_is_ignored_outside_section_downloads_and_before_data() {
        let line = "size=    1024kB time=00:00:15.00 bitrate=1x";
        assert!(parse_progress(line, None).is_none());
        assert!(parse_progress("size=       0kB time=N/A bitrate=N/A", Some(60.0)).is_none());
        let early = parse_progress("size=   10kB time=00:00:00.50", Some(600.0)).unwrap().info;
        assert_eq!(early.total, None);
    }

    // ── Whole-job progress ────────────────────────────────────────────────────

    fn expected(id: &str, bytes: Option<u64>, exact: bool) -> ExpectedStream {
        ExpectedStream { format_id: id.into(), bytes, exact }
    }

    /// A video of 900 bytes (137) and an audio of 100 (140), both exact.
    fn video_and_audio() -> Combiner {
        Combiner::new(&[expected("137", Some(900), true), expected("140", Some(100), true)]).unwrap()
    }

    fn fold(combiner: &mut Combiner, line: &str) -> ProgressInfo {
        combiner.fold(tick(line))
    }

    /// `pct`, rounded past floating-point noise.
    fn pct(info: &ProgressInfo) -> f64 {
        (info.pct * 1e6).round() / 1e6
    }

    #[test]
    fn video_then_audio_runs_the_bar_once() {
        let mut c = video_and_audio();
        let info = fold(&mut c, "ytdlp-gui-progress 450 900 NA 10 137");
        assert_eq!((pct(&info), info.downloaded, info.total), (45.0, Some(450), Some(1000)));
        assert_eq!((info.total_estimated, info.speed), (false, Some(10.0)));

        assert_eq!(pct(&fold(&mut c, "ytdlp-gui-progress 900 900 NA 10 137")), 90.0);

        // The audio starts at zero, and the bar carries on instead of restarting.
        let info = fold(&mut c, "ytdlp-gui-progress 0 100 NA 5 140");
        assert_eq!((pct(&info), info.downloaded), (90.0, Some(900)));
        let info = fold(&mut c, "ytdlp-gui-progress 25 100 NA 5 140");
        assert_eq!((pct(&info), info.downloaded), (92.5, Some(925)));
        let info = fold(&mut c, "ytdlp-gui-progress 100 100 NA 5 140");
        assert_eq!((info.pct, info.downloaded, info.total), (100.0, Some(1000), Some(1000)));
        assert!(!info.total_estimated);
    }

    #[test]
    fn the_real_size_of_a_stream_replaces_the_probes() {
        let mut c = video_and_audio();
        fold(&mut c, "ytdlp-gui-progress 900 900 NA 1 137");
        // The audio turns out twice as large as the probe said.
        let info = fold(&mut c, "ytdlp-gui-progress 0 200 NA 1 140");
        assert_eq!((info.downloaded, info.total), (Some(900), Some(1100)));
    }

    #[test]
    fn a_total_built_on_estimates_stays_estimated_and_below_100() {
        // The probe's audio size is an estimate until yt-dlp reports it.
        let mut c = Combiner::new(&[expected("137", Some(900), true), expected("140", Some(100), false)]).unwrap();
        let info = fold(&mut c, "ytdlp-gui-progress 900 900 NA 1 137");
        assert_eq!((pct(&info), info.total_estimated), (90.0, true));
        let info = fold(&mut c, "ytdlp-gui-progress 100 100 NA 1 140");
        assert_eq!((info.pct, info.total_estimated), (100.0, false));

        // A size yt-dlp itself only estimates (fragmented streams) is no better.
        let mut c = video_and_audio();
        fold(&mut c, "ytdlp-gui-progress 900 900 NA 1 137");
        let info = fold(&mut c, "ytdlp-gui-progress 100 NA 100 1 140");
        assert_eq!((info.pct, info.total_estimated), (99.0, true));
    }

    #[test]
    fn a_stream_the_probe_did_not_name_takes_the_first_free_slot() {
        // The probe guessed 401 + 140 for a preset; yt-dlp fetched 313 + 251.
        let mut c = Combiner::new(&[expected("401", Some(900), false), expected("140", Some(100), false)]).unwrap();
        let info = fold(&mut c, "ytdlp-gui-progress 90 800 NA 1 313");
        assert_eq!((info.downloaded, info.total), (Some(90), Some(900)));
        let info = fold(&mut c, "ytdlp-gui-progress 10 120 NA 1 251");
        assert_eq!((info.downloaded, info.total), (Some(810), Some(920)));
    }

    #[test]
    fn streams_are_matched_by_id_before_position() {
        // yt-dlp fetches the audio first, though the probe listed the video first.
        let mut c = video_and_audio();
        let info = fold(&mut c, "ytdlp-gui-progress 50 100 NA 1 140");
        assert_eq!((info.downloaded, info.total), (Some(50), Some(1000)));
        let info = fold(&mut c, "ytdlp-gui-progress 90 900 NA 1 137");
        assert_eq!((info.downloaded, info.total), (Some(190), Some(1000)));
    }

    #[test]
    fn a_resumed_run_counts_what_is_already_on_disk() {
        // Paused mid-video: the first tick resumes from the bytes on disk.
        let mut c = video_and_audio();
        let info = fold(&mut c, "ytdlp-gui-progress 600 900 NA 3 137");
        assert_eq!((pct(&info), info.downloaded), (60.0, Some(600)));

        // Paused during the audio: yt-dlp skips the finished video, reporting
        // only its size, and the bar must not fall back to the audio's share.
        let mut c = video_and_audio();
        let info = fold(&mut c, "ytdlp-gui-progress NA 900 NA NA 137");
        assert_eq!((pct(&info), info.downloaded, info.total), (90.0, Some(900), Some(1000)));
        let info = fold(&mut c, "ytdlp-gui-progress 20 100 NA 3 140");
        assert_eq!((pct(&info), info.downloaded), (92.0, Some(920)));
    }

    #[test]
    fn an_unknown_size_falls_back_to_the_streams_own_progress() {
        // Neither the probe nor yt-dlp knows the audio's size yet.
        let mut c = Combiner::new(&[expected("137", Some(900), true), expected("140", None, false)]).unwrap();
        let info = fold(&mut c, "ytdlp-gui-progress 450 900 NA 1 137");
        assert_eq!((info.pct, info.downloaded, info.total), (50.0, Some(450), Some(900)));

        // Once yt-dlp reports it, the whole job takes over.
        let info = fold(&mut c, "ytdlp-gui-progress 10 100 NA 1 140");
        assert_eq!((info.downloaded, info.total), (Some(910), Some(1000)));
    }

    #[test]
    fn a_tick_without_a_stream_passes_through() {
        let mut c = video_and_audio();
        let info = fold(&mut c, "ytdlp-gui-progress 450 900 NA 1");
        assert_eq!((info.pct, info.downloaded, info.total), (50.0, Some(450), Some(900)));
    }

    #[test]
    fn a_stream_nobody_expected_gets_a_slot_of_its_own() {
        let mut c = video_and_audio();
        fold(&mut c, "ytdlp-gui-progress 900 900 NA 1 137");
        fold(&mut c, "ytdlp-gui-progress 100 100 NA 1 140");
        let info = fold(&mut c, "ytdlp-gui-progress 50 500 NA 1 999");
        assert_eq!((info.downloaded, info.total), (Some(1050), Some(1500)));
    }

    #[test]
    fn fewer_than_two_streams_need_no_combining() {
        assert!(Combiner::new(&[]).is_none());
        assert!(Combiner::new(&[expected("18", Some(10), true)]).is_none());
    }

    /// Runs `lines` through a `Reporter` and returns what it reported.
    fn reported(streams: &[ExpectedStream], section_secs: Option<f64>, lines: &[&str]) -> Vec<ProgressInfo> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let reporter = Reporter::new(
            Box::new(move |info| sink.lock().unwrap().push(info)),
            section_secs,
            streams,
        );
        lines.iter().for_each(|line| reporter.on_line(line));
        seen.lock().unwrap().clone()
    }

    #[test]
    fn the_reporter_folds_several_streams_and_skips_other_output() {
        let streams = [expected("137", Some(900), true), expected("140", Some(100), true)];
        let got = reported(
            &streams,
            None,
            &[
                "[download] Destination: x.f137.mp4",
                "ytdlp-gui-progress 900 900 NA 1 137",
                "ytdlp-gui-progress 50 100 NA 1 140",
            ],
        );
        assert_eq!(got.iter().map(pct).collect::<Vec<_>>(), [90.0, 95.0]);
    }

    #[test]
    fn a_single_stream_is_reported_as_it_always_was() {
        let lines = ["ytdlp-gui-progress 512 1024 NA 2 137", "ytdlp-gui-progress 1024 1024 NA 2 137"];
        for streams in [vec![], vec![expected("137", Some(1024), true)]] {
            let got = reported(&streams, None, &lines);
            assert_eq!(got.iter().map(|i| i.pct).collect::<Vec<_>>(), [50.0, 100.0]);
        }
        // A file found complete on disk is no download tick when there is
        // nothing to fold it into.
        assert!(reported(&[], None, &["ytdlp-gui-progress NA 1024 NA NA 137"]).is_empty());
    }

    #[test]
    fn ffmpeg_lines_still_report_for_section_downloads() {
        let line = "size=    1024KiB time=00:00:15.00 bitrate=559.2kbits/s speed=30x";
        let got = reported(&[], Some(60.0), &[line]);
        assert_eq!(got.iter().map(|i| i.pct).collect::<Vec<_>>(), [25.0]);
    }

    #[test]
    fn section_duration_from_specifier() {
        assert_eq!(parse_section_secs("*00:01:30-00:03:00"), Some(90.0));
        assert_eq!(parse_section_secs("*00:03:00-00:01:00"), Some(0.0));
        assert_eq!(parse_section_secs("nonsense"), None);
    }

    #[test]
    fn lines_split_on_either_terminator_and_flush_the_tail() {
        let mut seen = Vec::new();
        for_each_line(&b"a\rb\r\nc\n\nlast"[..], |l| seen.push(l.to_owned()));
        assert_eq!(seen, ["a", "b", "c", "last"]);
    }

    #[test]
    fn output_template_variants() {
        let mut o = opts();
        assert_eq!(output_template(&o), "%(title)s%(height& [{}p]|)s.%(ext)s");
        o.sections = Some("*0-1".into());
        assert_eq!(
            output_template(&o),
            "%(title)s-%(section_start)s-%(section_end)s%(height& [{}p]|)s.%(ext)s"
        );
        o.output_name = Some("01 - A".into());
        assert_eq!(output_template(&o), "01 - A%(height& [{}p]|)s.%(ext)s");
    }

    #[test]
    fn extracted_audio_carries_no_height_tag() {
        let mut o = opts();
        o.extra = ["-x", "--audio-format", "mp3"].map(String::from).to_vec();
        assert_eq!(output_template(&o), "%(title)s.%(ext)s");
        assert_eq!(extracted_audio_ext(&o), Some("mp3"));

        // Other extras leave the tag, and the extension, alone.
        o.extra = ["-S", "res:1080", "--merge-output-format", "mp4"].map(String::from).to_vec();
        assert_eq!(output_template(&o), "%(title)s%(height& [{}p]|)s.%(ext)s");
        assert_eq!(extracted_audio_ext(&o), None);

        // Extraction with yt-dlp's own pick of format: the extension is unknown.
        o.extra = ["--extract-audio", "--audio-format", "best"].map(String::from).to_vec();
        assert_eq!(extracted_audio_ext(&o), None);
        o.extra = vec!["-x".into()];
        assert_eq!(extracted_audio_ext(&o), None);
    }

    #[test]
    fn target_args_are_shared_by_download_and_preflight() {
        let mut o = opts();
        o.sections = Some("*00:00:01-00:00:02".into());
        o.format = Some("18".into());
        o.extra = vec!["-x".into()];
        let args: Vec<String> =
            target_args(&o, "T").iter().map(|a| a.to_string_lossy().into_owned()).collect();
        let at = |flag: &str| args.iter().position(|a| a == flag).unwrap_or_else(|| panic!("{flag}"));
        assert_eq!(args[at("-P") + 1], "home:d");
        assert_eq!(args[at("-o") + 1], "T");
        assert_eq!(args[at("--download-sections") + 1], "*00:00:01-00:00:02");
        assert_eq!(args[at("-f") + 1], "18");
        assert!(args.contains(&"-x".to_string()));
        assert!(!args.contains(&"--progress".to_string()), "progress output belongs to the download only");
    }

    #[test]
    fn planned_path_is_the_first_name_printed() {
        let o = opts();
        let path = parse_planned_path("\n/d/My Video [1080p].mp4\n/d/other.mp4\n", &o).unwrap();
        assert_eq!(path, PathBuf::from("/d/My Video [1080p].mp4"));
        assert!(parse_planned_path("  \n\n", &o).is_err());
    }

    #[test]
    fn planned_path_of_extracted_audio_gets_the_target_extension() {
        let mut o = opts();
        o.extra = ["-x", "--audio-format", "mp3"].map(String::from).to_vec();
        // yt-dlp prints the name before extraction; dots in the title survive.
        let path = parse_planned_path("/d/Mr. Robot S01E01.webm\n", &o).unwrap();
        assert_eq!(path, PathBuf::from("/d/Mr. Robot S01E01.mp3"));
    }

    #[test]
    fn an_existing_output_is_refused_with_a_typed_error() {
        let dir = std::env::temp_dir().join(format!("ytdlp-gui-yt-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let taken = dir.join("Title [1080p].mp4");
        std::fs::write(&taken, b"x").unwrap();

        let err = ensure_absent(&taken).unwrap_err();
        let exists = err.downcast_ref::<OutputExists>().expect("typed error");
        assert_eq!(exists.0, taken);
        assert_eq!(
            err.to_string(),
            "File already exists: Title [1080p].mp4. Choose a different quality or range, or another folder, then retry."
        );
        assert!(ensure_absent(&dir.join("Title [720p].mp4")).is_ok());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
