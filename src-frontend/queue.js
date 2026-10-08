// The download queue: one row per job, driven by backend events.
//
// Rows are built once and patched in place, never rebuilt, so an update that
// arrives while the user is typing a timestamp or has a dropdown open does not
// disturb them.

import { api, onQueueEvent } from './api.js';
import {
  h, iconEl, iconHtml, formatBytes, formatSize, formatSpeed, formatClockFloor, normaliseClock,
  sanitiseTimestamp, renderStatus,
} from './util.js';
import {
  openQualityDropdown, openChapterPicker, openMenu,
  isPopoverOpenFor, closePopover, qualityButtonLabel,
} from './dropdown.js';
import { getMenu, onMenuChange, onDirChange } from './settings.js';

const STATE_TEXT = {
  probing: 'Reading details…',
  queued: 'Queued',
  downloading: 'Downloading',
  paused: 'Paused',
  stopped: 'Stopped',
  failed: 'Failed',
};
/** States whose progress is worth showing. */
const LIVE = new Set(['downloading', 'paused']);
const FLASH_MS = 6000;

const rows = new Map();     // job id -> Row
const progress = new Map(); // job id -> latest ProgressInfo (this session only)
const els = {};

// ── Scope drafts ─────────────────────────────────────────────────────────────
// The scope controls hold a *draft* that may be incomplete (Timeline chosen
// but no end time yet). Only a complete draft is sent to the backend.

function draftFromSpec(spec) {
  const s = spec.scope;
  return {
    kind: s.kind,
    start: s.kind === 'timeline' ? s.start : '',
    end: s.kind === 'timeline' ? s.end : '',
    chapters: new Set(s.kind === 'chapters' ? s.selected : []),
  };
}

function scopeFromDraft(d) {
  if (d.kind === 'full') return { kind: 'full' };
  if (d.kind === 'timeline') return { kind: 'timeline', start: d.start.trim(), end: d.end.trim() };
  if (!d.chapters.size) throw new Error('Select at least one chapter.');
  return { kind: 'chapters', selected: [...d.chapters].sort((a, b) => a - b) };
}

function sameScope(a, b) {
  if (a.kind !== b.kind) return false;
  if (a.kind === 'timeline') return a.start === b.start && a.end === b.end;
  if (a.kind === 'chapters') {
    return a.selected.length === b.selected.length && a.selected.every((v, i) => v === b.selected[i]);
  }
  return true;
}

// ── Resuming ─────────────────────────────────────────────────────────────────
// Pause kills the download and resume starts it again. Only a full-video
// download continues from its partial files; a clip or a chapter is fetched by
// ffmpeg, which cannot, so the interrupted file starts over.

/** What a resume starts over, or null when it continues where it stopped. */
function resumeRestarts(job) {
  const { kind } = job.spec.scope;
  if (kind === 'timeline') return 'this clip';
  if (kind === 'chapters') return 'the current chapter';
  return null;
}

/** The note shown on a paused row whose resume starts over, else null. */
function resumeNote(job) {
  const what = resumeRestarts(job);
  if (!what) return null;
  const kept = job.spec.scope.kind === 'chapters' && job.chapters_done > 0
    ? ' Finished chapters are kept.'
    : '';
  return `Resume restarts ${what} from the beginning.${kept}`;
}

/**
 * Tooltip and accessible name of the play / pause button. While downloading,
 * a clip or chapter job names the restart cost before the user pauses, since
 * pausing is the moment that cost is incurred.
 */
function playLabel(job) {
  switch (job.state) {
    case 'downloading': {
      const note = resumeNote(job);
      return note ? `Pause. ${note}` : 'Pause';
    }
    case 'paused': {
      const restarts = resumeRestarts(job);
      return restarts ? `Resume — restarts ${restarts}` : 'Resume';
    }
    case 'failed':
      return 'Retry';
    default:
      return 'Start download';
  }
}

// ── Row ──────────────────────────────────────────────────────────────────────

class Row {
  constructor(job) {
    this.job = job;
    this.busy = false;
    this.flashText = null;
    this.flashTimer = null;
    this.scopeSig = '';
    this.thumbUrl = '';
    this.draft = draftFromSpec(job.spec);
    // Per time box: text typed since it was last saved. Such a box is never
    // overwritten from a backend snapshot. Focus says nothing about this —
    // Enter saves a box and leaves the focus in it.
    this.edited = { start: false, end: false };
    this.build();
    this.render();
  }

  build() {
    this.checkbox = h('input', { type: 'checkbox', 'aria-label': 'Select download', onChange: refreshChrome });
    this.img = h('img', {
      alt: '', class: 'hidden', referrerPolicy: 'no-referrer',
      onError: () => this.img.classList.add('hidden'),
    });

    this.title = h('div', { class: 'q-title' });
    this.stateEl = h('span', { class: 'q-state' });
    this.sizeEl = h('span', { class: 'q-size' });
    this.speedEl = h('span', { class: 'q-speed' });
    this.bar = h('progress', { class: 'q-bar hidden', max: 100, value: 0 });

    this.tsStart = this.timeInput('Start (HH:MM:SS) — empty means the beginning', '00:00:00', 'start');
    // The placeholder becomes the video's length once it is known (see renderScope).
    this.tsEnd = this.timeInput('End (HH:MM:SS)', 'HH:MM:SS', 'end');
    this.tsBox = h('div', { class: 'q-ts hidden' },
      h('label', {}, 'Start', this.tsStart),
      h('label', {}, 'End', this.tsEnd));
    this.chBtn = h('button', { type: 'button', class: 'btn btn-secondary btn-sm hidden', onClick: () => this.onChapters() });
    this.msg = h('div', { class: 'q-msg hidden' });

    this.qualityText = h('span', { class: 'dd-btn-text' });
    this.qualityBtn = h('button', { type: 'button', class: 'dd-btn q-quality', title: 'Quality', onClick: () => this.onQuality() },
      this.qualityText, iconEl('chevron', 14));
    this.scopeSel = h('select', { class: 'q-scope', 'aria-label': 'Download range', onChange: () => this.onScope() });
    this.folderBtn = h('button', { type: 'button', class: 'icon-btn', 'aria-label': 'Download folder', onClick: () => this.onFolder() },
      iconEl('folder', 16));

    this.playIcon = h('span', { class: 'icon' });
    this.playBtn = h('button', { type: 'button', class: 'circle-btn', onClick: () => this.onPlayPause() }, this.playIcon);
    this.stopBtn = h('button', {
      type: 'button', class: 'circle-btn', title: 'Stop and delete partial files', 'aria-label': 'Stop',
      onClick: () => this.run(() => api.queueStop(this.job.id)),
    }, iconEl('stop', 16));
    this.removeBtn = h('button', {
      type: 'button', class: 'icon-btn icon-btn-danger', title: 'Remove from queue', 'aria-label': 'Remove from queue',
      onClick: () => this.onRemove(),
    }, iconEl('x', 16));

    this.root = h('div', { class: 'q-row' },
      h('label', { class: 'q-check' }, this.checkbox),
      h('div', { class: 'q-thumb' }, this.img, h('span', { class: 'q-thumb-ph', html: iconHtml('film', 22) })),
      h('div', { class: 'q-main' },
        this.title,
        h('div', { class: 'q-status' }, this.stateEl, this.sizeEl, this.speedEl),
        this.bar,
        h('div', { class: 'q-detail' }, this.tsBox, this.chBtn),
        this.msg),
      h('div', { class: 'q-controls' }, this.qualityBtn, this.scopeSel, this.folderBtn),
      h('div', { class: 'q-actions' }, this.playBtn, this.stopBtn, this.removeBtn));
  }

  timeInput(label, placeholder, field) {
    return h('input', {
      type: 'text', class: 'ts-input', placeholder, maxLength: 8, title: label, 'aria-label': label, autocomplete: 'off',
      onInput: e => {
        sanitiseTimestamp(e.target);
        this.draft[field] = e.target.value;
        this.edited[field] = true;
        this.clearFlash();
        this.renderMessage();
      },
      // `change` fires on blur and on Enter: the natural "I'm done" signal.
      onChange: e => {
        this.edited[field] = false;
        // The backend formats a box when it saves the range, and that waits
        // for an end time. Formatting here as well means a committed box reads
        // HH:MM:SS right away, saved or not. Text the backend would reject
        // stays as typed.
        const clock = normaliseClock(e.target.value);
        if (clock) {
          e.target.value = clock;
          this.draft[field] = clock;
        }
        return this.commitTimeline();
      },
      // Text typed and then restored fires no `change`, so leaving the box
      // ends the edit as well.
      onBlur: () => { this.edited[field] = false; },
    });
  }

  // ── Rendering ──────────────────────────────────────────────────────────────

  render() {
    const { job } = this;
    const ready = Boolean(job.meta) && job.state !== 'probing';
    const editable = ready && !job.locked;
    const idle = editable && !this.busy;

    this.root.dataset.state = job.state;
    this.title.textContent = job.meta?.title ?? job.url;
    this.title.title = job.url;
    this.renderThumb();
    this.renderStatus();

    this.qualityText.textContent = ready ? qualityButtonLabel(job) : '—';
    this.qualityBtn.disabled = !idle;
    this.renderScope(editable, idle);

    const where = `${job.dir} (${job.custom_dir ? 'this item' : 'common folder'})`;
    this.folderBtn.disabled = job.locked || job.state === 'probing' || this.busy;
    this.folderBtn.classList.toggle('active', job.custom_dir);
    this.folderBtn.title = job.locked ? `Saving to ${where} — stop the download to change` : `Save to ${where}`;

    const downloading = job.state === 'downloading';
    this.playIcon.innerHTML = iconHtml(downloading ? 'pause' : 'play', 16);
    this.playBtn.classList.toggle('active', downloading);
    this.playBtn.disabled = job.state === 'probing' || this.busy;
    const label = playLabel(job);
    this.playBtn.title = label;
    this.playBtn.setAttribute('aria-label', label);
    // `locked` means partial files exist, which is exactly when stop has work to do.
    this.stopBtn.disabled = !job.locked || job.state === 'probing' || this.busy;

    this.renderMessage();
  }

  renderThumb() {
    const url = this.job.meta?.thumbnail ?? '';
    if (url === this.thumbUrl) return;
    this.thumbUrl = url;
    if (url) {
      this.img.src = url;
      this.img.classList.remove('hidden');
    } else {
      this.img.removeAttribute('src');
      this.img.classList.add('hidden');
    }
  }

  renderStatus() {
    const { job } = this;
    const live = LIVE.has(job.state);
    const p = live ? progress.get(job.id) : null;

    let state = STATE_TEXT[job.state];
    if (p) state += ` (${Math.round(p.pct)}%)`;
    const chapter = live ? this.chapterLabel() : null;
    if (chapter) state = `${chapter} · ${state}`;
    this.stateEl.textContent = state;

    let size = '';
    if (p?.downloaded != null) {
      size = formatBytes(p.downloaded);
      if (p.total != null) size += ` / ${p.total_estimated ? '≈ ' : ''}${formatBytes(p.total)}`;
    } else if (job.est_size) {
      size = formatSize(job.est_size.bytes, job.est_size.exact);
    }
    this.sizeEl.textContent = size;
    this.speedEl.textContent = job.state === 'downloading' && p ? formatSpeed(p.speed) : '';

    this.bar.classList.toggle('hidden', !p);
    if (p) this.bar.value = p.pct;
  }

  /** "Chapter 2/5 — Intro" for chapter jobs, null otherwise. */
  chapterLabel() {
    const { job } = this;
    if (job.spec.scope.kind !== 'chapters') return null;
    const selected = job.spec.scope.selected;
    const index = selected[job.chapters_done];
    if (index === undefined) return null;
    const title = job.meta?.chapters[index]?.title;
    return `Chapter ${job.chapters_done + 1}/${selected.length}${title ? ` — ${title}` : ''}`;
  }

  renderScope(editable, idle) {
    const { job, draft } = this;
    const menu = getMenu();
    const options = [['full', 'Full video']];
    if (menu.show_timeline || draft.kind === 'timeline') options.push(['timeline', 'Timeline']);
    if ((menu.show_chapters && job.meta?.chapters.length) || draft.kind === 'chapters') {
      options.push(['chapters', 'Chapters']);
    }

    const signature = options.map(([value]) => value).join();
    if (signature !== this.scopeSig) {
      this.scopeSig = signature;
      this.scopeSel.replaceChildren(...options.map(([value, text]) => h('option', { value }, text)));
    }
    this.scopeSel.value = draft.kind;
    this.scopeSel.disabled = !idle;

    this.tsBox.classList.toggle('hidden', draft.kind !== 'timeline');
    this.chBtn.classList.toggle('hidden', draft.kind !== 'chapters');

    // The latest end time the backend accepts is the video's length cut to
    // whole seconds, shown as the End placeholder. Unknown length: a neutral
    // placeholder, since the backend sets no bound either.
    const duration = job.meta?.duration;
    this.tsEnd.placeholder = Number.isFinite(duration) ? formatClockFloor(duration) : 'HH:MM:SS';

    for (const [input, field] of [[this.tsStart, 'start'], [this.tsEnd, 'end']]) {
      // Not tied to `busy`: disabling an input drops focus, which would break
      // tabbing from one box to the next while a save is in flight.
      input.disabled = !editable;
      // Never overwrite text the user has typed and not yet saved.
      if (!this.edited[field]) input.value = draft[field];
    }
    const total = job.meta?.chapters.length ?? 0;
    this.chBtn.textContent = draft.chapters.size
      ? `${draft.chapters.size} of ${total} chapters ▾`
      : 'Select chapters ▾';
    this.chBtn.disabled = !idle;
  }

  /** What the message line shows: a transient flash, else the job's error, else a hint. */
  renderMessage() {
    const { job, draft } = this;
    let hint = null;
    if (job.state === 'paused') {
      hint = resumeNote(job);
    } else if (job.meta && !job.locked && draft.kind === 'chapters' && !draft.chapters.size) {
      hint = 'Select at least one chapter.';
    }
    const text = this.flashText ?? job.error ?? hint ?? '';
    this.msg.textContent = text;
    this.msg.title = text;
    this.msg.classList.toggle('hidden', !text);
    this.msg.classList.toggle('q-msg-hint', !this.flashText && !job.error && Boolean(hint));
  }

  flash(text) {
    clearTimeout(this.flashTimer);
    this.flashText = text;
    this.flashTimer = setTimeout(() => this.clearFlash(true), FLASH_MS);
    this.renderMessage();
  }

  clearFlash(rerender = false) {
    clearTimeout(this.flashTimer);
    this.flashText = null;
    if (rerender) this.renderMessage();
  }

  /** Applies a fresh snapshot from the backend. */
  update(job) {
    const scopeChanged = !sameScope(this.job.spec.scope, job.spec.scope);
    this.job = job;
    if (scopeChanged) {
      // The backend normalises what was sent (e.g. `1:30` → `00:01:30`), so
      // adopt its version — except in a box holding text typed since it was
      // last saved, which may already be newer than this snapshot.
      const fresh = draftFromSpec(job.spec);
      for (const field of ['start', 'end']) {
        if (this.edited[field]) fresh[field] = this.draft[field];
      }
      this.draft = fresh;
    }
    this.render();
  }

  destroy() {
    clearTimeout(this.flashTimer);
    this.root.remove();
  }

  // ── Actions ────────────────────────────────────────────────────────────────

  /** Runs a backend call with the controls locked, surfacing any error in the row. */
  async run(task) {
    this.busy = true;
    this.clearFlash();
    this.render();
    try {
      await task();
    } catch (e) {
      this.flash(String(e?.message ?? e));
    } finally {
      this.busy = false;
      this.render();
    }
  }

  async commit({ quality, scope }) {
    const spec = {
      quality: quality ?? this.job.spec.quality,
      scope: scope ?? this.job.spec.scope,
    };
    upsert(await api.queueSetSpec(this.job.id, spec));
  }

  onQuality() {
    if (isPopoverOpenFor(this.qualityBtn)) return closePopover();
    openQualityDropdown(this.qualityBtn, this.job, getMenu(), quality =>
      this.run(() => this.commit({ quality })));
  }

  async onScope() {
    this.draft.kind = this.scopeSel.value;
    this.render();
    if (this.draft.kind === 'full') {
      if (this.job.spec.scope.kind !== 'full') {
        await this.run(() => this.commit({ scope: { kind: 'full' } }));
      }
    } else if (this.draft.kind === 'chapters') {
      this.onChapters();
    } else {
      this.tsStart.focus();
    }
  }

  commitTimeline() {
    const { draft, job } = this;
    if (draft.kind !== 'timeline' || !draft.end.trim()) return;
    const saved = job.spec.scope;
    if (saved.kind === 'timeline' && saved.start === draft.start.trim() && saved.end === draft.end.trim()) return;
    return this.run(() => this.commit({ scope: scopeFromDraft(draft) }));
  }

  onChapters() {
    if (isPopoverOpenFor(this.chBtn)) return closePopover();
    openChapterPicker(this.chBtn, this.job.meta.chapters, this.draft.chapters, chosen => {
      this.draft.chapters = chosen;
      this.render();
      if (chosen.size) this.run(() => this.commit({ scope: scopeFromDraft(this.draft) }));
    });
  }

  onFolder() {
    const pick = () => this.run(async () => {
      const job = await api.queuePickItemDir(this.job.id);
      if (job) upsert(job);
    });
    if (!this.job.custom_dir) return pick();
    openMenu(this.folderBtn, [
      { label: 'Choose another folder…', onSelect: pick },
      { label: 'Use the common folder', onSelect: () => this.run(async () => upsert(await api.queueClearItemDir(this.job.id))) },
    ]);
  }

  onPlayPause() {
    const { job } = this;
    return this.run(async () => {
      if (job.state === 'downloading') return api.queuePause(job.id);
      // Make sure what the controls show is what gets downloaded.
      if (job.meta && !job.locked) {
        const want = scopeFromDraft(this.draft);
        if (!sameScope(want, job.spec.scope)) await this.commit({ scope: want });
      }
      return api.queueStart(job.id);
    });
  }

  onRemove() {
    return this.run(async () => {
      await api.queueRemove([this.job.id]);
      dropRow(this.job.id);
    });
  }
}

// ── Queue ────────────────────────────────────────────────────────────────────

/** Adds or refreshes a row from a backend snapshot. */
export function upsert(job) {
  const previous = rows.get(job.id);
  // Progress belongs to one run of one file: drop it when the run ends or the
  // next chapter starts.
  const chapterAdvanced = previous && previous.job.chapters_done !== job.chapters_done;
  if (!LIVE.has(job.state) || chapterAdvanced) progress.delete(job.id);

  if (previous) {
    previous.update(job);
  } else {
    const row = new Row(job);
    const next = [...rows.values()].find(r => r.job.id > job.id);
    els.list.insertBefore(row.root, next ? next.root : null);
    rows.set(job.id, row);
  }
  refreshChrome();
}

export const upsertJobs = jobs => jobs.forEach(upsert);

function dropRow(id) {
  rows.get(id)?.destroy();
  rows.delete(id);
  progress.delete(id);
  refreshChrome();
}

function refreshChrome() {
  const total = rows.size;
  const checked = [...rows.values()].filter(r => r.checkbox.checked).length;
  els.title.textContent = `Download Queue (${total} ${total === 1 ? 'item' : 'items'})`;
  els.empty.classList.toggle('hidden', total > 0);
  els.selectAll.disabled = total === 0;
  els.selectAll.checked = total > 0 && checked === total;
  els.selectAll.indeterminate = checked > 0 && checked < total;
  els.removeBtn.disabled = checked === 0;
  els.removeBtn.textContent = checked ? `Remove Selected (${checked})` : 'Remove Selected';
}

function handleEvent(event) {
  switch (event.kind) {
    case 'added':
      // Every new row arrives through here, from the Add box and from a
      // History re-download alike. Whatever the Add box was complaining about
      // (e.g. "Enter a video URL.") no longer applies once an item is added.
      renderStatus(els.status, '', '');
      upsert(event.job);
      break;
    case 'updated':
      upsert(event.job);
      break;
    case 'progress':
      progress.set(event.id, event.info);
      rows.get(event.id)?.renderStatus();
      break;
    case 'completed': // finished and moved to History
    case 'removed':
      dropRow(event.id);
      break;
  }
}

async function addUrl() {
  const url = els.url.value.trim();
  if (!url) return renderStatus(els.status, 'Enter a video URL.', 'err');
  els.add.disabled = true;
  try {
    upsert(await api.queueAdd(url));
    els.url.value = '';
    els.url.focus();
  } catch (e) {
    renderStatus(els.status, String(e), 'err');
  } finally {
    els.add.disabled = false;
  }
}

async function removeSelected() {
  const ids = [...rows.values()].filter(r => r.checkbox.checked).map(r => r.job.id);
  if (!ids.length) return;
  els.removeBtn.disabled = true;
  try {
    await api.queueRemove(ids);
    ids.forEach(dropRow);
  } catch (e) {
    renderStatus(els.status, String(e), 'err');
  }
  refreshChrome();
}

/** Re-reads the whole queue, e.g. after the common download folder changed. */
export async function reloadQueue() {
  upsertJobs(await api.queueList());
}

export async function initQueue() {
  const $ = id => document.getElementById(id);
  Object.assign(els, {
    list: $('queue-list'), title: $('queue-title'), empty: $('queue-empty'),
    selectAll: $('queue-select-all'), removeBtn: $('remove-selected-btn'),
    url: $('url'), add: $('add-btn'), status: $('status'),
  });

  els.add.addEventListener('click', addUrl);
  els.url.addEventListener('keydown', e => { if (e.key === 'Enter') addUrl(); });
  els.url.addEventListener('input', () => renderStatus(els.status, '', ''));
  els.removeBtn.addEventListener('click', removeSelected);
  els.selectAll.addEventListener('change', () => {
    rows.forEach(r => { r.checkbox.checked = els.selectAll.checked; });
    refreshChrome();
  });

  onMenuChange(() => rows.forEach(r => r.render()));
  onDirChange(() => reloadQueue());

  // Subscribe first, then read the list. An event that arrives in between is
  // newer than the list snapshot, so the list must not overwrite it.
  const touched = new Set();
  await onQueueEvent(event => {
    touched.add(event.job?.id ?? event.id);
    handleEvent(event);
  });
  (await api.queueList()).filter(job => !touched.has(job.id)).forEach(upsert);
  refreshChrome();
}
