// The History tab: completed downloads, with re-download for one or many.

import { api } from './api.js';
import { h, iconEl, describeQuality, describeScope } from './util.js';

const selected = new Set(); // urls
let entries = [];
let onRedownloaded = () => {};
const els = {};

const CONFIRM_MS = 4000;
let armed = false;
let armTimer = null;

function describe(entry) {
  if (!entry.spec) return '';
  return [describeQuality(entry.spec.quality), describeScope(entry.spec.scope)].filter(Boolean).join(' · ');
}

function refreshChrome() {
  const total = entries.length;
  const count = entries.filter(e => selected.has(e.url)).length;
  els.selectAll.disabled = total === 0;
  els.selectAll.checked = total > 0 && count === total;
  els.selectAll.indeterminate = count > 0 && count < total;
  els.redownloadSelected.disabled = count === 0;
  els.redownloadSelected.textContent = count ? `Re-download Selected (${count})` : 'Re-download Selected';
  els.clear.disabled = total === 0;
}

function buildRow(entry) {
  const box = h('input', {
    type: 'checkbox',
    checked: selected.has(entry.url),
    'aria-label': 'Select',
    onChange: () => {
      if (box.checked) selected.add(entry.url); else selected.delete(entry.url);
      refreshChrome();
    },
  });
  const tag = describe(entry);
  return h('div', { class: 'history-item' },
    h('label', { class: 'q-check' }, box),
    h('div', { class: 'history-item-info' },
      h('div', { class: 'title', title: entry.title }, entry.title),
      h('div', { class: 'url', title: entry.url }, entry.url)),
    tag ? h('span', { class: 'history-tag' }, tag) : null,
    h('div', { class: 'actions' },
      h('button', {
        type: 'button', class: 'circle-btn', title: 'Re-download', 'aria-label': 'Re-download',
        onClick: () => redownload([entry.url]),
      }, iconEl('redownload', 16)),
      h('button', {
        type: 'button', class: 'btn-icon-delete', title: 'Remove from history', 'aria-label': 'Remove from history',
        onClick: async () => {
          await api.deleteHistoryEntry(entry.url);
          selected.delete(entry.url);
          loadHistory();
        },
      }, iconEl('xCircle', 16))));
}

function render() {
  els.list.replaceChildren(
    ...(entries.length
      ? entries.map(buildRow)
      : [h('span', { class: 'history-empty' }, 'No downloads recorded yet.')]));
  refreshChrome();
}

export async function loadHistory() {
  entries = await api.getHistory();
  // Drop selections whose entry no longer exists.
  const urls = new Set(entries.map(e => e.url));
  [...selected].forEach(url => { if (!urls.has(url)) selected.delete(url); });
  render();
}

async function redownload(urls) {
  els.note.textContent = '';
  try {
    const jobs = await api.redownload(urls);
    selected.clear();
    onRedownloaded(jobs);
  } catch (e) {
    els.note.textContent = String(e);
  }
}

function disarm() {
  clearTimeout(armTimer);
  armTimer = null;
  armed = false;
  els.clear.textContent = 'Clear History';
  els.clear.classList.remove('btn-danger-armed');
}

export function initHistory(hooks) {
  onRedownloaded = hooks.onRedownloaded;
  const $ = id => document.getElementById(id);
  Object.assign(els, {
    list: $('history-list'), selectAll: $('history-select-all'),
    redownloadSelected: $('history-redownload-selected'), clear: $('clear-history-btn'),
    note: $('history-note'),
  });

  els.selectAll.addEventListener('change', () => {
    entries.forEach(e => { if (els.selectAll.checked) selected.add(e.url); else selected.delete(e.url); });
    render();
  });
  els.redownloadSelected.addEventListener('click', () =>
    redownload(entries.filter(e => selected.has(e.url)).map(e => e.url)));

  // Two clicks: the first arms the button, the second (within a few seconds) clears.
  els.clear.addEventListener('click', async () => {
    if (!armed) {
      armed = true;
      els.clear.textContent = 'Click to confirm';
      els.clear.classList.add('btn-danger-armed');
      armTimer = setTimeout(disarm, CONFIRM_MS);
      return;
    }
    disarm();
    await api.clearHistory();
    selected.clear();
    loadHistory();
  });
}
