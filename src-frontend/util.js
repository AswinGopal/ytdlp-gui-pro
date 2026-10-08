// DOM helper, icons, formatters and small shared helpers.

// ── DOM ──────────────────────────────────────────────────────────────────────

function append(node, children) {
  for (const child of children) {
    if (child == null || child === false) continue;
    if (Array.isArray(child)) append(node, child);
    else node.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
}

/**
 * Builds an element. `class`, `dataset`, `html` (trusted static markup only —
 * never user data) and `onXxx` handlers are special-cased; other keys are set
 * as properties when the element has them, attributes otherwise.
 */
export function h(tag, props, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props ?? {})) {
    if (value == null || value === false) continue;
    if (key === 'class') node.className = value;
    else if (key === 'dataset') Object.assign(node.dataset, value);
    else if (key === 'html') node.innerHTML = value;
    else if (key.startsWith('on') && typeof value === 'function') {
      node.addEventListener(key.slice(2).toLowerCase(), value);
    } else if (key in node) node[key] = value;
    else node.setAttribute(key, value === true ? '' : String(value));
  }
  append(node, children);
  return node;
}

// ── Icons ────────────────────────────────────────────────────────────────────

const PATHS = {
  play: '<polygon points="8,5 19,12 8,19" fill="currentColor" stroke="none"/>',
  pause: '<rect x="6" y="5" width="4" height="14" rx="1" fill="currentColor" stroke="none"/><rect x="14" y="5" width="4" height="14" rx="1" fill="currentColor" stroke="none"/>',
  stop: '<rect x="6" y="6" width="12" height="12" rx="2" fill="currentColor" stroke="none"/>',
  x: '<path d="M18 6L6 18M6 6l12 12"/>',
  xCircle: '<circle cx="12" cy="12" r="10"/><path d="M15 9l-6 6M9 9l6 6"/>',
  folder: '<path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>',
  chevron: '<path d="M6 9l6 6 6-6"/>',
  redownload: '<path d="M21 12a9 9 0 1 1-3-6.7"/><path d="M21 3v6h-6"/>',
  film: '<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M7 4v16M17 4v16M3 9h4M3 15h4M17 9h4M17 15h4"/>',
  note: '<path d="M9 18V5l12-2v13"/><circle cx="6" cy="18" r="3"/><circle cx="18" cy="16" r="3"/>',
  video: '<rect x="2" y="6" width="14" height="12" rx="2"/><path d="M16 10l6-3v10l-6-3z"/>',
  star: '<path d="M12 2l3.09 6.26L22 9.27l-5 4.87 1.18 6.88L12 17.77l-6.18 3.25L7 14.14 2 9.27l6.91-1.01L12 2z"/>',
  muxed: '<circle cx="12" cy="12" r="10"/><polygon points="10,8 17,12 10,16" fill="currentColor" stroke="none"/>',
  videoOnly: '<rect x="3" y="6" width="13" height="12" rx="2"/><path d="M16 10l5-3v10l-5-3z"/>',
  audioOnly: '<path d="M9 17V4l11-2v11"/><circle cx="6" cy="17" r="3"/><circle cx="17" cy="15" r="3"/>',
  ok: '<circle cx="12" cy="12" r="10"/><path d="M7 13l3 3 7-8"/>',
};

/** Static SVG markup for `name`, for use with the `html` prop. */
export function iconHtml(name, size = 16) {
  return `<svg viewBox="0 0 24 24" width="${size}" height="${size}" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${PATHS[name]}</svg>`;
}

export function iconEl(name, size = 16) {
  return h('span', { class: 'icon', html: iconHtml(name, size) });
}

// ── Formatting ───────────────────────────────────────────────────────────────

export function formatBytes(bytes) {
  if (bytes == null) return null;
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let n = bytes, i = 0;
  while (n >= 1024 && i < units.length - 1) { n /= 1024; i++; }
  return (i === 0 ? Math.round(n) : n.toFixed(n < 10 ? 1 : 0)) + ' ' + units[i];
}

export function formatSpeed(bytesPerSec) {
  return bytesPerSec == null ? '' : formatBytes(bytesPerSec) + '/s';
}

export function formatDuration(sec) {
  const total = Math.max(0, Math.round(sec));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  return h > 0
    ? `${h}:${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`
    : `${m}:${String(s).padStart(2, '0')}`;
}

/**
 * `HH:MM:SS` with the seconds truncated, never rounded. The backend accepts an
 * end time of at most `floor(duration)` seconds and names that same value in
 * its error (`seconds_to_hms`). `formatDuration` rounds, so for a 90.7 s video
 * it would show `1:31` — a time the backend then rejects as past the end.
 */
export function formatClockFloor(sec) {
  const total = Math.max(0, Math.floor(sec));
  const pad = n => String(n).padStart(2, '0');
  return `${pad(Math.floor(total / 3600))}:${pad(Math.floor((total % 3600) / 60))}:${pad(total % 60)}`;
}

/** `≈ ` marks every size that is only an estimate. */
export function formatSize(bytes, exact) {
  return bytes == null ? '' : (exact ? '' : '≈ ') + formatBytes(bytes);
}

export const PRESET_NAMES = {
  mp3: 'MP3 Audio',
  m4a: 'M4A Audio',
  '1080p': '1080p Video',
  best: 'Best Available',
};

/** One-line description of a job spec's range, '' for the full video. */
export function describeScope(scope) {
  if (scope.kind === 'timeline') return `Clip ${scope.start}–${scope.end}`;
  if (scope.kind === 'chapters') {
    const n = scope.selected.length;
    return `${n} chapter${n === 1 ? '' : 's'}`;
  }
  return '';
}

export function describeQuality(quality) {
  return quality.kind === 'preset'
    ? PRESET_NAMES[quality.value] ?? quality.value
    : `Custom · ${quality.value.join('+')}`;
}

// ── Inputs ───────────────────────────────────────────────────────────────────

/**
 * Strips non-digit/colon characters live, preserving the cursor position
 * relative to the retained characters.
 */
export function sanitiseTimestamp(el) {
  const pos = el.selectionStart ?? el.value.length;
  const original = el.value;
  const cleaned = original
    .replace(/[^\d:]/g, '')
    .replace(/::+/g, ':')
    .split(':')
    .slice(0, 3)
    .map(seg => seg.slice(0, 2))
    .join(':');
  if (cleaned !== original) {
    const newPos = original.slice(0, pos).replace(/[^\d:]/g, '').length;
    el.value = cleaned;
    el.setSelectionRange?.(newPos, newPos);
  }
}

/**
 * `SS`, `MM:SS` or `HH:MM:SS` as `HH:MM:SS`, by the same rules as the backend's
 * `parse_clock`: one or two digits per segment, hours below 24, minutes and
 * seconds below 60. `null` for anything else, which the caller leaves as typed
 * for the backend to report.
 */
export function normaliseClock(text) {
  const segments = text.trim().split(':');
  if (segments.length > 3 || !segments.every(seg => /^\d{1,2}$/.test(seg))) return null;
  const [hours, minutes, seconds] = [...Array(3 - segments.length).fill(0), ...segments.map(Number)];
  if (hours >= 24 || minutes >= 60 || seconds >= 60) return null;
  return [hours, minutes, seconds].map(n => String(n).padStart(2, '0')).join(':');
}

/** Renders an optional icon plus a message; `type` also themes the colour. */
export function renderStatus(el, msg, type) {
  el.replaceChildren();
  el.className = type;
  if (msg && (type === 'ok' || type === 'err')) {
    el.append(h('span', { class: 'status-icon', html: iconHtml(type === 'ok' ? 'ok' : 'xCircle') }));
  }
  el.append(document.createTextNode(msg));
}
