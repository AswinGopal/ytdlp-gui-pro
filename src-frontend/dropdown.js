// Popover primitive plus the three popovers built on it: the per-row quality
// dropdown, the chapter picker and a small action menu.

import {
  h, iconHtml, formatDuration, PRESET_NAMES,
} from './util.js';

// ── Popover primitive ────────────────────────────────────────────────────────

let active = null;

export function closePopover() {
  active?.close();
}

export function isPopoverOpenFor(anchor) {
  return active?.anchor === anchor;
}

/**
 * Shows `content` in a floating panel next to `anchor`. Only one popover is
 * open at a time. It closes on outside click, Escape, resize, or a scroll
 * outside itself; `onClose` runs exactly once whichever way it closes.
 * With `matchWidth` the panel is never narrower than the anchor.
 */
function openPopover(anchor, content, { onClose, className = '', matchWidth = false } = {}) {
  closePopover();
  const pop = h('div', { class: `popover ${className}`.trim(), role: 'dialog' }, content);
  if (matchWidth) pop.style.minWidth = `${anchor.offsetWidth}px`;
  document.body.append(pop);
  anchor.setAttribute('aria-expanded', 'true');

  let closed = false;
  const onPointerDown = e => {
    if (!pop.contains(e.target) && !anchor.contains(e.target)) close();
  };
  const onKeyDown = e => {
    if (e.key === 'Escape') { e.stopPropagation(); close(); }
  };
  const onScroll = e => {
    if (!pop.contains(e.target)) close();
  };

  function close() {
    if (closed) return;
    closed = true;
    document.removeEventListener('pointerdown', onPointerDown, true);
    document.removeEventListener('keydown', onKeyDown, true);
    document.removeEventListener('scroll', onScroll, true);
    window.removeEventListener('resize', close);
    pop.remove();
    anchor.removeAttribute('aria-expanded');
    if (active === handle) active = null;
    onClose?.();
  }

  const handle = { anchor, close, el: pop };
  active = handle;

  document.addEventListener('pointerdown', onPointerDown, true);
  document.addEventListener('keydown', onKeyDown, true);
  document.addEventListener('scroll', onScroll, true);
  window.addEventListener('resize', close);
  place(pop, anchor);
  return handle;
}

/**
 * Room below the anchor under which a panel that does not fit flips above it.
 * With at least this much, it opens below and scrolls inside itself instead,
 * so it does not cover what sits above its anchor.
 */
const MIN_ROOM_BELOW = 240;

/**
 * Below the anchor, capped to the room there (the panel scrolls inside) when
 * it does not fit. Above only when there is less than `MIN_ROOM_BELOW` below
 * and more room above.
 */
function place(pop, anchor) {
  const margin = 8;
  const gap = 4;
  const r = anchor.getBoundingClientRect();
  const { offsetWidth: w, offsetHeight: ht } = pop;
  const room = {
    below: window.innerHeight - r.bottom - gap - margin,
    above: r.top - gap - margin,
  };

  let top = r.bottom + gap;
  if (ht > room.below) {
    if (room.below < MIN_ROOM_BELOW && room.above > room.below) {
      const height = Math.min(ht, room.above);
      pop.style.maxHeight = `${height}px`;
      top = r.top - height - gap;
    } else {
      pop.style.maxHeight = `${Math.max(room.below, 120)}px`;
    }
  }
  const left = Math.max(margin, Math.min(r.left, window.innerWidth - w - margin));
  pop.style.top = `${top}px`;
  pop.style.left = `${left}px`;
}

/** A list of actions: `[{ label, onSelect }]`. */
export function openMenu(anchor, items) {
  const handle = openPopover(
    anchor,
    items.map(({ label, onSelect }) =>
      h('button', {
        type: 'button',
        class: 'dd-item',
        onClick: () => { handle.close(); onSelect(); },
      }, h('span', { class: 'dd-item-name' }, label))),
    { className: 'popover-menu' },
  );
}

// ── Chapter picker ───────────────────────────────────────────────────────────

/**
 * Multi-select of chapter names. `onDone` receives the chosen set of
 * zero-based indices when the picker closes, whether or not it changed.
 */
export function openChapterPicker(anchor, chapters, selected, onDone) {
  const chosen = new Set(selected);
  const boxes = [];
  const count = h('span', { class: 'dd-count' });
  const all = h('input', {
    type: 'checkbox',
    onChange: () => {
      chapters.forEach((_, i) => (all.checked ? chosen.add(i) : chosen.delete(i)));
      boxes.forEach((box, i) => { box.checked = chosen.has(i); });
      sync();
    },
  });

  function sync() {
    all.checked = chosen.size === chapters.length;
    all.indeterminate = chosen.size > 0 && chosen.size < chapters.length;
    count.textContent = `${chosen.size} / ${chapters.length}`;
  }

  const list = chapters.map((c, i) => {
    const box = h('input', {
      type: 'checkbox',
      checked: chosen.has(i),
      onChange: () => { if (box.checked) chosen.add(i); else chosen.delete(i); sync(); },
    });
    boxes.push(box);
    const title = c.title || `Chapter ${i + 1}`;
    return h('label', { class: 'dd-chapter' },
      box,
      h('span', { class: 'dd-chapter-title', title }, title),
      h('span', { class: 'dd-chapter-dur' }, formatDuration(c.end - c.start)));
  });
  sync();

  openPopover(
    anchor,
    [
      h('label', { class: 'dd-chapter dd-chapter-all' }, all, h('span', { class: 'dd-chapter-title' }, 'Select all'), count),
      h('div', { class: 'dd-chapter-list' }, list),
    ],
    { className: 'popover-chapters', onClose: () => onDone(chosen) },
  );
}

// ── Quality dropdown ─────────────────────────────────────────────────────────

const CODEC_NAMES = [
  [/^avc1|^h264/i, 'H.264'],
  [/^av01/i, 'AV1'],
  [/^vp0?9/i, 'VP9'],
  [/^vp8/i, 'VP8'],
  [/^hev1|^hvc1|^h265/i, 'HEVC'],
  [/^mp4a/i, 'AAC'],
  [/^opus/i, 'Opus'],
  [/^vorbis/i, 'Vorbis'],
  [/^mp3/i, 'MP3'],
  [/^e?a?c-?3/i, 'Dolby Digital'],
  [/^flac/i, 'FLAC'],
];

export function friendlyCodec(codec) {
  if (!codec || codec === 'none') return null;
  for (const [re, name] of CODEC_NAMES) {
    if (re.test(codec)) return name;
  }
  return codec;
}

/** 'muxed' | 'video' | 'audio' — matches how the backend classifies streams. */
function streamKind(fmt) {
  const hasVideo = fmt.vcodec && fmt.vcodec !== 'none';
  const hasAudio = fmt.acodec && fmt.acodec !== 'none';
  if (hasVideo && hasAudio) return 'muxed';
  return hasVideo ? 'video' : 'audio';
}

// Audio-only formats are not listed: the presets cover audio, and a video-only
// pick gets matching audio added when it downloads. Combined and video-only
// rows look alike — either way the file has audio — and the codec text says
// which is which (`H.264 / AAC` is combined, `H.264` is video-only).

/**
 * `1080p`, with the frame rate appended only above 30 (`1080p60`) and an `HDR`
 * tag only for HDR streams (`2160p HDR`). Falls back to the reported
 * resolution string when the height is unknown.
 */
function resolutionLabel(fmt) {
  const height = Math.round(fmt.height ?? 0);
  if (!height) return fmt.resolution && fmt.resolution !== 'audio only' ? fmt.resolution : 'Video';
  let label = `${height}p`;
  const fps = Math.round(fmt.fps ?? 0);
  if (fps > 30) label += fps;
  if (fmt.dynamic_range && fmt.dynamic_range !== 'SDR') label += ' HDR';
  return label;
}

/** The video codec of a video-only stream, "video / audio" for a combined one. */
function codecText(fmt) {
  return [friendlyCodec(fmt.vcodec), friendlyCodec(fmt.acodec)].filter(Boolean).join(' / ') || '—';
}

function audioLabel(fmt) {
  const rate = fmt.abr || fmt.tbr;
  return rate ? `${Math.round(rate)} kbps` : 'Audio';
}

/** `resolution · codec · container`, as shown on the row's quality button. */
function streamSummary(fmt) {
  const head = streamKind(fmt) === 'audio' ? audioLabel(fmt) : resolutionLabel(fmt);
  return [head, codecText(fmt), fmt.ext].filter(Boolean).join(' · ');
}

/** Text for the row's quality button. */
export function qualityButtonLabel(job) {
  const q = job.spec.quality;
  if (q.kind === 'preset') return PRESET_NAMES[q.value] ?? q.value;
  const byId = new Map((job.meta?.formats ?? []).map(f => [f.format_id, f]));
  return q.value
    .map(id => {
      const f = byId.get(id);
      return f ? streamSummary(f) : id;
    })
    .join(' + ');
}

/**
 * Applies the gear panel's filters. A format that is currently selected stays
 * visible even if a filter would hide it, so the selection is never invisible.
 * Formats of unknown height are never hidden by the minimum resolution.
 */
function isVisible(fmt, menu, selectedIds) {
  if (selectedIds.includes(fmt.format_id)) return true;
  if (menu.hidden_containers.includes((fmt.ext || '').toLowerCase())) return false;
  const codecs = [fmt.vcodec, fmt.acodec].map(friendlyCodec).filter(Boolean);
  if (codecs.some(c => menu.hidden_codecs.includes(c.toLowerCase()))) return false;
  if (menu.min_height != null && fmt.height != null && fmt.height < menu.min_height) return false;
  return true;
}

/** Whether `a` should replace `b` as the one row for streams that read the same. */
function outranks(a, b, selectedIds) {
  const picked = f => selectedIds.includes(f.format_id);
  if (picked(a) !== picked(b)) return picked(a);
  return (a.tbr ?? 0) > (b.tbr ?? 0);
}

/**
 * The formats the dropdown lists, best first. Streams that would read the same
 * (kind, resolution, codec, container) collapse into one row: the highest
 * bitrate, or the selected stream when it is one of them.
 */
function listedFormats(formats, menu, selectedIds) {
  const rows = new Map();
  for (const fmt of [...formats].reverse()) { // yt-dlp lists worst → best
    if (streamKind(fmt) === 'audio' || !isVisible(fmt, menu, selectedIds)) continue;
    const key = [streamKind(fmt), resolutionLabel(fmt), codecText(fmt), fmt.ext].join('|');
    const kept = rows.get(key);
    if (!kept || outranks(fmt, kept, selectedIds)) rows.set(key, fmt);
  }
  return [...rows.values()];
}

const PRESET_ENTRIES = [
  ['mp3', 'show_mp3', 'note', 'icon-mp3'],
  ['m4a', 'show_m4a', 'note', 'icon-m4a'],
  ['1080p', 'show_1080p', 'video', 'icon-video'],
  ['best', 'show_best', 'star', 'icon-best'],
];

/**
 * Opens the quality dropdown for `job`. `onApply(quality)` receives a backend
 * `Quality` value; every entry applies with a single click. Picking a
 * video-only format selects just that format — the backend adds the audio.
 */
export function openQualityDropdown(anchor, job, menu, onApply) {
  const current = job.spec.quality;
  const currentIds = current.kind === 'formats' ? current.value : [];

  const apply = quality => {
    handle.close();
    onApply(quality);
  };

  const presetItems = PRESET_ENTRIES
    .filter(([, flag]) => menu[flag])
    .map(([name, , iconName, iconClass]) => {
      const selected = current.kind === 'preset' && current.value === name;
      return h('button', {
        type: 'button',
        class: selected ? 'dd-item selected' : 'dd-item',
        'aria-pressed': String(selected),
        onClick: () => (selected ? handle.close() : apply({ kind: 'preset', value: name })),
      },
      h('span', { class: `dd-item-icon ${iconClass}`, html: iconHtml(iconName, 18) }),
      h('span', { class: 'dd-item-name' }, PRESET_NAMES[name]));
    });

  const formatRows = listedFormats(job.meta.formats, menu, currentIds).map(fmt =>
    formatRow(fmt, currentIds.includes(fmt.format_id), id =>
      (currentIds.includes(id) ? handle.close() : apply({ kind: 'formats', value: [id] }))));

  // Presets, then formats, separated by a divider; either group may be absent.
  const groups = [];
  if (presetItems.length) groups.push(group(presetItems));
  if (formatRows.length) {
    groups.push(group(formatRows));
  } else if (job.meta.formats.some(f => streamKind(f) !== 'audio')) {
    groups.push(group([emptyNote()]));
  }
  if (!groups.length) groups.push(group([emptyNote()]));

  const handle = openPopover(anchor, groups, { className: 'popover-quality', matchWidth: true });
}

const emptyNote = () => h('div', { class: 'dd-empty' }, 'Nothing matches your filters');

const group = items => h('div', { class: 'dd-group' }, items);

/** One line, `resolution · codec · container`: the same text the row's button shows. */
function formatRow(fmt, selected, onPick) {
  return h('button', {
    type: 'button',
    class: selected ? 'dd-fmt selected' : 'dd-fmt',
    'aria-pressed': String(selected),
    onClick: () => onPick(String(fmt.format_id)),
  }, streamSummary(fmt));
}
