// The gear panel: yt-dlp update, common download folder, and which entries the
// per-row quality dropdown offers.

import { api } from './api.js';
import { h, renderStatus } from './util.js';

let menu = null;
const menuListeners = new Set();
const dirListeners = new Set();

export const getMenu = () => menu;
export const onMenuChange = cb => menuListeners.add(cb);
export const onDirChange = cb => dirListeners.add(cb);

const TOGGLES = [
  ['Presets', [
    ['show_mp3', 'MP3 Audio'],
    ['show_m4a', 'M4A Audio'],
    ['show_1080p', '1080p Video'],
    ['show_best', 'Best Available'],
  ]],
  ['Download range', [
    ['show_timeline', 'Timeline'],
    ['show_chapters', 'Chapters'],
  ]],
];

const CONTAINERS = ['mp4', 'webm', 'mkv', 'mov', 'flv', '3gp', 'm4a', 'mp3', 'opus', 'ogg', 'aac'];
// Display names of the labels the dropdown shows (see friendlyCodec); the
// stored filter is their lower-case form.
const CODECS = ['H.264', 'AV1', 'VP9', 'VP8', 'HEVC', 'AAC', 'Opus', 'Vorbis', 'MP3', 'Dolby Digital', 'FLAC'];
const MIN_HEIGHTS = [null, 144, 240, 360, 480, 720, 1080, 1440, 2160];

const $ = id => document.getElementById(id);

function check(label, checked, onChange) {
  const box = h('input', { type: 'checkbox', checked, onChange: () => onChange(box.checked) });
  return h('label', { class: 'check chip' }, box, h('span', {}, label));
}

/** Persists `next`; on rejection shows why and redraws from the stored menu. */
async function save(next) {
  const error = $('menu-error');
  try {
    menu = await api.setQualityMenu(next);
    error.textContent = '';
    menuListeners.forEach(cb => cb(menu));
  } catch (e) {
    error.textContent = String(e);
  }
  renderMenu();
}

/** The checkbox means "shown", the stored list means "hidden". */
function toggleHidden(list, value, shown) {
  const rest = list.filter(v => v !== value);
  return shown ? rest : [...rest, value];
}

function renderMenu() {
  const root = $('menu-controls');
  root.replaceChildren();

  for (const [group, items] of TOGGLES) {
    root.append(h('div', { class: 'menu-group' },
      h('div', { class: 'menu-group-title' }, group),
      h('div', { class: 'menu-chips' }, items.map(([key, label]) =>
        check(label, menu[key], on => save({ ...menu, [key]: on }))))));
  }

  root.append(h('div', { class: 'menu-group' },
    h('div', { class: 'menu-group-title' }, 'Show containers'),
    h('div', { class: 'menu-chips' }, CONTAINERS.map(ext =>
      check(ext, !menu.hidden_containers.includes(ext),
        on => save({ ...menu, hidden_containers: toggleHidden(menu.hidden_containers, ext, on) }))))));

  root.append(h('div', { class: 'menu-group' },
    h('div', { class: 'menu-group-title' }, 'Show codecs'),
    h('div', { class: 'menu-chips' }, CODECS.map(name => {
      const key = name.toLowerCase();
      return check(name, !menu.hidden_codecs.includes(key),
        on => save({ ...menu, hidden_codecs: toggleHidden(menu.hidden_codecs, key, on) }));
    }))));

  const select = h('select', {
    id: 'menu-min-height',
    'aria-label': 'Minimum resolution',
    onChange: () => save({ ...menu, min_height: select.value ? Number(select.value) : null }),
  }, MIN_HEIGHTS.map(v =>
    h('option', { value: v ?? '', selected: v === menu.min_height }, v == null ? 'No minimum' : `${v}p`)));
  root.append(h('div', { class: 'menu-group' },
    h('div', { class: 'menu-group-title' }, 'Minimum resolution'),
    select));
}

export async function initSettings() {
  menu = await api.getQualityMenu();
  renderMenu();

  const dialog = $('settings-dialog');
  $('settings-btn').addEventListener('click', () => dialog.showModal());
  $('settings-close').addEventListener('click', () => dialog.close());
  // A click on the backdrop lands on the <dialog> element itself.
  dialog.addEventListener('click', e => { if (e.target === dialog) dialog.close(); });

  // Download folder
  // The element is `direction: rtl` so a long path truncates at its start and
  // keeps the folder name visible. The leading LRM stops the bidi algorithm
  // from moving a leading `/` to the end of the line (`home/u/Videos/`).
  const showDir = path => {
    const el = $('dl-dir-display');
    el.textContent = `‎${path}`;
    el.title = path;
  };
  showDir(await api.getDownloadDir());
  const dirChanged = path => {
    showDir(path);
    renderStatus($('dl-dir-status'), '', '');
    dirListeners.forEach(cb => cb(path));
  };

  $('dl-dir-change-btn').addEventListener('click', async () => {
    try {
      const path = await api.pickDownloadDir();
      if (path) dirChanged(path);
    } catch (e) {
      renderStatus($('dl-dir-status'), String(e), 'err');
    }
  });
  $('dl-dir-reset-btn').addEventListener('click', async () => {
    try {
      dirChanged(await api.resetDownloadDir());
    } catch (e) {
      renderStatus($('dl-dir-status'), String(e), 'err');
    }
  });

  // yt-dlp update
  const btn = $('update-btn');
  const status = $('update-status');
  btn.addEventListener('click', async () => {
    btn.disabled = true;
    renderStatus(status, 'Checking…', 'checking');
    try {
      renderStatus(status, await api.updateYtdlp(), 'ok');
    } catch (e) {
      renderStatus(status, String(e), 'err');
    } finally {
      btn.disabled = false;
    }
  });
}
