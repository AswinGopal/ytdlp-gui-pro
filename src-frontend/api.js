// Thin wrapper over the Tauri bridge (`withGlobalTauri` exposes it on window).
// Every backend call the UI makes goes through here, so the full command
// surface is visible in one place.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

export const api = {
  // Queue
  queueList: () => invoke('cmd_queue_list'),
  queueAdd: url => invoke('cmd_queue_add', { url }),
  queueSetSpec: (id, spec) => invoke('cmd_queue_set_spec', { id, spec }),
  queuePickItemDir: id => invoke('cmd_queue_pick_item_dir', { id }),
  queueClearItemDir: id => invoke('cmd_queue_clear_item_dir', { id }),
  queueStart: id => invoke('cmd_queue_start', { id }),
  queuePause: id => invoke('cmd_queue_pause', { id }),
  queueStop: id => invoke('cmd_queue_stop', { id }),
  queueRemove: ids => invoke('cmd_queue_remove', { ids }),
  redownload: urls => invoke('cmd_redownload', { urls }),

  // History
  getHistory: () => invoke('cmd_get_history'),
  deleteHistoryEntry: url => invoke('cmd_delete_history_entry', { url }),
  clearHistory: () => invoke('cmd_clear_history'),

  // Settings
  getQualityMenu: () => invoke('cmd_get_quality_menu'),
  setQualityMenu: menu => invoke('cmd_set_quality_menu', { menu }),
  getDownloadDir: () => invoke('cmd_get_download_dir'),
  pickDownloadDir: () => invoke('cmd_pick_download_dir'),
  resetDownloadDir: () => invoke('cmd_reset_download_dir'),
  updateYtdlp: () => invoke('cmd_update_ytdlp'),
  checkDeno: () => invoke('cmd_check_deno'),
};

/** Subscribes to queue events; resolves once the listener is registered. */
export const onQueueEvent = handler => listen('queue_event', e => handler(e.payload));
