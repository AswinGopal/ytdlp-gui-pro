import { api } from './api.js';
import { initSettings } from './settings.js';
import { initQueue, upsertJobs } from './queue.js';
import { initHistory, loadHistory } from './history.js';

const $ = id => document.getElementById(id);

function showTab(name) {
  document.querySelectorAll('nav button').forEach(b => b.classList.toggle('active', b.dataset.tab === name));
  document.querySelectorAll('.panel').forEach(p => p.classList.toggle('active', p.id === `tab-${name}`));
  if (name === 'history') loadHistory();
}

async function main() {
  document.querySelectorAll('nav button').forEach(btn =>
    btn.addEventListener('click', () => showTab(btn.dataset.tab)));

  api.checkDeno().then(ok => {
    if (!ok) $('deno-warning').classList.remove('hidden');
  });

  await initSettings();
  // After a re-download the new rows are the point, so go and show them.
  initHistory({
    onRedownloaded: jobs => {
      upsertJobs(jobs);
      showTab('download');
    },
  });
  await initQueue();
}

main();
