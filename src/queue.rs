//! The download queue.
//!
//! A row is a *job*: a URL plus its probed metadata, a `JobSpec` (quality ×
//! scope) and a lifecycle. Nothing starts by itself — a download runs only
//! after an explicit `start`, and there is no concurrency cap.
//!
//! yt-dlp has no pause, so *pause* kills the process tree and *resume*
//! respawns it with identical arguments; yt-dlp continues from the `.part`
//! files it left in the job's private temp directory. Only its own downloader
//! can: a clip or a chapter is fetched by ffmpeg (`--download-sections`),
//! which restarts the interrupted file — chapters that already finished are
//! kept. *Stop* kills and deletes that directory, so the next start begins
//! from zero.
//!
//! A fresh start (not a resume) first asks yt-dlp which file it would write
//! (`yt::planned_path`) and refuses with `yt::OutputExists` when that file is
//! already there: yt-dlp would skip it yet exit 0, and the queue would record
//! a download that never happened. Nothing was fetched, so the row is then
//! unlocked and the user can change quality, range or folder before retrying.
//!
//! Persistence: every job lives in the `queue` table of the application
//! database (shared with history). Progress ticks are in-memory only. On
//! startup a job that was downloading is restored as paused; one that was
//! still probing is probed again.
//!
//! Threading: all mutable state sits behind one mutex. Worker threads (one
//! per probe or download) are tracked in `State::runs` so any of
//! pause / stop / remove / shutdown can cancel and join them. Events are
//! always emitted *after* the lock is released, so a sink may call back in.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};

use anyhow::{anyhow, bail};
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::modes::chapters::{self, ChapterPlan};
use crate::probe::{self, Meta, SizeEstimate};
use crate::process::CancelToken;
use crate::types::{DlOpts, JobSpec, ProgressInfo, Quality, Result, Scope};
use crate::yt::{self, RunOutcome};
use crate::{config, history, preset};

pub type JobId = i64;

const NOT_FOUND: &str = "That download no longer exists.";
const LOCKED: &str = "Stop the download first — this setting can't change while it has partial files.";

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS queue (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    url           TEXT    NOT NULL,
    state         TEXT    NOT NULL,
    spec          TEXT    NOT NULL,
    meta          TEXT,
    error         TEXT,
    item_dir      TEXT,
    frozen_dir    TEXT,
    chapters_done INTEGER NOT NULL DEFAULT 0
);";

// ── Public model ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Fetching metadata.
    Probing,
    /// Ready; waiting for the user to press play.
    Queued,
    Downloading,
    Paused,
    /// Killed by the user; partial files deleted.
    Stopped,
    /// The probe or the download failed; `error` says why. Play retries.
    Failed,
}

impl JobState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Probing => "probing",
            Self::Queued => "queued",
            Self::Downloading => "downloading",
            Self::Paused => "paused",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "probing" => Self::Probing,
            "queued" => Self::Queued,
            "downloading" => Self::Downloading,
            "paused" => Self::Paused,
            "stopped" => Self::Stopped,
            "failed" => Self::Failed,
            _ => return None,
        })
    }
}

/// What the frontend renders for one row.
#[derive(Clone, Debug, Serialize)]
pub struct JobView {
    pub id: JobId,
    pub url: String,
    pub state: JobState,
    pub spec: JobSpec,
    /// `None` until the probe succeeds (and after it fails).
    pub meta: Option<Meta>,
    pub error: Option<String>,
    /// Where finished files will land, or already are headed.
    pub dir: String,
    /// `true` when the row has its own folder instead of the common one.
    pub custom_dir: bool,
    /// `true` while partial files may exist; quality, scope and folder are
    /// read-only until the download is stopped.
    pub locked: bool,
    /// Chapter jobs: how many of the selected chapters are finished. While
    /// downloading, this is also the zero-based position of the current one.
    pub chapters_done: usize,
    pub est_size: Option<SizeEstimate>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QueueEvent {
    Added { job: JobView },
    Updated { job: JobView },
    Progress { id: JobId, info: ProgressInfo },
    /// Finished and moved to history; the row is gone.
    Completed { id: JobId },
    Removed { id: JobId },
}

pub type EventSink = Arc<dyn Fn(QueueEvent) + Send + Sync + 'static>;

/// The hidden per-job directory inside the destination folder that holds a
/// job's `.part` files and other intermediates.
pub fn partial_dir(dest: &Path, id: JobId) -> PathBuf {
    dest.join(format!(".ytdl-job-{id}"))
}

// ── Internal state ───────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct Job {
    id: JobId,
    url: String,
    state: JobState,
    spec: JobSpec,
    meta: Option<Meta>,
    error: Option<String>,
    /// Folder the user picked for this row.
    item_dir: Option<String>,
    /// Folder pinned when the download first started. `Some` means partial
    /// files may exist there, so it must not change under a paused job.
    frozen_dir: Option<String>,
    chapters_done: usize,
}

struct Run {
    token: CancelToken,
    handle: JoinHandle<()>,
}

struct State {
    conn: Connection,
    jobs: BTreeMap<JobId, Job>,
    runs: HashMap<JobId, Run>,
}

struct Inner {
    state: Mutex<State>,
    emit: EventSink,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        // A panicking worker must not wedge the whole queue.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn emit(&self, event: QueueEvent) {
        (self.emit)(event);
    }
}

fn persist(conn: &Connection, job: &Job) -> Result<()> {
    conn.execute(
        "UPDATE queue SET state = ?1, spec = ?2, error = ?3, item_dir = ?4,
                          frozen_dir = ?5, chapters_done = ?6
         WHERE id = ?7",
        params![
            job.state.as_str(),
            serde_json::to_string(&job.spec)?,
            job.error,
            job.item_dir,
            job.frozen_dir,
            job.chapters_done as i64,
            job.id,
        ],
    )?;
    Ok(())
}

fn persist_meta(conn: &Connection, id: JobId, meta: &Meta) -> Result<()> {
    conn.execute(
        "UPDATE queue SET meta = ?1 WHERE id = ?2",
        params![serde_json::to_string(meta)?, id],
    )?;
    Ok(())
}

impl State {
    fn job(&self, id: JobId) -> Result<&Job> {
        self.jobs.get(&id).ok_or_else(|| anyhow!(NOT_FOUND))
    }

    /// Applies `change` to a copy of the job, writes it to the database, and
    /// only then replaces the in-memory job, so the two cannot diverge when a
    /// write fails.
    fn update(&mut self, id: JobId, change: impl FnOnce(&mut Job) -> Result<()>) -> Result<()> {
        let mut job = self.job(id)?.clone();
        change(&mut job)?;
        persist(&self.conn, &job)?;
        self.jobs.insert(id, job);
        Ok(())
    }

    fn view(&self, id: JobId) -> Result<JobView> {
        Ok(self.job(id)?.view(&config::get_download_dir_display()))
    }

    fn updated(&self, id: JobId) -> Result<QueueEvent> {
        Ok(QueueEvent::Updated { job: self.view(id)? })
    }

    /// Moves a finished job from the queue to the history in one transaction.
    /// Returns the directory whose leftovers should be cleaned up.
    fn complete(&mut self, id: JobId) -> Result<Option<PathBuf>> {
        let job = self.job(id)?;
        let title = job.meta.as_ref().map_or_else(|| job.url.clone(), |m| m.title.clone());
        let (url, spec) = (job.url.clone(), job.spec.clone());
        let cleanup = job.frozen_dir.as_deref().map(|d| partial_dir(Path::new(d), id));

        let tx = self.conn.transaction()?;
        history::log_entry_on(&tx, &title, &url, &spec)?;
        tx.execute("DELETE FROM queue WHERE id = ?1", params![id])?;
        tx.commit()?;

        self.jobs.remove(&id);
        Ok(cleanup)
    }

    /// Records a failure. The in-memory state is authoritative for the
    /// session, so a failing database write here is deliberately tolerated:
    /// a `downloading` row restores as paused anyway.
    fn mark_failed(&mut self, id: JobId, message: String) {
        if let Some(job) = self.jobs.get_mut(&id) {
            job.state = JobState::Failed;
            job.error = Some(message);
            let _ = persist(&self.conn, job);
        }
    }

    /// Like `mark_failed`, for a run that was refused before it fetched
    /// anything (its output already exists). No partial files can exist, so
    /// the folder is unpinned: quality, range and folder are editable again
    /// and the next play starts afresh.
    fn mark_refused(&mut self, id: JobId, message: String) {
        if let Some(job) = self.jobs.get_mut(&id) {
            job.frozen_dir = None;
            job.state = JobState::Failed;
            job.error = Some(message);
            let _ = persist(&self.conn, job);
        }
    }
}

impl Job {
    fn view(&self, common_dir: &str) -> JobView {
        JobView {
            id: self.id,
            url: self.url.clone(),
            state: self.state,
            spec: self.spec.clone(),
            meta: self.meta.clone(),
            error: self.error.clone(),
            dir: self
                .item_dir
                .as_deref()
                .or(self.frozen_dir.as_deref())
                .unwrap_or(common_dir)
                .to_owned(),
            custom_dir: self.item_dir.is_some(),
            locked: self.frozen_dir.is_some(),
            chapters_done: self.chapters_done,
            est_size: self.meta.as_ref().and_then(|m| m.estimate_size(&self.spec)),
        }
    }
}

fn default_spec() -> JobSpec {
    JobSpec { quality: Quality::Preset(config::get_last_preset()), scope: Scope::Full }
}

/// Deletes a job's private temp directory. Best-effort on purpose: a
/// leftover hidden directory is harmless — the next run either resumes from
/// it or overwrites it — and failing a stop or remove over it would be worse.
fn remove_partials(dest: &Path, id: JobId) {
    let _ = fs::remove_dir_all(partial_dir(dest, id));
}

fn join(run: Option<Run>) {
    if let Some(run) = run {
        let _ = run.handle.join();
    }
}

// ── Loading ──────────────────────────────────────────────────────────────────

fn load_jobs(conn: &Connection) -> Result<BTreeMap<JobId, Job>> {
    let mut stmt = conn.prepare(
        "SELECT id, url, state, spec, meta, error, item_dir, frozen_dir, chapters_done
         FROM queue ORDER BY id",
    )?;
    let rows = stmt.query_map([], |row| {
        let state: String = row.get(2)?;
        let spec: String = row.get(3)?;
        let meta: Option<String> = row.get(4)?;
        Ok(Job {
            id: row.get(0)?,
            url: row.get(1)?,
            state: JobState::parse(&state).unwrap_or(JobState::Failed),
            spec: serde_json::from_str(&spec).unwrap_or_default(),
            meta: meta.and_then(|m| serde_json::from_str(&m).ok()),
            error: row.get(5)?,
            item_dir: row.get(6)?,
            frozen_dir: row.get(7)?,
            chapters_done: usize::try_from(row.get::<_, i64>(8)?).unwrap_or(0),
        })
    })?;

    let mut jobs = BTreeMap::new();
    for row in rows {
        let mut job = row?;
        let before = (job.state, job.meta.is_some());
        match (job.state, job.meta.is_some()) {
            // The app died or closed mid-download: wait for an explicit play.
            (JobState::Downloading, true) => job.state = JobState::Paused,
            // Without metadata a row can't be started; probe it again. A
            // failed probe stays failed so its error remains visible.
            (JobState::Failed, false) | (JobState::Probing, _) => {}
            (_, false) => job.state = JobState::Probing,
            _ => {}
        }
        if job.state == JobState::Probing {
            job.meta = None;
        }
        if (job.state, job.meta.is_some()) != before {
            persist(conn, &job)?;
        }
        jobs.insert(job.id, job);
    }
    Ok(jobs)
}

// ── Workers ──────────────────────────────────────────────────────────────────

fn spawn_probe(inner: &Arc<Inner>, st: &mut State, id: JobId, url: String) {
    let token = CancelToken::new();
    let (worker, worker_token) = (Arc::clone(inner), token.clone());
    let handle = thread::spawn(move || probe_worker(&worker, id, &url, &worker_token));
    // The worker's first act is taking the state lock, which the caller still
    // holds, so the run is always registered before the worker can finish.
    st.runs.insert(id, Run { token, handle });
}

fn probe_worker(inner: &Arc<Inner>, id: JobId, url: &str, token: &CancelToken) {
    let result = catch_unwind(AssertUnwindSafe(|| probe::fetch(url, token)))
        .unwrap_or_else(|_| Err(anyhow!("Internal error while reading the video's details.")));
    let fallback = config::get_last_preset();

    let event = {
        let mut st = inner.lock();
        if token.is_cancelled() {
            return;
        }
        st.runs.remove(&id);

        let outcome = match result {
            Ok(Some(meta)) => st.update(id, |job| {
                job.spec = job
                    .spec
                    .reconciled(&meta.formats, meta.chapters.len(), meta.duration, fallback);
                job.state = JobState::Queued;
                job.error = None;
                job.meta = Some(meta);
                Ok(())
            }),
            Ok(None) => return,
            Err(e) => Err(e),
        };
        match outcome {
            Ok(()) => {
                // The meta column is written separately: `persist` skips it so
                // a state change never rewrites the (large) format list.
                let saved = st.jobs.get(&id).and_then(|j| j.meta.as_ref()).map(|m| persist_meta(&st.conn, id, m));
                if let Some(Err(e)) = saved {
                    st.mark_failed(id, format!("Could not save the video's details: {e}"));
                }
            }
            Err(e) => st.mark_failed(id, e.to_string()),
        }
        st.updated(id)
    };
    if let Ok(event) = event {
        inner.emit(event);
    }
}

fn spawn_download(inner: &Arc<Inner>, st: &mut State, id: JobId) {
    let token = CancelToken::new();
    let (worker, worker_token) = (Arc::clone(inner), token.clone());
    let handle = thread::spawn(move || download_worker(&worker, id, &worker_token));
    st.runs.insert(id, Run { token, handle });
}

fn download_worker(inner: &Arc<Inner>, id: JobId, token: &CancelToken) {
    let outcome = catch_unwind(AssertUnwindSafe(|| execute(inner, id, token)))
        .unwrap_or_else(|_| Err(anyhow!("Internal error during the download.")));

    let (events, cleanup) = {
        let mut st = inner.lock();
        // Whoever cancelled us (pause / stop / remove / shutdown) has already
        // updated the job and owns any cleanup; just step aside. The flag is
        // set under this same lock, so the check cannot race it.
        if token.is_cancelled() {
            return;
        }
        st.runs.remove(&id);

        let mut events = Vec::new();
        let mut cleanup = None;
        let outcome = match outcome {
            Ok(RunOutcome::Completed) => st.complete(id).map(|dir| {
                cleanup = dir;
                events.push(QueueEvent::Completed { id });
            }),
            Ok(RunOutcome::Cancelled) => return,
            Err(e) => Err(e),
        };
        if let Err(e) = outcome {
            if e.downcast_ref::<yt::OutputExists>().is_some() {
                st.mark_refused(id, e.to_string());
            } else {
                st.mark_failed(id, e.to_string());
            }
            events.extend(st.updated(id).ok());
        }
        (events, cleanup)
    };

    if let Some(dir) = cleanup {
        // `complete` returned the per-job temp dir itself.
        let _ = fs::remove_dir_all(dir);
    }
    for event in events {
        inner.emit(event);
    }
}

fn progress_cb(inner: &Arc<Inner>, id: JobId) -> yt::ProgressCb {
    let inner = Arc::clone(inner);
    Some(Box::new(move |info| inner.emit(QueueEvent::Progress { id, info })))
}

/// Whether `temp` holds anything — yt-dlp's `.part` files — i.e. whether a
/// start continues an earlier run instead of beginning one.
fn has_partials(temp: &Path) -> bool {
    fs::read_dir(temp).is_ok_and(|mut entries| entries.next().is_some())
}

/// Pre-flight for one output file: `Ok(false)` when cancelled, an
/// `OutputExists` error when the file is already there.
fn output_is_free(opts: &DlOpts, token: &CancelToken) -> Result<bool> {
    let Some(path) = yt::planned_path(opts, token)? else {
        return Ok(false);
    };
    yt::ensure_absent(&path)?;
    Ok(true)
}

/// Pre-flight for the chapters still to download. One yt-dlp call names the
/// first (`first`); the others follow from it. `Ok(false)` when cancelled.
fn chapters_are_free(first: &DlOpts, remaining: &[ChapterPlan], token: &CancelToken) -> Result<bool> {
    let Some(sample) = yt::planned_path(first, token)? else {
        return Ok(false);
    };
    // Names that cannot be derived (yt-dlp shortened one) skip the check
    // instead of guessing.
    for path in chapters::sibling_paths(remaining, &sample).unwrap_or_default() {
        yt::ensure_absent(&path)?;
    }
    Ok(true)
}

/// Runs the yt-dlp invocation(s) a job needs: one for the full video or a
/// clip, one per remaining chapter otherwise.
fn execute(inner: &Arc<Inner>, id: JobId, token: &CancelToken) -> Result<RunOutcome> {
    let job = inner.lock().job(id)?.clone();
    let meta = job
        .meta
        .as_ref()
        .ok_or_else(|| anyhow!("The video's details are missing; retry to load them again."))?;
    let dest = PathBuf::from(
        job.frozen_dir
            .as_deref()
            .ok_or_else(|| anyhow!("The download folder is not set."))?,
    );
    let temp = partial_dir(&dest, id);

    let (format, extra) = match &job.spec.quality {
        Quality::Preset(p) => (None, preset::extra_args(*p)),
        Quality::Formats(ids) => (Some(meta.format_selector(ids)), Vec::new()),
    };
    let opts = |sections: Option<String>, output_name: Option<String>| DlOpts {
        url: job.url.clone(),
        format: format.clone(),
        sections,
        extra: extra.clone(),
        output_name,
        dest_dir: dest.clone(),
        temp_dir: temp.clone(),
        streams: Vec::new(),
    };

    // Finished chapters or partial files mean this continues a run that
    // already passed the existence check, so it is not repeated.
    let resuming = job.chapters_done > 0 || has_partials(&temp);

    match &job.spec.scope {
        Scope::Full => {
            let mut run_opts = opts(None, None);
            // yt-dlp fetches video and audio as separate files. Naming the
            // expected streams lets `yt::run` report one progress for the
            // whole job. A clip or a chapter is a single ffmpeg call and
            // already reports as one.
            run_opts.streams = meta.expected_streams(&job.spec.quality);
            if !resuming && !output_is_free(&run_opts, token)? {
                return Ok(RunOutcome::Cancelled);
            }
            yt::run(run_opts, progress_cb(inner, id), token)
        }
        Scope::Timeline { start, end } => {
            let run_opts = opts(Some(format!("*{start}-{end}")), None);
            if !resuming && !output_is_free(&run_opts, token)? {
                return Ok(RunOutcome::Cancelled);
            }
            yt::run(run_opts, progress_cb(inner, id), token)
        }
        Scope::Chapters { selected } => {
            let plan = chapters::plan(&meta.chapters, selected);
            let remaining = plan.get(job.chapters_done..).unwrap_or_default();
            if !resuming
                && let Some(first) = remaining.first()
            {
                let sample = opts(Some(first.section.clone()), Some(first.output_name.clone()));
                if !chapters_are_free(&sample, remaining, token)? {
                    return Ok(RunOutcome::Cancelled);
                }
            }
            for (position, chapter) in plan.iter().enumerate().skip(job.chapters_done) {
                if token.is_cancelled() {
                    return Ok(RunOutcome::Cancelled);
                }
                let chapter_opts =
                    opts(Some(chapter.section.clone()), Some(chapter.output_name.clone()));
                if yt::run(chapter_opts, progress_cb(inner, id), token)? == RunOutcome::Cancelled {
                    return Ok(RunOutcome::Cancelled);
                }
                // Persist the cursor, so a pause or restart resumes at the
                // chapter that was interrupted instead of redoing finished ones.
                let event = {
                    let mut st = inner.lock();
                    st.update(id, |j| {
                        j.chapters_done = position + 1;
                        Ok(())
                    })?;
                    st.updated(id)?
                };
                inner.emit(event);
            }
            Ok(RunOutcome::Completed)
        }
    }
}

// ── Public API ───────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Queue {
    inner: Arc<Inner>,
}

impl Queue {
    /// Opens the database, restores the persisted queue and re-probes rows
    /// that never finished probing. `emit` receives every state change.
    pub fn open(emit: EventSink) -> Result<Queue> {
        let conn = history::open()?;
        conn.execute_batch(SCHEMA)?;
        let jobs = load_jobs(&conn)?;
        let to_probe: Vec<(JobId, String)> = jobs
            .values()
            .filter(|j| j.state == JobState::Probing)
            .map(|j| (j.id, j.url.clone()))
            .collect();

        let inner = Arc::new(Inner {
            state: Mutex::new(State { conn, jobs, runs: HashMap::new() }),
            emit,
        });
        {
            let mut st = inner.lock();
            for (id, url) in to_probe {
                spawn_probe(&inner, &mut st, id, url);
            }
        }
        Ok(Queue { inner })
    }

    pub fn list(&self) -> Vec<JobView> {
        let st = self.inner.lock();
        let common = config::get_download_dir_display();
        st.jobs.values().map(|j| j.view(&common)).collect()
    }

    /// Adds a row with the last-used quality and starts reading its details.
    pub fn add(&self, url: &str) -> Result<JobView> {
        self.add_with_spec(url, default_spec())
    }

    fn add_with_spec(&self, url: &str, spec: JobSpec) -> Result<JobView> {
        let url = url.trim();
        if url.is_empty() {
            bail!("Enter a video URL.");
        }

        let job = {
            let mut st = self.inner.lock();
            st.conn.execute(
                "INSERT INTO queue (url, state, spec) VALUES (?1, ?2, ?3)",
                params![url, JobState::Probing.as_str(), serde_json::to_string(&spec)?],
            )?;
            let id = st.conn.last_insert_rowid();
            st.jobs.insert(
                id,
                Job {
                    id,
                    url: url.to_owned(),
                    state: JobState::Probing,
                    spec,
                    meta: None,
                    error: None,
                    item_dir: None,
                    frozen_dir: None,
                    chapters_done: 0,
                },
            );
            spawn_probe(&self.inner, &mut st, id, url.to_owned());
            st.view(id)?
        };
        self.inner.emit(QueueEvent::Added { job: job.clone() });
        Ok(job)
    }

    /// Re-queues history entries at the quality they were downloaded with
    /// (entries from before the queue existed use the default quality).
    pub fn redownload(&self, urls: &[String]) -> Result<Vec<JobView>> {
        urls.iter()
            .map(|url| {
                let spec = history::get_entry(url).and_then(|e| e.spec).unwrap_or_else(default_spec);
                self.add_with_spec(url, spec)
            })
            .collect()
    }

    /// Replaces a row's quality and scope. Rejected while partial files exist.
    pub fn set_spec(&self, id: JobId, spec: JobSpec) -> Result<JobView> {
        let (event, view, preset) = {
            let mut st = self.inner.lock();
            let job = st.job(id)?;
            if job.frozen_dir.is_some() {
                bail!(LOCKED);
            }
            let meta = job
                .meta
                .as_ref()
                .ok_or_else(|| anyhow!("The video's details haven't loaded yet."))?;
            let spec = spec.validated(&meta.formats, meta.chapters.len(), meta.duration)?;
            let preset = match &spec.quality {
                Quality::Preset(p) => Some(*p),
                Quality::Formats(_) => None,
            };
            st.update(id, |j| {
                j.spec = spec;
                Ok(())
            })?;
            (st.updated(id)?, st.view(id)?, preset)
        };
        // Remembering the preset is a convenience; failing to save it must not
        // fail the edit the user just made.
        if let Some(p) = preset {
            let _ = config::set_last_preset(p);
        }
        self.inner.emit(event);
        Ok(view)
    }

    /// Sets or clears the row's own download folder. Rejected while partial
    /// files exist, since they live under the folder in use.
    pub fn set_item_dir(&self, id: JobId, dir: Option<String>) -> Result<JobView> {
        if let Some(dir) = &dir {
            let path = Path::new(dir);
            if !path.is_absolute() || !path.is_dir() {
                bail!("Choose an existing folder.");
            }
        }
        let (event, view) = {
            let mut st = self.inner.lock();
            if st.job(id)?.frozen_dir.is_some() {
                bail!(LOCKED);
            }
            st.update(id, |j| {
                j.item_dir = dir;
                Ok(())
            })?;
            (st.updated(id)?, st.view(id)?)
        };
        self.inner.emit(event);
        Ok(view)
    }

    /// Play. Starts a queued or stopped row, resumes a paused one, retries a
    /// failed download, or — for a row whose probe failed — retries the probe.
    pub fn start(&self, id: JobId) -> Result<()> {
        let event = {
            let mut st = self.inner.lock();
            let job = st.job(id)?;
            match job.state {
                JobState::Downloading => bail!("This download is already running."),
                JobState::Probing => bail!("Still reading the video's details."),
                _ => {}
            }
            if job.meta.is_none() {
                let url = job.url.clone();
                st.update(id, |j| {
                    j.state = JobState::Probing;
                    j.error = None;
                    Ok(())
                })?;
                spawn_probe(&self.inner, &mut st, id, url);
            } else {
                // Pin the folder now: from here on partial files exist there.
                let dir = job
                    .item_dir
                    .clone()
                    .or_else(|| job.frozen_dir.clone())
                    .unwrap_or_else(|| config::get_download_dir().to_string_lossy().into_owned());
                st.update(id, |j| {
                    j.frozen_dir = Some(dir);
                    j.state = JobState::Downloading;
                    j.error = None;
                    Ok(())
                })?;
                spawn_download(&self.inner, &mut st, id);
            }
            st.updated(id)?
        };
        self.inner.emit(event);
        Ok(())
    }

    /// Kills the download but keeps its partial files for a later resume.
    pub fn pause(&self, id: JobId) -> Result<()> {
        let (run, event) = {
            let mut st = self.inner.lock();
            if st.job(id)?.state != JobState::Downloading {
                bail!("This download isn't running.");
            }
            st.update(id, |j| {
                j.state = JobState::Paused;
                Ok(())
            })?;
            let run = st.runs.remove(&id);
            if let Some(run) = &run {
                run.token.cancel();
            }
            (run, st.updated(id)?)
        };
        self.inner.emit(event);
        join(run);
        Ok(())
    }

    /// Kills the download and deletes its partial files. The row stays, as
    /// `Stopped`; play starts it again from zero.
    pub fn stop(&self, id: JobId) -> Result<()> {
        let (run, frozen, event) = {
            let mut st = self.inner.lock();
            let job = st.job(id)?;
            match job.state {
                JobState::Probing => bail!("Still reading the video's details."),
                JobState::Queued | JobState::Stopped => return Ok(()),
                _ => {}
            }
            let frozen = job.frozen_dir.clone();
            st.update(id, |j| {
                j.state = JobState::Stopped;
                j.error = None;
                j.chapters_done = 0;
                j.frozen_dir = None;
                Ok(())
            })?;
            let run = st.runs.remove(&id);
            if let Some(run) = &run {
                run.token.cancel();
            }
            (run, frozen, st.updated(id)?)
        };
        self.inner.emit(event);
        join(run);
        // Only after the join: until then the tree may still be writing.
        if let Some(dir) = frozen {
            remove_partials(Path::new(&dir), id);
        }
        Ok(())
    }

    /// Removes rows: kills whatever is running and deletes partial files.
    /// Ids that no longer exist are skipped.
    pub fn remove(&self, ids: &[JobId]) -> Result<()> {
        for &id in ids {
            let (run, frozen) = {
                let mut st = self.inner.lock();
                let Some(job) = st.jobs.get(&id) else { continue };
                let frozen = job.frozen_dir.clone();
                st.conn.execute("DELETE FROM queue WHERE id = ?1", params![id])?;
                st.jobs.remove(&id);
                let run = st.runs.remove(&id);
                if let Some(run) = &run {
                    run.token.cancel();
                }
                (run, frozen)
            };
            self.inner.emit(QueueEvent::Removed { id });
            join(run);
            if let Some(dir) = frozen {
                remove_partials(Path::new(&dir), id);
            }
        }
        Ok(())
    }

    /// Kills every running download and probe and waits for them to be gone.
    /// Persisted states are left untouched: whatever was downloading restores
    /// as paused on the next launch. Call on application exit.
    pub fn shutdown(&self) {
        let runs: Vec<Run> = {
            let mut st = self.inner.lock();
            st.runs.drain().map(|(_, run)| run).collect()
        };
        for run in &runs {
            run.token.cancel();
        }
        for run in runs {
            let _ = run.handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_a_downloading_job() -> State {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO queue (id, url, state, spec, frozen_dir) VALUES (1, 'u', 'downloading', '{}', '/d')",
            [],
        )
        .unwrap();
        let job = Job {
            id: 1,
            url: "u".into(),
            state: JobState::Downloading,
            spec: JobSpec::default(),
            meta: None,
            error: None,
            item_dir: None,
            frozen_dir: Some("/d".into()),
            chapters_done: 0,
        };
        State { conn, jobs: BTreeMap::from([(1, job)]), runs: HashMap::new() }
    }

    #[test]
    fn a_failed_run_keeps_its_folder_pinned_but_a_refused_one_does_not() {
        let mut st = state_with_a_downloading_job();

        st.mark_failed(1, "boom".into());
        let failed = st.job(1).unwrap().view("common");
        assert_eq!((failed.state, failed.error.as_deref()), (JobState::Failed, Some("boom")));
        assert!(failed.locked, "partial files may exist, so quality and folder stay read-only");

        st.mark_refused(1, "exists".into());
        let refused = st.job(1).unwrap().view("common");
        assert_eq!((refused.state, refused.error.as_deref()), (JobState::Failed, Some("exists")));
        assert!(!refused.locked, "nothing was fetched, so the row can be changed and retried");
        assert_eq!(refused.dir, "common");

        let stored: Option<String> =
            st.conn.query_row("SELECT frozen_dir FROM queue WHERE id = 1", [], |r| r.get(0)).unwrap();
        assert_eq!(stored, None, "the unpinned folder is persisted too");
    }

    #[test]
    fn partial_files_mean_a_resume() {
        let dir = std::env::temp_dir().join(format!("ytdlp-gui-queue-test-{}", std::process::id()));
        let temp = partial_dir(&dir, 7);
        assert!(!has_partials(&temp), "no directory yet");

        fs::create_dir_all(&temp).unwrap();
        assert!(!has_partials(&temp), "an empty directory is not a partial download");

        fs::write(temp.join("clip.mp4.part"), b"x").unwrap();
        assert!(has_partials(&temp));

        fs::remove_dir_all(&dir).unwrap();
    }
}
