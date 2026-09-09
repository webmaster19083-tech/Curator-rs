'use strict';

// ---------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------

const el = (sel, root = document) => root.querySelector(sel);

function escapeHtml(str) {
  const d = document.createElement('div');
  d.textContent = str;
  return d.innerHTML;
}

function shuffleArray(arr) {
  for (let i = arr.length - 1; i > 0; i--) {
    const j = Math.floor(Math.random() * (i + 1));
    [arr[i], arr[j]] = [arr[j], arr[i]];
  }
  return arr;
}

function pad4(n) { return String(n).padStart(4, '0'); }

// The clips/videos split, in one place. A video's duration_secs comes from
// server-side ffprobe (see duration.rs) for the grid's type filter below;
// the feed and portrait wall instead measure it live via loadedmetadata
// (see feedBuildItem/pwCheckAndPrepare) since they need an answer before
// server-side backfill may have gotten to a given file, not after.
// Videos under 15s and over 90s both exist on a spectrum "clip" doesn't
// really capture either way, but of the two, folding them into "clip"
// (rather than a third, easy-to-forget bucket, or excluding them from both
// filtered views entirely) is the one that doesn't make content quietly
// vanish from every category — so: clip = anything up to 90s, video =
// anything over. Tighten CLIP_MIN_SECONDS below if a hard 15s floor turns
// out to matter more than that in practice.
// One server-persisted boundary defines Clip versus Video everywhere. The
// fallback only covers a UI paint before settings have loaded.
function clipMaxSeconds() {
  const value = Number(appSettings?.max_clip_length_secs);
  return Number.isFinite(value) && value >= 5 ? value : 60;
}

// Playback modes share this small history. It prevents an obvious immediate
// repeat at mode/session boundaries while allowing reuse once a collection has
// been exhausted. A one-item queue is deliberately the only exception.
const playbackHistory = [];
function preparePlaybackItems(items, shuffle = false) {
  const unique = [];
  const ids = new Set();
  for (const item of items || []) {
    if (!item || ids.has(item.id)) continue;
    ids.add(item.id); unique.push(item);
  }
  if (shuffle && unique.length > 1) shuffleArray(unique);
  const last = playbackHistory.at(-1);
  if (unique.length > 1 && unique[0]?.id === last) [unique[0], unique[1]] = [unique[1], unique[0]];
  return unique;
}
function rememberPlaybackItem(item) {
  if (!item?.id || playbackHistory.at(-1) === item.id) return;
  playbackHistory.push(item.id);
  if (playbackHistory.length > 24) playbackHistory.splice(0, playbackHistory.length - 24);
}

function mediaMatchesTypeFilter(item, typeFilter) {
  if (typeFilter === 'all') return true;
  if (typeFilter === 'image') return item.type === 'image';
  if (item.type !== 'video') return false;
  // Unknown durations stay visible under Videos until metadata is available.
  if (item.duration_secs == null) return typeFilter === 'video';
  return typeFilter === 'clip'
    ? item.duration_secs <= clipMaxSeconds()
    : item.duration_secs > clipMaxSeconds();
}

function reportVideoDuration(video, item) {
  bindVirtualClip(video, item);
  if (item.clip_start_secs != null) return;
  if (item.duration_secs != null || item._durationPending) return;
  video.addEventListener('loadedmetadata', async () => {
    const duration = video.duration;
    if (!Number.isFinite(duration) || duration <= 0 || item._durationPending) return;
    item._durationPending = true;
    try {
      await api(`/api/media/${item.id}/duration`, {method:'PUT', body:JSON.stringify({duration_secs:duration})});
      item.duration_secs = duration;
    } catch (_) {} finally { delete item._durationPending; }
  }, {once:true});
}

// A media item that isn't downloaded yet (downloaded===0 — see the
// "real sources stream live before they're downloaded" feature in
// app.py's populate_placeholders/scan_and_index) has no local file at
// item.filepath at all; item.filepath is a synthetic id, never a real
// path. These two helpers are the ONLY place that distinction should be
// checked — every render site below calls one of these instead of
// building a /library or /api/thumb URL directly, specifically so this
// stays a one-line difference from a real downloaded item rather than a
// visibly different code path (that's the whole point: nothing about
// how it's displayed should reveal which one it is).
function mediaFullSrc(item) {
  return item.downloaded === 0 ? item.origin_url : `/library/${encodeURI(item.playback_filepath || item.filepath)}`;
}
function mediaThumbSrc(item) {
  // No local file to thumbnail yet, so this is the one place a
  // placeholder is slightly more expensive than a real item: the grid
  // loads the actual full-size remote image instead of a small cached
  // JPEG. Once scan_and_index retires this row it gets the real, fast
  // thumbnail like everything else automatically.
  return item.downloaded === 0 ? item.origin_url : `/api/thumb/${item.id}`;
}

let toastTimer = null;
function toast(msg, isError = false) {
  const t = el('#toast');
  t.textContent = msg;
  t.classList.toggle('toast-error', isError);
  t.hidden = false;
  requestAnimationFrame(() => t.classList.add('show'));
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    t.classList.remove('show');
    setTimeout(() => { t.hidden = true; }, 200);
  }, 3200);
}

async function api(path, opts = {}) {
  const res = await fetch(path, {
    headers: { 'Content-Type': 'application/json' },
    ...opts,
  });
  if (!res.ok) {
    let msg = res.statusText;
    try { const j = await res.json(); msg = j.detail || j.error || msg; } catch (_) { /* ignore */ }
    const error = new Error(msg); error.status = res.status; throw error;
  }
  return res.json();
}

// ---------------------------------------------------------------------
// application state
// ---------------------------------------------------------------------

const state = {
  sources: [],
  sourcesById: {},
  groups: [],
  groupsById: {},
  collapsedGroups: new Set(),
  view: { type: 'all' },   // {type:'all'} | {type:'creator', id} | {type:'group', id, name}
  currentItems: [],
  typeFilter: 'all',       // 'all' | 'image' | 'clip' | 'video' — applied client-side in loadView(), see mediaMatchesTypeFilter
  sortOrder: 'default',    // one of _MEDIA_SORT_ORDERS' keys server-side
  tagFilter: '',           // tag name, or '' for no filter
  maxRatingFilter: '',     // '' for no filter, else '1'..'4' — hide rating > this (0/unrated always shown)
  downloadsPaused: false,
  lightboxIndex: -1,
  page: 0,
  PAGE_SIZE: 150,
  pollTimer: null,
};

let sourceMenuOpenFor = null;

const ss = {
  active: false,
  items: [],
  index: 0,
  playing: true,
  speed: 3000,
  loop: true,
  shuffleMode: false,
  timer: null,
  videoEl: null,
};

let appSettings = {
  max_concurrent: 6,
  max_clip_length_secs: 60,
  default_slideshow_speed: 3000,
  default_slideshow_loop: true,
  default_slideshow_shuffle: false,
  start_with_windows: false,
  keep_running_in_tray: true,
  theme: 'system',
};

// ---------------------------------------------------------------------
// bootstrap
// ---------------------------------------------------------------------

document.addEventListener('DOMContentLoaded', init);

async function init() {
  bindGlobalUI();
  await loadAppSettings();
  renderExportReminderBanner();
  refreshTagIndex();
  refreshPauseButtonFromServer();
  vrCheckSupport();
  try {
    await refreshGroups();
    await refreshSources();
  } catch (e) {
    toast('Could not reach the Curator server: ' + e.message, true);
  }
  await loadView();
  maybeStartPolling();
}

async function loadAppSettings() {
  try {
    const data = await api('/api/settings');
    appSettings = { ...appSettings, ...data };
  } catch (e) {
    // Not fatal — falls back to the in-memory defaults above.
  }
  appSettings.theme = normalizeTheme(appSettings.theme);
  applyTheme(appSettings.theme);
  configureClipLengthControls();

  // Seed both the live slideshow state (used directly by the portrait
  // wall, which may never touch the slideshow's own controls) and the
  // controls themselves, so the very first slideshow/portrait-wall of the
  // session already reflects the saved defaults. Once the user changes a
  // control mid-session, it keeps that value until the page reloads —
  // only these saved defaults, not live session tweaks, get persisted.
  ss.speed = appSettings.default_slideshow_speed;
  ss.loop = appSettings.default_slideshow_loop;
  ss.shuffleMode = appSettings.default_slideshow_shuffle;
  el('#ss-speed').value = String(appSettings.default_slideshow_speed);
  el('#ss-loop').checked = appSettings.default_slideshow_loop;
  el('#ss-shuffle').checked = appSettings.default_slideshow_shuffle;
}

function configureClipLengthControls() {
  const select = el('#clip-seconds');
  if (!select) return;
  const maximum = clipMaxSeconds();
  const values = [...new Set([5, 15, 30, 45, 60, maximum].filter((value) => value >= 5 && value <= maximum))]
    .sort((a, b) => a - b);
  const previous = Number(select.value);
  select.replaceChildren(...values.map((value) => new Option(`${value}s`, String(value), false, value === Math.min(previous || maximum, maximum))));
  const clipsButton = el('#create-clips-btn');
  if (clipsButton) clipsButton.title = `Create virtual clips up to ${maximum} seconds. The original video is preserved.`;
}

// "system" isn't a real palette — it resolves to the OS's own light/dark
// preference, live-updating if that preference changes while the app is
// open (e.g. the OS auto-switches at sunset).
let systemThemeMedia = null;
const LEGACY_THEME_MAP = {
  yotsuba: 'linen', 'yotsuba-b': 'midnight', futaba: 'ember', burichan: 'midnight',
  tomorrow: 'linen', photon: 'linen', light: 'linen', 'oled-dark': 'oled', dark: 'atelier-dark',
};
function normalizeTheme(theme) {
  return LEGACY_THEME_MAP[theme] || theme;
}

function applyTheme(theme) {
  theme = normalizeTheme(theme);
  if (systemThemeMedia) {
    systemThemeMedia.onchange = null;
    systemThemeMedia = null;
  }
  if (theme === 'system') {
    systemThemeMedia = window.matchMedia('(prefers-color-scheme: light)');
    const resolve = () => {
      if (systemThemeMedia.matches) document.documentElement.dataset.theme = 'linen';
      else delete document.documentElement.dataset.theme; // dark = the base palette, no override needed
    };
    resolve();
    systemThemeMedia.onchange = resolve;
  } else if (theme === 'atelier-dark') {
    delete document.documentElement.dataset.theme;
  } else {
    document.documentElement.dataset.theme = theme;
  }
}

function bindGlobalUI() {
  el('#sidebar-open-btn').addEventListener('click', openSidebarDrawer);
  el('#sidebar-close-btn').addEventListener('click', closeSidebarDrawer);
  el('#sidebar-backdrop').addEventListener('click', closeSidebarDrawer);

  el('#add-source-btn').addEventListener('click', openAddModal);
  el('#quick-add-btn').addEventListener('click', submitQuickAdd);
  el('#quick-add-input').addEventListener('input', checkQuickAddDuplicate);
  el('#quick-add-input').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') { e.preventDefault(); submitQuickAdd(); }
  });
  el('#add-cancel').addEventListener('click', closeAddModal);
  el('#add-confirm').addEventListener('click', submitAddSources);
  el('#add-modal').addEventListener('click', (e) => { if (e.target.id === 'add-modal') closeAddModal(); });
  el('#add-textarea').addEventListener('input', checkBulkAddDuplicates);

  el('#resync-all-btn').addEventListener('click', resyncAllSources);
  el('#pause-downloads-btn').addEventListener('click', togglePauseDownloads);

  el('#select-all-btn').addEventListener('click', () => bulkSetIncluded(true));
  el('#select-none-btn').addEventListener('click', () => bulkSetIncluded(false));
  el('#new-group-btn').addEventListener('click', () => createGroup());

  document.addEventListener('click', (e) => {
    const menu = el('#source-menu');
    if (!menu.hidden && !menu.contains(e.target) && !e.target.closest('.source-menu-btn')) {
      closeSourceMenu();
    }
  });

  el('#settings-btn').addEventListener('click', openSettingsModal);
  el('#settings-cancel').addEventListener('click', closeSettingsModal);
  el('#settings-save').addEventListener('click', saveSettings);
  el('#settings-modal').addEventListener('click', (e) => { if (e.target.id === 'settings-modal') closeSettingsModal(); });
  el('#settings-run-setup-again').addEventListener('click', runSetupAgain);

  el('#export-btn').addEventListener('click', exportSources);
  el('#import-trigger-btn').addEventListener('click', triggerImportPicker);
  el('#import-file-input').addEventListener('change', handleImportFile);
  el('#export-reminder-export-btn').addEventListener('click', exportSources);
  el('#export-reminder-snooze-btn').addEventListener('click', snoozeExportReminder);

  el('.tab[data-view="all"]').addEventListener('click', () => switchView({ type: 'all' }));

  el('#shuffle-btn').addEventListener('click', shuffleCurrentGrid);
  el('#sort-select').addEventListener('change', (e) => {
    gridShuffleSeed = null;
    state.sortOrder = e.target.value;
    loadView();
  });
  el('#tag-filter-select').addEventListener('change', (e) => {
    state.tagFilter = e.target.value;
    loadView();
  });
  el('#max-rating-select').addEventListener('change', (e) => {
    state.maxRatingFilter = e.target.value;
    loadView();
  });
  document.querySelectorAll('.type-filter-btn').forEach((btn) => {
    btn.addEventListener('click', () => {
      state.typeFilter = btn.dataset.typeFilter;
      document.querySelectorAll('.type-filter-btn').forEach((b) => {
        b.classList.toggle('active', b === btn);
      });
      loadView();
    });
  });
  el('#slideshow-btn').addEventListener('click', () => startSlideshow(0));
  el('#portrait-wall-btn').addEventListener('click', startPortraitWall);
  el('#pw-close').addEventListener('click', exitPortraitWall);
  el('#pw-fullscreen').addEventListener('click', () => toggleFullscreen(el('#portrait-wall')));
  el('#portrait-wall').addEventListener('click', (e) => {
    if (e.target.id === 'portrait-wall') exitPortraitWall();
  });
  el('#feed-btn').addEventListener('click', () => startFeed());
  el('#review-ratings-btn').addEventListener('click', () => startFeed(true));
  el('#rating-status-select').addEventListener('change', e => { state.ratingStatus = e.target.value; loadView(); });
  document.addEventListener('visibilitychange', () => { if (document.visibilityState === 'visible') feedAcquireWakeLock(); else feedReleaseWakeLock(); });
  el('#vr-btn').addEventListener('click', startVRMode);
  el('#vr-exit').addEventListener('click', exitVRMode);
  el('#feed-close').addEventListener('click', exitFeed);
  el('#lightbox-tag-add-btn').addEventListener('click', () => {
    const item = state.currentItems[state.lightboxIndex];
    if (!item) return;
    const input = el('#lightbox-tag-input');
    addTagToMedia(item, input.value);
    input.value = '';
  });
  el('#lightbox-tag-input').addEventListener('keydown', (e) => {
    if (e.key !== 'Enter') return;
    const item = state.currentItems[state.lightboxIndex];
    if (!item) return;
    addTagToMedia(item, e.target.value);
    e.target.value = '';
  });
  el('#refresh-banner').addEventListener('click', () => { el('#refresh-banner').hidden = true; loadView(); });

  el('#lightbox-close').addEventListener('click', closeLightbox);
  el('#lightbox-prev').addEventListener('click', () => stepLightbox(-1));
  el('#lightbox-next').addEventListener('click', () => stepLightbox(1));
  el('#create-clips-btn').addEventListener('click', createVideoClips);
  const activeClipJob = localStorage.getItem('curatorClipJob');
  if (activeClipJob) watchClipJob(Number(activeClipJob));
  el('#lightbox-start-slideshow').addEventListener('click', () => startSlideshow(state.lightboxIndex));
  el('#lightbox').addEventListener('click', (e) => { if (e.target.id === 'lightbox') closeLightbox(); });

  el('#ss-prev').addEventListener('click', () => ssStep(-1));
  el('#ss-next').addEventListener('click', () => ssStep(1));
  el('#ss-playpause').addEventListener('click', ssTogglePlay);
  el('#ss-fullscreen').addEventListener('click', toggleFullscreen);
  el('#ss-exit').addEventListener('click', exitSlideshow);
  el('#slideshow-close').addEventListener('click', exitSlideshow);
  el('#ss-speed').addEventListener('change', (e) => {
    ss.speed = parseInt(e.target.value, 10);
    restartImageTimerIfNeeded();
  });
  el('#ss-loop').addEventListener('change', (e) => { ss.loop = e.target.checked; });
  el('#ss-shuffle').addEventListener('change', (e) => {
    ss.shuffleMode = e.target.checked;
    reshuffleSlideshowInPlace();
  });

  enableSwipeNav(el('#lightbox-stage'), () => stepLightbox(-1), () => stepLightbox(1));
  enableSwipeNav(el('#slideshow-stage'), () => ssStep(-1), () => ssStep(1));

  document.addEventListener('fullscreenchange', onFullscreenChange);
  document.addEventListener('webkitfullscreenchange', onFullscreenChange);
  el('#slideshow').addEventListener('mousemove', showFullscreenControls);
  el('#slideshow').addEventListener('touchstart', showFullscreenControls, { passive: true });
  document.addEventListener('keydown', onKeydown);

  const io = new IntersectionObserver((entries) => {
    if (entries.some((e) => e.isIntersecting)) renderNextPage();
  });
  io.observe(el('#grid-sentinel'));
}

// ---------------------------------------------------------------------
// sources (sidebar)
// ---------------------------------------------------------------------

async function refreshSources() {
  const data = await api('/api/sources');
  state.sources = data.sources;
  state.sourcesById = Object.fromEntries(data.sources.map((s) => [s.id, s]));
  renderSidebar();
  updateDownloadBar();
}

// Tracks whichever sources are actively downloading right now as a single
// "batch", so the bar reflects real progress (X of Y finished) instead of
// just a vague spinner, and cleanly resets once everything's caught up.
const downloadBatch = { ids: new Set(), doneIds: new Set() };

function updateDownloadBar() {
  const bar = el('#download-bar');
  const fill = el('#download-bar-fill');
  const label = el('#download-bar-label');

  const activeIds = state.sources
    .filter((s) => s.status === 'pending' || s.status === 'downloading')
    .map((s) => s.id);

  if (activeIds.length === 0) {
    downloadBatch.ids.clear();
    downloadBatch.doneIds.clear();
    bar.hidden = true;
    label.hidden = true;
    return;
  }

  for (const id of activeIds) downloadBatch.ids.add(id);
  for (const id of downloadBatch.ids) {
    const s = state.sourcesById[id];
    if (s && s.status !== 'pending' && s.status !== 'downloading') downloadBatch.doneIds.add(id);
  }

  const total = downloadBatch.ids.size;
  const done = downloadBatch.doneIds.size;
  const pct = total > 0 ? Math.round((done / total) * 100) : 0;

  bar.hidden = false;
  label.hidden = false;
  fill.style.width = pct + '%';
  label.innerHTML = `<span class="status-dot status-downloading"></span>syncing ${done}/${total}`;
}

async function refreshGroups() {
  const data = await api('/api/groups');
  state.groups = data.groups;
  state.groupsById = Object.fromEntries(data.groups.map((g) => [g.id, g]));
}

function renderSidebar() {
  const list = el('#source-list');
  list.innerHTML = '';

  if (state.sources.length === 0 && state.groups.length === 0) {
    list.innerHTML = '<li class="source-empty muted small">No sources yet. Add a creator URL to begin.</li>';
    updateStatsLine();
    return;
  }

  if (state.groups.length === 0) {
    // nobody has created a group yet — keep the sidebar exactly as flat as before
    for (const s of state.sources) list.appendChild(buildSourceRow(s, 0));
    updateStatsLine();
    return;
  }

  const sourcesByGroup = new Map(state.groups.map((g) => [g.id, []]));
  const ungrouped = [];
  for (const s of state.sources) {
    if (s.group_id && sourcesByGroup.has(s.group_id)) sourcesByGroup.get(s.group_id).push(s);
    else ungrouped.push(s);
  }

  const childrenByParent = new Map(); // parent_id (or null for top-level) -> [group, ...]
  for (const g of state.groups) {
    const key = g.parent_id || null;
    if (!childrenByParent.has(key)) childrenByParent.set(key, []);
    childrenByParent.get(key).push(g);
  }

  list.appendChild(buildGroupHeader(null, 'Ungrouped', ungrouped, 0, []));
  if (!state.collapsedGroups.has('ungrouped')) {
    for (const s of ungrouped) list.appendChild(buildSourceRow(s, 1));
  }

  function renderGroupTree(groupId, depth) {
    const g = state.groupsById[groupId];
    if (!g) return;
    const members = sourcesByGroup.get(groupId) || [];
    list.appendChild(buildGroupHeader(groupId, g.name, members, depth, g.tags || []));
    if (state.collapsedGroups.has(String(groupId))) return;
    for (const s of members) list.appendChild(buildSourceRow(s, depth + 1));
    for (const child of childrenByParent.get(groupId) || []) renderGroupTree(child.id, depth + 1);
  }

  for (const g of childrenByParent.get(null) || []) renderGroupTree(g.id, 0);

  updateStatsLine();
}

function updateStatsLine() {
  const totalFrames = state.sources.reduce((a, s) => a + s.item_count, 0);
  el('#stats-line').textContent =
    `${state.sources.length} source${state.sources.length === 1 ? '' : 's'} · ${totalFrames} frame${totalFrames === 1 ? '' : 's'}`;
}

function buildSourceRow(s, depth) {
  const li = document.createElement('li');
  li.className = 'source-item'
    + (depth > 0 ? ' in-group' : '')
    + (state.view.type === 'creator' && state.view.id === s.id ? ' active' : '');
  li.dataset.id = s.id;
  if (depth > 0) li.style.marginLeft = (depth * 14) + 'px';

  const thumb = s.thumbnail_id
    ? `<img class="source-thumb" src="/api/thumb/${s.thumbnail_id}" loading="lazy" alt="">`
    : '<div class="source-thumb source-thumb-empty">◧</div>';

  const metaText = s.status === 'error'
    ? 'error'
    : s.status === 'paused'
    ? 'paused'
    : (s.status === 'downloading' ? 'syncing…' : `${s.item_count} frame${s.item_count === 1 ? '' : 's'}`);

  li.innerHTML = `
    <input type="checkbox" class="source-check" ${s.included ? 'checked' : ''} title="include in Selected view">
    ${thumb}
    <div class="source-info">
      <div class="source-name" contenteditable="false" spellcheck="false" title="double-click to rename">${escapeHtml(s.name)}</div>
      <div class="source-meta mono small">
        <span class="status-dot status-${s.status}"></span>${metaText}
      </div>
    </div>
    <div class="source-actions">
      <button class="icon-btn source-menu-btn" title="More actions">⋯</button>
    </div>
  `;

  if (s.status === 'error' && s.error_message) {
    li.title = s.error_message.slice(0, 400);
  }

  li.querySelector('.source-check').addEventListener('change', (e) => {
    e.stopPropagation();
    setIncluded(s.id, e.target.checked);
  });

  li.querySelector('.source-menu-btn').addEventListener('click', (e) => {
    e.stopPropagation();
    toggleSourceMenu(s.id, e.currentTarget);
  });

  const nameEl = li.querySelector('.source-name');
  nameEl.addEventListener('click', (e) => e.stopPropagation());
  nameEl.addEventListener('dblclick', (e) => {
    e.stopPropagation();
    nameEl.contentEditable = 'true';
    nameEl.focus();
    document.execCommand('selectAll', false, null);
  });
  nameEl.addEventListener('blur', () => {
    nameEl.contentEditable = 'false';
    const newName = nameEl.textContent.trim();
    if (newName && newName !== s.name) renameSource(s.id, newName);
    else nameEl.textContent = s.name;
  });
  nameEl.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') { e.preventDefault(); nameEl.blur(); }
    if (e.key === 'Escape') { nameEl.textContent = s.name; nameEl.blur(); }
  });

  li.addEventListener('click', () => switchView({ type: 'creator', id: s.id }));
  return li;
}

function buildGroupHeader(groupId, name, members, depth, tags) {
  const key = groupId === null ? 'ungrouped' : String(groupId);
  const isActive = state.view.type === 'group'
    && ((groupId === null && state.view.id === 0) || state.view.id === groupId);

  const li = document.createElement('li');
  li.className = 'group-header'
    + (depth > 0 ? ' in-group' : '')
    + (state.collapsedGroups.has(key) ? ' collapsed' : '')
    + (isActive ? ' active' : '');
  li.dataset.groupKey = key;
  if (depth > 0) li.style.marginLeft = (depth * 14) + 'px';

  li.innerHTML = `
    <button class="group-toggle">▾</button>
    <span class="group-title">
      <span class="group-name"${groupId !== null ? ' contenteditable="false" spellcheck="false" title="double-click to rename"' : ''}>${escapeHtml(name)}</span>
      <span class="group-count">(${members.length})</span>
    </span>
    <div class="group-actions">
      ${groupId !== null ? `
        <button class="icon-btn group-tag-btn" title="Add tag">🏷</button>
        <button class="icon-btn group-add-sub-btn" title="Add subgroup">+</button>
        <button class="icon-btn group-rename-btn" title="Rename group">✎</button>
        <button class="icon-btn group-delete-btn" title="Delete group">✕</button>
      ` : ''}
    </div>
  `;

  if (groupId !== null && tags && tags.length) {
    const tagRow = document.createElement('div');
    tagRow.className = 'tag-row group-tag-row';
    tags.forEach((tagName) => {
      tagRow.appendChild(buildTagChip(tagName, () => removeTagFromGroup(groupId, tagName)));
    });
    li.appendChild(tagRow);
  }

  li.querySelector('.group-toggle').addEventListener('click', (e) => {
    e.stopPropagation();
    toggleGroupCollapse(key);
  });

  li.addEventListener('click', () => {
    if (groupId === null) switchView({ type: 'group', id: 0, name: 'Ungrouped' });
    else switchView({ type: 'group', id: groupId, name });
  });

  const tagBtn = li.querySelector('.group-tag-btn');
  if (tagBtn) {
    tagBtn.addEventListener('click', (e) => {
      e.stopPropagation();
      const name = window.prompt('Add tag to this group:');
      if (name && name.trim()) addTagToGroup(groupId, name);
    });
  }

  if (groupId !== null) {
    const nameEl = li.querySelector('.group-name');
    const startEditing = (e) => {
      e.stopPropagation();
      nameEl.contentEditable = 'true';
      nameEl.focus();
      document.execCommand('selectAll', false, null);
    };
    nameEl.addEventListener('click', (e) => e.stopPropagation());
    nameEl.addEventListener('dblclick', startEditing);
    nameEl.addEventListener('blur', () => {
      nameEl.contentEditable = 'false';
      const newName = nameEl.textContent.trim();
      if (newName && newName !== name) renameGroup(groupId, newName);
      else nameEl.textContent = name;
    });
    nameEl.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') { e.preventDefault(); nameEl.blur(); }
      if (e.key === 'Escape') { nameEl.textContent = name; nameEl.blur(); }
    });

    li.querySelector('.group-add-sub-btn').addEventListener('click', (e) => {
      e.stopPropagation();
      state.collapsedGroups.delete(key); // so the new subgroup is visible right away
      createGroup(undefined, groupId);
    });
    li.querySelector('.group-rename-btn').addEventListener('click', startEditing);
    li.querySelector('.group-delete-btn').addEventListener('click', (e) => {
      e.stopPropagation();
      deleteGroup(groupId, name);
    });
  }

  return li;
}

function toggleGroupCollapse(key) {
  if (state.collapsedGroups.has(key)) state.collapsedGroups.delete(key);
  else state.collapsedGroups.add(key);
  renderSidebar();
}

async function createGroup(promptedName, parentId) {
  const name = promptedName !== undefined ? promptedName : prompt('New group name:');
  if (!name || !name.trim()) return null;
  try {
    const body = { name: name.trim() };
    if (parentId) body.parent_id = parentId;
    const g = await api('/api/groups', { method: 'POST', body: JSON.stringify(body) });
    await refreshGroups();
    renderSidebar();
    return g;
  } catch (e) {
    toast('Could not create group: ' + e.message, true);
    return null;
  }
}

async function renameGroup(groupId, name) {
  try {
    await api(`/api/groups/${groupId}`, { method: 'PATCH', body: JSON.stringify({ name }) });
    await refreshGroups();
    renderSidebar();
    if (state.view.type === 'group' && state.view.id === groupId) {
      el('#context-tab-label').textContent = name;
    }
  } catch (e) {
    toast('Rename failed: ' + e.message, true);
    await refreshGroups();
    renderSidebar();
  }
}

async function deleteGroup(groupId, name) {
  if (!confirm(`Delete group "${name}"? Its sources become ungrouped — nothing is deleted.`)) return;
  try {
    await api(`/api/groups/${groupId}`, { method: 'DELETE' });
    await refreshGroups();
    await refreshSources();
    if (state.view.type === 'group' && state.view.id === groupId) {
      switchView({ type: 'all' });
    }
  } catch (e) {
    toast('Could not delete group: ' + e.message, true);
  }
}

async function setSourceGroup(sourceId, groupId) {
  try {
    await api(`/api/sources/${sourceId}/group`, { method: 'PATCH', body: JSON.stringify({ group_id: groupId }) });
    await refreshSources();
    if (state.view.type === 'group') loadView(); // membership of the viewed group may have changed
  } catch (e) {
    toast('Could not move source: ' + e.message, true);
  }
}

// ---------------------------------------------------------------------
// per-source action menu (open page / re-sync / move to group / remove)
// ---------------------------------------------------------------------

function toggleSourceMenu(sourceId, anchorEl) {
  if (sourceMenuOpenFor === sourceId) { closeSourceMenu(); return; }
  openSourceMenu(sourceId, anchorEl);
}

function flattenGroupsForSelect() {
  const childrenByParent = new Map();
  for (const g of state.groups) {
    const key = g.parent_id || null;
    if (!childrenByParent.has(key)) childrenByParent.set(key, []);
    childrenByParent.get(key).push(g);
  }
  const out = [];
  function walk(parentKey, depth) {
    for (const g of childrenByParent.get(parentKey) || []) {
      out.push({ id: g.id, label: (depth > 0 ? '—'.repeat(depth) + ' ' : '') + g.name });
      walk(g.id, depth + 1);
    }
  }
  walk(null, 0);
  return out;
}

function openSourceMenu(sourceId, anchorEl) {
  const s = state.sourcesById[sourceId];
  if (!s) return;
  sourceMenuOpenFor = sourceId;
  const menu = el('#source-menu');

  const groupOptionsHtml = flattenGroupsForSelect().map(({ id, label }) =>
    `<option value="${id}" ${s.group_id === id ? 'selected' : ''}>${escapeHtml(label)}</option>`
  ).join('');

  menu.innerHTML = `
    <button class="source-menu-item" data-action="open">↗ open original page</button>
    <button class="source-menu-item" data-action="resync">⟳ re-sync</button>
    <div class="source-menu-divider"></div>
    <div class="source-menu-group">
      <span>group</span>
      <select class="source-menu-group-select">
        <option value="" ${!s.group_id ? 'selected' : ''}>Ungrouped</option>
        ${groupOptionsHtml}
        <option value="__new__">+ new group…</option>
      </select>
    </div>
    <div class="source-menu-divider"></div>
    <button class="source-menu-item danger" data-action="remove">✕ remove</button>
  `;

  menu.querySelector('[data-action="open"]').addEventListener('click', () => {
    closeSourceMenu();
    window.open(s.url, '_blank', 'noopener,noreferrer');
  });
  menu.querySelector('[data-action="resync"]').addEventListener('click', () => {
    closeSourceMenu();
    resyncSource(sourceId);
  });
  menu.querySelector('[data-action="remove"]').addEventListener('click', () => {
    closeSourceMenu();
    removeSource(sourceId, s.name);
  });
  menu.querySelector('.source-menu-group-select').addEventListener('change', async (e) => {
    const val = e.target.value;
    closeSourceMenu();
    if (val === '__new__') {
      const g = await createGroup();
      if (g) await setSourceGroup(sourceId, g.id);
    } else {
      await setSourceGroup(sourceId, val ? parseInt(val, 10) : null);
    }
  });

  menu.hidden = false;
  const rect = anchorEl.getBoundingClientRect();
  const top = Math.min(rect.bottom + 4, window.innerHeight - menu.offsetHeight - 8);
  const left = Math.min(rect.left, window.innerWidth - menu.offsetWidth - 8);
  menu.style.top = Math.max(8, top) + 'px';
  menu.style.left = Math.max(8, left) + 'px';
}

function closeSourceMenu() {
  el('#source-menu').hidden = true;
  sourceMenuOpenFor = null;
}

function normalizeForCompare(raw) {
  let u = (raw || '').trim();
  if (!u) return '';
  if (!u.includes('://')) u = 'https://' + u;
  return u.replace(/\/+$/, '').toLowerCase();
}

function findDuplicateSource(rawUrl) {
  const key = normalizeForCompare(rawUrl);
  if (!key) return null;
  return state.sources.find((s) => normalizeForCompare(s.url) === key) || null;
}

function openAddModal() {
  el('#add-modal').hidden = false;
  el('#add-textarea').value = '';
  el('#add-textarea-warning').hidden = true;
  el('#add-textarea').focus();
}
function closeAddModal() { el('#add-modal').hidden = true; }

function checkQuickAddDuplicate() {
  const input = el('#quick-add-input');
  const warn = el('#quick-add-warning');
  const dup = findDuplicateSource(input.value);
  input.classList.toggle('has-duplicate', !!dup);
  if (dup) {
    warn.textContent = `already in your list — "${dup.name}"`;
    warn.hidden = false;
  } else {
    warn.hidden = true;
  }
  return dup;
}

function checkBulkAddDuplicates() {
  const warn = el('#add-textarea-warning');
  const lines = splitBulkLines(el('#add-textarea').value);
  const dupNames = [];
  for (const line of lines) {
    const dup = findDuplicateSource(line);
    if (dup) dupNames.push(dup.name);
  }
  if (dupNames.length) {
    const shown = dupNames.slice(0, 3).join(', ');
    const extra = dupNames.length > 3 ? ` +${dupNames.length - 3} more` : '';
    warn.textContent = `already in your list: ${shown}${extra}`;
    warn.hidden = false;
  } else {
    warn.hidden = true;
  }
}

function splitBulkLines(raw) {
  return raw.split(/[\n,]+/).map((s) => s.trim()).filter(Boolean);
}

async function submitQuickAdd() {
  const input = el('#quick-add-input');
  const value = input.value.trim();
  if (!value) { input.focus(); return; }
  try {
    const data = await api('/api/sources', { method: 'POST', body: JSON.stringify({ text: value }) });
    if (data.sources.length) {
      input.value = '';
      el('#quick-add-warning').hidden = true;
      input.classList.remove('has-duplicate');
      toast(`Added "${data.sources[0].name}" — downloading…`);
      await refreshSources();
      maybeStartPolling();
    } else if (data.duplicates.length) {
      toast(`Already in your list: "${data.duplicates[0].name}" — skipped`, true);
    }
  } catch (e) {
    toast('Could not add source: ' + e.message, true);
  }
  input.focus();
}

async function submitAddSources() {
  const text = el('#add-textarea').value;
  if (!text.trim()) { closeAddModal(); return; }
  try {
    const data = await api('/api/sources', { method: 'POST', body: JSON.stringify({ text }) });
    closeAddModal();
    announceAddResult(data);
    await refreshSources();
    maybeStartPolling();
  } catch (e) {
    toast('Could not add sources: ' + e.message, true);
  }
}

function announceAddResult(data) {
  const added = data.sources.length;
  const dupes = data.duplicates.length;
  if (added && dupes) {
    toast(`Added ${added} — skipped ${dupes} already in your list`);
  } else if (added) {
    toast(`Added ${added} source${added === 1 ? '' : 's'} — downloading…`);
  } else if (dupes) {
    toast(`Already in your list — skipped ${dupes} duplicate${dupes === 1 ? '' : 's'}`, true);
  }
}

// ---------------------------------------------------------------------
// settings (download concurrency)
// ---------------------------------------------------------------------

async function openSettingsModal() {
  try {
    const data = await api('/api/settings');
    appSettings = { ...appSettings, ...data };
  } catch (e) {
    toast('Could not load current settings: ' + e.message, true);
  }
  el('#settings-max-concurrent').value = appSettings.max_concurrent;
  el('#settings-max-clip-length').value = appSettings.max_clip_length_secs || 60;
  el('#settings-theme').value = appSettings.theme;
  el('#settings-default-speed').value = appSettings.default_slideshow_speed;
  el('#settings-default-loop').checked = !!appSettings.default_slideshow_loop;
  el('#settings-default-shuffle').checked = !!appSettings.default_slideshow_shuffle;
  el('#settings-export-reminder-days').value = appSettings.export_reminder_days;
  el('#settings-nsfw-filter-enabled').checked = !!appSettings.nsfw_filter_enabled;
  el('#settings-start-with-windows').checked = !!appSettings.start_with_windows;
  el('#settings-keep-running-in-tray').checked = appSettings.keep_running_in_tray !== false;
  renderRemoteAccessStatus();
  el('#settings-modal').hidden = false;
}
function closeSettingsModal() { el('#settings-modal').hidden = true; }

async function saveSettings() {
  const rawConcurrent = parseInt(el('#settings-max-concurrent').value, 10);
  const maxConcurrent = Number.isFinite(rawConcurrent) ? Math.max(1, Math.min(20, rawConcurrent)) : 6;
  const rawReminderDays = parseInt(el('#settings-export-reminder-days').value, 10);
  const reminderDays = Number.isFinite(rawReminderDays) ? Math.max(1, Math.min(365, rawReminderDays)) : 30;
  const nsfwFilterEnabled = el('#settings-nsfw-filter-enabled').checked;
  const nsfwFilterChanged = !!appSettings.nsfw_filter_enabled !== nsfwFilterEnabled;
  const body = {
    max_concurrent: maxConcurrent,
    max_clip_length_secs: Math.max(5, Math.min(3600, parseInt(el('#settings-max-clip-length').value, 10) || 60)),
    theme: el('#settings-theme').value,
    default_slideshow_speed: parseInt(el('#settings-default-speed').value, 10),
    default_slideshow_loop: el('#settings-default-loop').checked,
    default_slideshow_shuffle: el('#settings-default-shuffle').checked,
    export_reminder_days: reminderDays,
    nsfw_filter_enabled: nsfwFilterEnabled,
    start_with_windows: el('#settings-start-with-windows').checked,
    keep_running_in_tray: el('#settings-keep-running-in-tray').checked,
  };
  try {
    const data = await api('/api/settings', { method: 'PATCH', body: JSON.stringify(body) });
    appSettings = { ...appSettings, ...data };
    applyTheme(appSettings.theme);
    configureClipLengthControls();
    closeSettingsModal();
    toast(nsfwFilterChanged ? 'Settings saved — restart Curator for NSFW auto-rating to take effect' : 'Settings saved');
    renderExportReminderBanner();
  } catch (e) {
    toast('Could not save settings: ' + e.message, true);
  }
}

async function renderRemoteAccessStatus() {
  const node = el('#settings-remote-access');
  if (!node) return;
  node.textContent = 'Checking server status…';
  try {
    const info = await api('/api/remote-access');
    const urls = [
      ...(info.local_urls || []),
      ...(info.lan_urls || []),
      ...(info.tailscale_urls || []),
    ];
    node.replaceChildren();
    const heading = document.createElement('div');
    heading.textContent = `Server: ${info.server || (info.running ? 'Running' : 'Stopped')}  ·  Port: ${info.port ?? '—'}`;
    node.append(heading);
    const groups = [
      ['Local', info.local_urls],
      ['LAN', info.lan_urls],
      ['Tailscale', info.tailscale_urls],
    ];
    for (const [label, entries] of groups) {
      if (!entries?.length) continue;
      const row = document.createElement('div');
      row.textContent = `${label}  ${entries.join('  ')}`;
      node.append(row);
    }
    if (!urls.length && info.running) {
      const row = document.createElement('div'); row.textContent = 'No currently reachable interface addresses were detected.'; node.append(row);
    }
  } catch (error) {
    node.textContent = `Remote access status unavailable: ${error.message}`;
  }
}

async function runSetupAgain() {
  // Only flips the oobe_completed flag server-side — no downloads, database
  // rows, or other settings are touched (see routes::oobe::reset). The
  // wizard itself re-reads current settings/config to prefill every step.
  try {
    await api('/api/oobe/reset', { method: 'POST' });
    window.location.href = '/';
  } catch (e) {
    toast('Could not reopen setup: ' + e.message, true);
  }
}

// ---------------------------------------------------------------------
// export / import source list
// ---------------------------------------------------------------------

async function exportSources() {
  try {
    const data = await api('/api/export');
    const blob = new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    const stamp = new Date().toISOString().slice(0, 10);
    a.href = url;
    a.download = `curator-sources-${stamp}.json`;
    document.body.appendChild(a);
    a.click();
    a.remove();
    URL.revokeObjectURL(url);
    toast(`Exported ${data.sources.length} source${data.sources.length === 1 ? '' : 's'}`);
    // /api/export already reset last_export_at and cleared any snooze
    // server-side (see app.py) — mirror that locally so the banner
    // disappears immediately instead of waiting for the next settings load.
    appSettings.last_export_at = data.exported_at;
    appSettings.export_reminder_snoozed_until = null;
    renderExportReminderBanner();
  } catch (e) {
    toast('Could not export sources: ' + e.message, true);
  }
}

// ---------------------------------------------------------------------
// backup / export reminder banner
// ---------------------------------------------------------------------

function renderExportReminderBanner() {
  const banner = el('#export-reminder-banner');
  const now = Date.now();

  if (appSettings.export_reminder_snoozed_until) {
    const snoozedUntil = Date.parse(appSettings.export_reminder_snoozed_until);
    if (Number.isFinite(snoozedUntil) && now < snoozedUntil) {
      banner.hidden = true;
      return;
    }
  }

  const reminderMs = (appSettings.export_reminder_days || 30) * 24 * 60 * 60 * 1000;
  let daysSince = null;
  if (appSettings.last_export_at) {
    const lastExport = Date.parse(appSettings.last_export_at);
    if (Number.isFinite(lastExport)) daysSince = Math.floor((now - lastExport) / (24 * 60 * 60 * 1000));
  }
  const due = daysSince === null || (now - Date.parse(appSettings.last_export_at)) >= reminderMs;

  if (!due) {
    banner.hidden = true;
    return;
  }

  el('#export-reminder-text').textContent = daysSince === null
    ? "You've never exported your source list — worth a backup."
    : `Sources last exported ${daysSince} day${daysSince === 1 ? '' : 's'} ago — worth a fresh backup.`;
  banner.hidden = false;
}

async function snoozeExportReminder() {
  const snoozeUntil = new Date(Date.now() + 7 * 24 * 60 * 60 * 1000).toISOString();
  try {
    const data = await api('/api/settings', {
      method: 'PATCH',
      body: JSON.stringify({ export_reminder_snoozed_until: snoozeUntil }),
    });
    appSettings = { ...appSettings, ...data };
    renderExportReminderBanner();
  } catch (e) {
    toast('Could not snooze the reminder: ' + e.message, true);
  }
}

function triggerImportPicker() {
  el('#import-file-input').click();
}

async function handleImportFile(e) {
  const file = e.target.files && e.target.files[0];
  e.target.value = ''; // allow re-selecting the same file later
  if (!file) return;
  try {
    const text = await file.text();
    const parsed = JSON.parse(text);
    const sources = Array.isArray(parsed) ? parsed : parsed.sources;
    if (!Array.isArray(sources) || !sources.length) {
      toast('That file has no sources in it', true);
      return;
    }
    const data = await api('/api/import', { method: 'POST', body: JSON.stringify({ sources }) });
    announceAddResult(data);
    await refreshSources();
    maybeStartPolling();
  } catch (e) {
    toast('Could not import that file: ' + e.message, true);
  }
}

async function setIncluded(id, included) {
  try {
    await api(`/api/sources/${id}`, { method: 'PATCH', body: JSON.stringify({ included }) });
    if (state.sourcesById[id]) state.sourcesById[id].included = included;
    if (state.view.type === 'all') loadView();
  } catch (e) {
    toast('Could not update selection: ' + e.message, true);
  }
}

async function bulkSetIncluded(included) {
  try {
    await Promise.all(state.sources.map((s) =>
      api(`/api/sources/${s.id}`, { method: 'PATCH', body: JSON.stringify({ included }) })
    ));
    await refreshSources();
    if (state.view.type === 'all') loadView();
  } catch (e) {
    toast('Could not update selection: ' + e.message, true);
  }
}

async function renameSource(id, name) {
  try {
    await api(`/api/sources/${id}`, { method: 'PATCH', body: JSON.stringify({ name }) });
    if (state.sourcesById[id]) state.sourcesById[id].name = name;
    if (state.view.type === 'creator' && state.view.id === id) {
      el('#context-tab-label').textContent = name;
    }
  } catch (e) {
    toast('Rename failed: ' + e.message, true);
    await refreshSources();
  }
}

async function resyncSource(id) {
  try {
    const data = await api(`/api/sources/${id}/resync`, { method: 'POST' });
    toast(data.status === 'already_syncing' ? 'Already syncing' : 'Re-sync queued');
    await refreshSources();
    maybeStartPolling();
  } catch (e) {
    toast('Could not queue re-sync: ' + e.message, true);
  }
}

function setPauseButtonState(paused) {
  const btn = el('#pause-downloads-btn');
  btn.textContent = paused ? '▶ resume' : '⏸ pause';
  btn.title = paused ? 'Resume paused downloads' : 'Stop all downloads right now';
  btn.classList.toggle('is-paused', paused);
  state.downloadsPaused = paused;
}

async function refreshPauseButtonFromServer() {
  try {
    const data = await api('/api/downloads/status');
    setPauseButtonState(data.paused);
  } catch (e) {
    // Non-fatal — button just won't reflect server state until it succeeds.
  }
}

async function togglePauseDownloads() {
  try {
    if (state.downloadsPaused) {
      const data = await api('/api/downloads/resume', { method: 'POST' });
      setPauseButtonState(false);
      const n = data.resumed_sources.length;
      toast(n ? `Resumed — re-queued ${n} source${n === 1 ? '' : 's'}` : 'Downloads resumed');
      await refreshSources();
      maybeStartPolling();
    } else {
      const data = await api('/api/downloads/pause', { method: 'POST' });
      setPauseButtonState(true);
      const n = data.terminated_sources.length;
      toast(n ? `Paused — stopped ${n} in-flight sync${n === 1 ? '' : 's'}` : 'Downloads paused');
      await refreshSources();
    }
  } catch (e) {
    toast('Could not change pause state: ' + e.message, true);
  }
}

async function resyncAllSources() {
  if (!state.sources.length) { toast('No sources yet.', true); return; }
  const busy = state.sources.filter((s) => s.status === 'pending' || s.status === 'downloading').length;
  const eligible = state.sources.length - busy;
  if (eligible === 0) { toast('Everything is already syncing.'); return; }
  const msg = busy
    ? `Re-sync all ${eligible} source${eligible === 1 ? '' : 's'} not already in progress?`
    : `Re-sync all ${eligible} source${eligible === 1 ? '' : 's'}?`;
  if (!confirm(msg)) return;
  try {
    const data = await api('/api/sources/resync-all', { method: 'POST' });
    toast(`Queued ${data.queued} source${data.queued === 1 ? '' : 's'} to re-sync`);
    await refreshSources();
    maybeStartPolling();
  } catch (e) {
    toast('Could not queue re-sync: ' + e.message, true);
  }
}

async function removeSource(id, name) {
  if (!confirm(`Remove "${name}" from Curator?`)) return;
  const deleteFiles = confirm('Also delete its downloaded files from disk?\n\nOK = delete files\nCancel = keep files on disk');
  try {
    await api(`/api/sources/${id}?delete_files=${deleteFiles}`, { method: 'DELETE' });
    if (state.view.type === 'creator' && state.view.id === id) switchView({ type: 'all' });
    await refreshSources();
    loadView();
  } catch (e) {
    toast('Could not remove source: ' + e.message, true);
  }
}

function maybeStartPolling() {
  if (state.pollTimer) return;
  state.pollTimer = setInterval(pollTick, 2500);
}

async function pollTick() {
  const prevCounts = Object.fromEntries(state.sources.map((s) => [s.id, s.item_count]));
  try {
    await refreshSources();
  } catch (_) { return; }
  refreshPauseButtonFromServer();
  const active = state.sources.some((s) => s.status === 'pending' || s.status === 'downloading');
  const changed = state.sources.some((s) => s.item_count !== prevCounts[s.id]);
  if (changed) el('#refresh-banner').hidden = false;
  if (!active) { clearInterval(state.pollTimer); state.pollTimer = null; }
}

// ---------------------------------------------------------------------
// views & grid
// ---------------------------------------------------------------------

function openSidebarDrawer() {
  el('.sidebar').classList.add('open');
  el('#sidebar-backdrop').hidden = false;
  el('#sidebar-open-btn').setAttribute('aria-expanded', 'true');
}

function closeSidebarDrawer() {
  el('.sidebar').classList.remove('open');
  el('#sidebar-backdrop').hidden = true;
  el('#sidebar-open-btn').setAttribute('aria-expanded', 'false');
}

function switchView(view) {
  state.view = view;
  closeSidebarDrawer();
  el('.tab[data-view="all"]').classList.toggle('active', view.type === 'all');

  const label = el('#context-tab-label');
  const divider = el('#context-tab-divider');
  const openLink = el('#open-source-link');

  if (view.type === 'creator') {
    const s = state.sourcesById[view.id];
    label.hidden = false;
    divider.hidden = false;
    label.textContent = s ? s.name : 'creator';
    label.classList.add('active');
    if (s) {
      openLink.hidden = false;
      openLink.href = s.url;
    } else {
      openLink.hidden = true;
    }
  } else if (view.type === 'group') {
    label.hidden = false;
    divider.hidden = false;
    label.textContent = view.name || (view.id === 0 ? 'Ungrouped' : 'group');
    label.classList.add('active');
    openLink.hidden = true;
  } else {
    label.hidden = true;
    divider.hidden = true;
    openLink.hidden = true;
  }

  el('#refresh-banner').hidden = true;
  renderSidebar();
  loadView();
}

let viewRequestSeq = 0;
let mediaPage = { url: '', cursor: null, more: false, pending: null };
let gridShuffleSeed = null;

async function loadView() {
  const requestId = ++viewRequestSeq;
  const params = new URLSearchParams({limit: 150, media_type: state.typeFilter});
  const sizeFilter = el('#size-filter')?.value;
  if (sizeFilter === 'unknown') params.set('unknown_size','true');
  else if (sizeFilter) params.set('min_size',sizeFilter);
  if (state.view.type === 'creator') params.set('source_id', state.view.id);
  else if (state.view.type === 'group') params.set('group_id', state.view.id);
  else params.set('only_included', 'true');
  if (state.sortOrder && state.sortOrder !== 'default') params.set('sort', state.sortOrder);
  if (gridShuffleSeed != null) { params.set('sort','shuffle'); params.set('shuffle_seed',gridShuffleSeed); }
  if (state.tagFilter) params.set('tag',state.tagFilter);
  if (state.maxRatingFilter !== '') params.set('max_rating',state.maxRatingFilter);
  if (state.ratingStatus) params.set('rating_status', state.ratingStatus);
  const page = {url:'/api/media?'+params, cursor:null, more:false, pending:null};
  mediaPage = page;
  try {
    const data = await api(page.url);
    if (requestId !== viewRequestSeq) return;
    page.cursor=data.next_cursor; page.more=data.has_more;
    state.currentItems=data.media;
  } catch(e) {
    if (requestId !== viewRequestSeq) return;
    state.currentItems=[];
    toast('Could not load media: '+e.message,true);
  }
  toggleEmptyState(state.currentItems.length===0);
  renderNextPage(true);
}

async function loadMoreMedia() {
  const page=mediaPage;
  if (page.pending) return page.pending;
  if (!page.more) return [];
  const seq=viewRequestSeq;
  page.pending=(async()=>{
    try {
      const data=await api(page.url+'&cursor='+encodeURIComponent(page.cursor));
      if (seq!==viewRequestSeq) return [];
      page.cursor=data.next_cursor; page.more=data.has_more;
      const seen=new Set(state.currentItems.map(m=>m.id));
      const added=data.media.filter(m=>!seen.has(m.id));
      state.currentItems.push(...added);
      if (ss.active) {
        const existing=new Set(ss.items.map(m=>m.id));
        ss.items.push(...added.filter(m=>!existing.has(m.id)));
      }
      return added;
    } catch(e) { toast('Could not load next page: '+e.message,true); return []; }
    finally {page.pending=null;}
  })();
  return page.pending;
}

function toggleEmptyState(isEmpty) {
  el('#empty-state').hidden = !isEmpty;
  el('#grid').hidden = isEmpty;
}

async function renderNextPage(reset = false) {
  if (reset) {
    state.page = 0;
    state.renderedCount = 0;
    el('#grid').innerHTML = '';
  }
  const seq = viewRequestSeq;
  const start = state.renderedCount || 0;
  if (start >= state.currentItems.length && mediaPage.more) await loadMoreMedia();
  if (seq !== viewRequestSeq || start !== (state.renderedCount || 0)) return;
  const slice = state.currentItems.slice(start, start + state.PAGE_SIZE);
  if (slice.length === 0) return;
  const frag = document.createDocumentFragment();
  slice.forEach((item, i) => frag.appendChild(buildTile(item, start + i)));
  el('#grid').appendChild(frag);
  state.renderedCount = start + slice.length;
  state.page++;
}

// Setting a <video>'s src immediately (like the native `loading="lazy"` on
// <img>) fires a real network request the instant the tile is created —
// with a page of 150 mixed items, that can mean dozens of simultaneous
// metadata requests competing with every image on the same connection pool.
// This mirrors that same lazy behavior for video, just done by hand since
// browsers don't offer it natively for <video>.
const videoLazyObserver = new IntersectionObserver((entries) => {
  entries.forEach((entry) => {
    if (!entry.isIntersecting) return;
    const v = entry.target;
    if (v.dataset.src) {
      v.src = v.dataset.src;
      delete v.dataset.src;
    }
    videoLazyObserver.unobserve(v);
  });
}, { rootMargin: '200px' });

function buildTile(item, index) {
  const tile = document.createElement('div');
  tile.className = 'tile' + (item.type === 'video' ? ' tile-video' : '');

  let mediaEl;
  if (item.type === 'video') {
    mediaEl = document.createElement('video');
    reportVideoDuration(mediaEl, item);
    mediaEl.dataset.src = mediaFullSrc(item);
    mediaEl.preload = 'metadata';
    mediaEl.muted = true;
    videoLazyObserver.observe(mediaEl);
  } else {
    mediaEl = document.createElement('img');
    // Small cached JPEG, not the multi-MB original — see /api/thumb.
    // (Not-yet-downloaded items skip that cache entirely — see mediaThumbSrc.)
    mediaEl.src = mediaThumbSrc(item);
    mediaEl.loading = 'lazy';
    mediaEl.decoding = 'async';
    mediaEl.alt = item.filename;
  }
  tile.appendChild(mediaEl);

  const frame = document.createElement('span');
  frame.className = 'tile-frame mono';
  frame.textContent = pad4(index + 1);
  tile.appendChild(frame);

  if (item.rating > 0) {
    const badge = document.createElement('span');
    badge.className = 'tile-rating mono';
    badge.title = item.rating_reviewed ? 'Human reviewed' : item.rating_source === 'auto' ? 'AUTO - Needs review' : 'Rating';
    badge.textContent = (item.rating_source === 'auto' ? 'AUTO ' : item.rating_reviewed ? 'HUMAN ' : '') + `★${item.rating}`;
    tile.appendChild(badge);
  }

  if (item.type === 'video') {
    const play = document.createElement('span');
    play.className = 'tile-play';
    play.textContent = '▶';
    tile.appendChild(play);
  }

  tile.addEventListener('click', () => openLightbox(index));
  return tile;
}

function shuffleCurrentGrid() {
  gridShuffleSeed = 1 + Math.floor(Math.random() * 2147483645);
  loadView();
}

// ---------------------------------------------------------------------
// lightbox
// ---------------------------------------------------------------------

function openLightbox(index) {
  state.lightboxIndex = index;
  renderLightboxItem();
  el('#lightbox').hidden = false;
}

function closeLightbox() {
  el('#lightbox').hidden = true;
  el('#lightbox-stage').innerHTML = '';
}

// A touch starting on the <video> itself is left alone entirely — that's
// the native player's own controls (including the scrubber, which is a
// horizontal drag), and swiping for navigation there would fight it every
// time someone tries to seek. Swiping anywhere else on the stage (the
// image, or the letterboxed area around a video) navigates.
function enableSwipeNav(container, onPrev, onNext) {
  const THRESHOLD = 50; // px — deliberate swipe, not just an imprecise tap
  let startX = null;
  let startY = null;
  let ignore = false;

  container.addEventListener('touchstart', (e) => {
    if (e.touches.length !== 1 || e.target.closest('video')) {
      ignore = true;
      return;
    }
    ignore = false;
    startX = e.touches[0].clientX;
    startY = e.touches[0].clientY;
  }, { passive: true });

  container.addEventListener('touchend', (e) => {
    if (ignore || startX === null) {
      ignore = false;
      startX = null;
      return;
    }
    const touch = e.changedTouches[0];
    const dx = touch.clientX - startX;
    const dy = touch.clientY - startY;
    startX = null;
    if (Math.abs(dx) > THRESHOLD && Math.abs(dx) > Math.abs(dy) * 1.5) {
      if (dx < 0) onNext(); else onPrev();
    }
  }, { passive: true });
}

async function stepLightbox(delta) {
  if (delta>0 && state.lightboxIndex+delta>=state.currentItems.length && mediaPage.more) await loadMoreMedia();
  const n = state.currentItems.length;
  if (!n) return;
  state.lightboxIndex = (state.lightboxIndex + delta + n) % n;
  renderLightboxItem();
}

function renderLightboxItem() {
  const item = state.currentItems[state.lightboxIndex];
  if (!item) return;
  el('#lightbox-clip-tools').hidden = item.type !== 'video' || item.downloaded === 0 || item.clip_parent_id != null || (item.duration_secs != null && item.duration_secs <= clipMaxSeconds());
  const stage = el('#lightbox-stage');
  stage.innerHTML = '';
  const src = mediaFullSrc(item);

  let mediaEl;
  if (item.type === 'video') {
    mediaEl = document.createElement('video');
    reportVideoDuration(mediaEl, item);
    mediaEl.src = src;
    mediaEl.controls = true;
    mediaEl.autoplay = true;
    mediaEl.playsInline = true;
  } else {
    mediaEl = document.createElement('img');
    mediaEl.src = src;
    mediaEl.alt = item.filename;
  }
  stage.appendChild(mediaEl);

  const source = state.sourcesById[item.source_id];
  el('#lightbox-meta').textContent =
    `${pad4(state.lightboxIndex + 1)} / ${pad4(state.currentItems.length)}  —  ${item.filename}  —  ${source ? source.name : ''}`;

  renderStarRating(el('#lightbox-rating'), item.rating || 0, (rating) => rateMedia(item, rating));
  renderTagRow(item);
}

let clipJobPending = false;
async function createVideoClips() {
  if (clipJobPending) return;
  const item = state.currentItems[state.lightboxIndex];
  if (!item || item.type !== 'video') return;
  clipJobPending = true;
  el('#create-clips-btn').disabled = true;
  try {
    const job = await api(`/api/media/${item.id}/clips`, {method:'POST',body:JSON.stringify({seconds:Number(el('#clip-seconds').value)})});
    localStorage.setItem('curatorClipJob', String(job.job_id));
    watchClipJob(job.job_id);
  } catch (e) {
    clipJobPending = false; el('#create-clips-btn').disabled = false;
    toast('Could not create clips: ' + e.message, true);
  }
}
async function watchClipJob(id) {
  clipJobPending = true; el('#create-clips-btn').disabled = true;
  try {
    const job = await api(`/api/clip-jobs/${id}`);
    if (job.status === 'running') {
      el('#clip-job-status').textContent = 'Saving virtual clip ranges. No video files are copied.';
      setTimeout(() => watchClipJob(id), 3000); return;
    }
    localStorage.removeItem('curatorClipJob');
    clipJobPending = false; el('#create-clips-btn').disabled = false;
    el('#clip-job-status').textContent = job.status === 'done' ? `${job.clip_count} clips ready - refresh the grid to view` : job.error;
    if (job.status === 'done') el('#refresh-banner').hidden = false;
  } catch (e) {
    if (e.status === 404) {
      localStorage.removeItem('curatorClipJob'); clipJobPending = false;
      el('#create-clips-btn').disabled = false;
      el('#clip-job-status').textContent = 'Clip job is no longer available.'; return;
    }
    el('#clip-job-status').textContent = 'Waiting to reconnect to clip job...';
    setTimeout(() => watchClipJob(id), 5000);
  }
}

function renderStarRating(container, rating, onRate) {
  container.innerHTML = '';
  for (let i = 1; i <= 5; i++) {
    const star = document.createElement('button');
    star.type = 'button';
    star.className = 'star' + (i <= rating ? ' filled' : '');
    star.textContent = '★';
    star.title = `${i} star${i === 1 ? '' : 's'}`;
    // Clicking the star that's already the current rating clears it —
    // otherwise there'd be no way to get back to "unrated" once rated.
    star.addEventListener('click', () => onRate(i === rating ? 0 : i));
    container.appendChild(star);
  }
}

async function rateMedia(item, rating) {
  try {
    Object.assign(item, await api(`/api/media/${item.id}/rating`, { method: 'PUT', body: JSON.stringify({ rating }) }));
    renderStarRating(el('#lightbox-rating'), rating, (r) => rateMedia(item, r));
  } catch (e) {
    toast('Could not save rating: ' + e.message, true);
  }
}

// Tag names only come back attached to media/group rows — removal needs a
// tag id, so this keeps a name→id map filled in from /api/tags, refreshed
// whenever a tag is added (which may have just created a brand new one).
state.tagsByName = {};

async function refreshTagIndex() {
  try {
    const data = await api('/api/tags');
    state.tagsByName = {};
    data.tags.forEach((t) => { state.tagsByName[t.name] = t.id; });
    populateTagFilterOptions(data.tags);
  } catch (e) {
    // Non-fatal — tag removal just won't work until this succeeds.
  }
}

function populateTagFilterOptions(tags) {
  const select = el('#tag-filter-select');
  const current = select.value;
  select.innerHTML = '<option value="">all tags</option>';
  tags.forEach((t) => {
    const opt = document.createElement('option');
    opt.value = t.name;
    opt.textContent = `${t.name} (${t.media_count})`;
    select.appendChild(opt);
  });
  // Keep whatever was selected, if it still exists — this runs after every
  // tag add/remove, not just once at startup.
  if ([...select.options].some((o) => o.value === current)) select.value = current;
}

function renderTagRow(item) {
  const row = el('#lightbox-tags');
  row.innerHTML = '';
  (item.tags || []).forEach((tagName) => {
    row.appendChild(buildTagChip(tagName, () => removeTagFromMedia(item, tagName)));
  });
  // Inherited from the file's group (or the group's own name) — not
  // removable here since there's no per-file row to delete; edit the
  // group's tags, or move the file to a different group, to change these.
  (item.inherited_tags || []).forEach((tagName) => {
    const chip = buildTagChip(tagName, null);
    chip.classList.add('tag-chip-inherited');
    chip.title = 'From this file\'s group';
    row.appendChild(chip);
  });
}

function buildTagChip(tagName, onRemove) {
  const chip = document.createElement('span');
  chip.className = 'tag-chip';
  const label = document.createElement('span');
  label.textContent = tagName;
  chip.appendChild(label);
  if (onRemove) {
    const removeBtn = document.createElement('button');
    removeBtn.type = 'button';
    removeBtn.textContent = '×';
    removeBtn.title = `Remove "${tagName}"`;
    removeBtn.addEventListener('click', onRemove);
    chip.appendChild(removeBtn);
  }
  return chip;
}

async function addTagToGroup(groupId, name) {
  name = name.trim();
  if (!name) return;
  try {
    await api(`/api/groups/${groupId}/tags`, { method: 'POST', body: JSON.stringify({ name }) });
    await refreshTagIndex();
    await refreshGroups();
    renderSidebar();
  } catch (e) {
    toast('Could not add tag: ' + e.message, true);
  }
}

async function removeTagFromGroup(groupId, tagName) {
  if (state.tagsByName[tagName] == null) await refreshTagIndex();
  const tagId = state.tagsByName[tagName];
  if (tagId == null) return;
  try {
    await api(`/api/groups/${groupId}/tags/${tagId}`, { method: 'DELETE' });
    await refreshGroups();
    renderSidebar();
  } catch (e) {
    toast('Could not remove tag: ' + e.message, true);
  }
}
async function addTagToMedia(item, name) {
  name = name.trim();
  if (!name) return;
  try {
    const data = await api(`/api/media/${item.id}/tags`, { method: 'POST', body: JSON.stringify({ name }) });
    item.tags = data.tags;
    renderTagRow(item);
    refreshTagIndex();
  } catch (e) {
    toast('Could not add tag: ' + e.message, true);
  }
}

async function removeTagFromMedia(item, tagName) {
  if (state.tagsByName[tagName] == null) { await refreshTagIndex(); }
  const resolvedId = state.tagsByName[tagName];
  if (resolvedId == null) return;
  try {
    await api(`/api/media/${item.id}/tags/${resolvedId}`, { method: 'DELETE' });
    item.tags = (item.tags || []).filter((t) => t !== tagName);
    renderTagRow(item);
  } catch (e) {
    toast('Could not remove tag: ' + e.message, true);
  }
}

// ---------------------------------------------------------------------
// slideshow
// ---------------------------------------------------------------------

async function startSlideshow(startIndex) {
  if (el('#ss-shuffle').checked && gridShuffleSeed == null) {
    gridShuffleSeed = 1 + Math.floor(Math.random() * 2147483645);
    await loadView();
    startIndex = 0;
  }
  if (!state.currentItems.length) { toast('Nothing to show yet.', true); return; }
  closeLightbox();
  closeSourceMenu();

  const requested = state.currentItems[Math.max(0, startIndex || 0)];
  ss.items = preparePlaybackItems(state.currentItems, false);
  ss.index = Math.max(0, ss.items.indexOf(requested));
  ss.playing = true;
  ss.speed = parseInt(el('#ss-speed').value, 10);
  ss.loop = el('#ss-loop').checked;
  ss.shuffleMode = el('#ss-shuffle').checked;

  if (ss.shuffleMode) {
    const current = ss.items[ss.index];
    ss.items = preparePlaybackItems(ss.items, true);
    ss.index = Math.max(0, ss.items.indexOf(current));
  }

  ss.active = true;
  el('#slideshow').hidden = false;
  renderSlide();
}

function exitSlideshow() {
  clearAdvanceTimer();
  detachVideoListeners();
  ss.active = false;
  if (isFullscreen()) exitBrowserFullscreen();
  el('#slideshow').hidden = true;
  el('#slideshow-stage').innerHTML = '';
}

// ---------------------------------------------------------------------
// Portrait wall — 3 portrait-orientation files side by side, each pane
// advancing on its own schedule (pictures on a timer, videos when they
// finish) rather than all 3 changing in lockstep. Pulls from whatever the
// current view + type filter already produced (state.currentItems), so
// "photos only" or "videos only" naturally carries over into this view too.
// ---------------------------------------------------------------------

const PW_PREFETCH_DEPTH = 2; // verified-portrait items to keep queued up per pane

const pw = {
  active: false,
  queueIndex: 0,
  items: [],
  timers: [null, null, null],
  ready: [[], [], []],             // per-pane queues of already-checked {item, el}
  filling: [false, false, false],  // guards against two overlapping fill loops on one pane
};

async function pwNextCandidate() {
  if (pw.queueIndex >= pw.items.length && mediaPage.more) {
    await loadMoreMedia();
    pw.items = preparePlaybackItems(state.currentItems, true);
    pw.queueIndex = 0;
  }
  if (pw.queueIndex >= pw.items.length) {
    // The already-seen collection can recycle after exhaustion, but its next
    // choice is rearranged to avoid the most recently displayed item.
    pw.items = preparePlaybackItems(pw.items, true);
    pw.queueIndex = 0;
  }
  return pw.items[pw.queueIndex++] || null;
}

// Orientation isn't stored anywhere, so this checks it the cheap way:
// images are probed via their small cached thumbnail (not the multi-MB
// original), and videos only need their metadata (preload="metadata"),
// not the actual file. Only once something is confirmed portrait does the
// real, full-quality file start loading — and since that happens here,
// during prefetch, well before the item is actually displayed, it's
// usually already finished loading by the time its turn comes up.
function pwCheckAndPrepare(item) {
  return new Promise((resolve) => {
    if (item.type === 'video') {
      const v = document.createElement('video');
      v.preload = 'metadata';
      v.playsInline = true;
      v.onloadedmetadata = () => {
        // Long-form video doesn't fit this view — treat it the same as a
        // rejected orientation: measured fresh here (not from the
        // server-backfilled duration_secs, which may not be known yet for
        // this file) so the exclusion is correct immediately, not only
        // once the background backfill has caught up to it.
        if ((item.clip_start_secs != null ? item.duration_secs : v.duration) > clipMaxSeconds()) { resolve(null); return; }
        resolve(v.videoHeight > v.videoWidth ? { item, el: v } : null);
      };
      v.onerror = () => resolve(null);
      bindVirtualClip(v, item);
      v.src = mediaFullSrc(item);
    } else {
      const probe = new Image();
      probe.onload = () => {
        if (probe.naturalHeight <= probe.naturalWidth) { resolve(null); return; }
        const full = document.createElement('img');
        full.alt = item.filename;
        full.src = mediaFullSrc(item); // starts loading now, ahead of need
        resolve({ item, el: full });
      };
      probe.onerror = () => resolve(null);
      probe.src = mediaThumbSrc(item);
    }
  });
}

// Keeps pane i's ready-queue topped up in the background. Safe to call any
// time (e.g. right after a pane consumes one) — it's a no-op if a fill is
// already in flight or the queue is already at depth.
async function pwFillReady(i) {
  if (pw.filling[i]) return;
  pw.filling[i] = true;
  try {
    while (pw.active && pw.ready[i].length < PW_PREFETCH_DEPTH) {
      const item = await pwNextCandidate();
      if (!item) break; // no more candidates left anywhere, for any pane
      const result = await pwCheckAndPrepare(item);
      if (!pw.active) return;
      if (result) pw.ready[i].push(result);
    }
  } finally {
    pw.filling[i] = false;
  }
}

async function pwAdvance(i) {
  if (!pw.active) return;
  if (pw.ready[i].length === 0) {
    await pwFillReady(i); // nothing queued yet (first run, or prefetch fell behind) — wait for it
    if (!pw.active) return;
  }
  const next = pw.ready[i].shift();
  if (!next) { pwShowEmpty(i); return; }
  pwMountPane(i, next.item, next.el);
  pwFillReady(i); // top the queue back up in the background — don't wait on this
}

function pwShowEmpty(i) {
  if (pw.timers[i]) { clearTimeout(pw.timers[i]); pw.timers[i] = null; }
  const media = el(`#pw-pane-media-${i}`);
  media.innerHTML = '<div class="pw-pane-empty">No more portrait files here</div>';
}

function pwMountPane(i, item, mediaEl) {
  if (pw.timers[i]) { clearTimeout(pw.timers[i]); pw.timers[i] = null; }

  const media = el(`#pw-pane-media-${i}`);
  media.innerHTML = '';
  rememberPlaybackItem(item);
  mediaEl.className = 'pw-media';
  media.appendChild(mediaEl);

  if (item.type === 'video') {
    // Metadata already loaded during the check phase, so playback here is
    // effectively immediate — no separate "wait for load" step needed.
    mediaEl.addEventListener('ended', () => { if (pw.active) pwAdvance(i); });
    mediaEl.addEventListener('error', () => { if (pw.active) pwAdvance(i); });
    const p = mediaEl.play();
    if (p && p.catch) {
      // Some browsers block unmuted autoplay outright — fall back to
      // muted rather than leaving the video stuck paused. No "tap to
      // unmute" prompt here; keeping this view hands-off.
      p.catch(() => { mediaEl.muted = true; mediaEl.play().catch(() => {}); });
    }
  } else {
    mediaEl.addEventListener('error', () => { if (pw.active) pwAdvance(i); });
    // The timer only starts once the image is actually, visibly loaded —
    // it was very likely finished during prefetch already (mediaEl.complete
    // covers that instantly), but if it's still in flight (a big file, or
    // prefetch just started), wait for the real 'load' event rather than
    // burning part of the display duration on a still-loading image.
    const startTimer = () => {
      if (!pw.active) return;
      pw.timers[i] = setTimeout(() => { if (pw.active) pwAdvance(i); }, ss.speed);
    };
    if (mediaEl.complete && mediaEl.naturalWidth > 0) {
      startTimer();
    } else {
      mediaEl.addEventListener('load', startTimer, { once: true });
    }
  }
}

function startPortraitWall() {
  if (!state.currentItems.length) { toast('Nothing to show here.', true); return; }
  pw.active = true;
  pw.queueIndex = 0;
  pw.items = preparePlaybackItems(state.currentItems, true);
  pw.ready = [[], [], []];
  pw.filling = [false, false, false];
  el('#portrait-wall').hidden = false;
  requestBrowserFullscreen(el('#portrait-wall'));
  for (let i = 0; i < 3; i++) pwAdvance(i);
}

function exitPortraitWall() {
  pw.active = false;
  for (let i = 0; i < 3; i++) {
    if (pw.timers[i]) { clearTimeout(pw.timers[i]); pw.timers[i] = null; }
    const media = el(`#pw-pane-media-${i}`);
    const v = media && media.querySelector('video');
    if (v) v.pause();
    if (media) media.innerHTML = '';
  }
  pw.ready = [[], [], []];
  pw.items = [];
  if (isFullscreen() && document.fullscreenElement === el('#portrait-wall')) exitBrowserFullscreen();
  el('#portrait-wall').hidden = true;
}

// ---------------------------------------------------------------------
// Feed — a full-screen, one-item-per-screen scrolling view for mobile.
// Manual swipes are just native scrolling (scroll-snap handles that for
// free); "auto scroll" on top of that means each item smooth-scrolls to
// the next on its own once its time (pictures) or its length (videos) is
// up, same pacing rules as the slideshow. Landscape files get rotated 90°
// so they fill the portrait screen instead of sitting as a thin strip —
// see the .feed-media-wrap.rotated CSS for how.
//
// Items are pulled from state.currentItems in catalog order, but an item
// is only ever appended to the visible scroll list once its media has
// actually finished loading (a real decoded frame, not just headers) —
// see feedPumpPrefetch/feedLoadAndAppend. A few candidates load in
// parallel and whichever finishes first gets shown first, which can
// reorder things slightly relative to catalog order. That's the point:
// the user should essentially never scroll onto something still loading,
// because it was never appended until it was already confirmed good. An
// item that errors (404, corrupt/undecodable file) or times out after
// FEED_LOAD_TIMEOUT_MS is just dropped silently and replaced with the
// next candidate — it never occupies a slot in the visible feed at all.
// ---------------------------------------------------------------------

const FEED_TARGET_BUFFER = 3;       // keep at least this many loaded-and-appended items unseen, ahead of the viewer
const FEED_PRELOAD_POOL = 4;        // max candidates loading in parallel at once
const FEED_LOAD_TIMEOUT_MS = 15000; // a hung load counts as failed after this, freeing its pool slot

const feed = {
  active: false,
  sourceIndex: 0,   // next untried index into state.currentItems
  inFlight: 0,      // candidates currently loading (<= FEED_PRELOAD_POOL)
  activeSection: null,
  itemObserver: null,
  session: 0, review: false, items: [], page: null,
  seenIds: new Set(), recentIds: [], queuedIds: new Set(),
  recyclePool: new Map(), recycleMode: false, failedIds: new Set(),
  loading: new Set(), wakeLock: null, wakePending: null, wakeEpoch: 0,
  waitingNext: null, retryTimer: null,
};

function feedFlashIcon(iconEl, symbol) {
  iconEl.textContent = symbol;
  iconEl.classList.remove('show');
  void iconEl.offsetWidth; // force reflow so re-adding 'show' restarts the animation
  iconEl.classList.add('show');
}

// Resolves true once `mediaEl` has an actual decoded frame ready to
// paint (not just headers/metadata), false on error OR on timeout. A
// timeout is treated exactly like an error — the pool slot is freed
// either way, so a dead link or stalled connection can never
// permanently starve the prefetch pipeline.
function feedWaitForMedia(mediaEl, isVideo) {
  return new Promise((resolve) => {
    let settled = false;
    const finish = (ok) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      cleanup();
      mediaEl._cancelLoad = null;
      resolve(ok);
    };
    mediaEl._cancelLoad = () => finish(false);
    const onReady = () => {
      if (!isVideo && mediaEl.decode) mediaEl.decode().then(() => finish(true), () => finish(false));
      else finish(true);
    };
    const onError = () => finish(false);
    function cleanup() {
      mediaEl.removeEventListener(isVideo ? 'loadeddata' : 'load', onReady);
      mediaEl.removeEventListener('error', onError);
    }
    const timer = setTimeout(() => finish(false), FEED_LOAD_TIMEOUT_MS);
    mediaEl.addEventListener(isVideo ? 'loadeddata' : 'load', onReady);
    mediaEl.addEventListener('error', onError);
  });
}

// Builds a feed item's DOM but does not set the media source until
// listeners are attached, and does not append it anywhere — caller gets
// it back along with a `ready` promise (see feedWaitForMedia) that
// resolves true once a real frame has decoded, false on error or on
// FEED_LOAD_TIMEOUT_MS.
function feedBuildItem(item) {
  const section = document.createElement('section');
  section.className = 'feed-item';

  const wrap = document.createElement('div');
  wrap.className = 'feed-media-wrap';
  section.appendChild(wrap);

  const progress = document.createElement('div');
  progress.className = 'feed-progress';
  const track = document.createElement('div');
  track.className = 'feed-progress-track';
  const fill = document.createElement('div');
  fill.className = 'feed-progress-fill';
  track.appendChild(fill);
  progress.appendChild(track);
  section.appendChild(progress);

  const pauseIcon = document.createElement('div');
  pauseIcon.className = 'feed-pause-icon';
  section.appendChild(pauseIcon);

  let mediaEl;
  let ready;
  if (item.type === 'video') {
    mediaEl = document.createElement('video');
    reportVideoDuration(mediaEl, item);
    mediaEl.playsInline = true;
    // 'auto' (not 'metadata') so the 'loadeddata' wait in feedWaitForMedia
    // actually has a decoded frame to show, not just fetched dimensions.
    mediaEl.preload = 'auto';
    ready = feedWaitForMedia(mediaEl, true).then((ok) => {
      // Long-form video doesn't fit this view — treat it the same as a
      // load failure/timeout: never appended, next candidate takes its
      // slot. Measured fresh here (not from server-backfilled
      // duration_secs, which may not be known yet for this file) so the
      // exclusion is correct immediately, not only once the backfill has
      // caught up to it.
      return ok && !feed.review && (item.clip_start_secs != null ? item.duration_secs : mediaEl.duration) > clipMaxSeconds() ? false : ok;
    });
    mediaEl.onloadedmetadata = () => {
      if (mediaEl.videoWidth > mediaEl.videoHeight) wrap.classList.add('rotated');
    };
    // Attached once, here, rather than on every activation — these only
    // ever fire while actually playing, which only happens while active.
    mediaEl.addEventListener('timeupdate', () => {
      if (!mediaEl.duration) return;
      fill.style.transition = 'none';
      fill.style.width = (clipProgress(mediaEl) * 100) + '%';
    });
    mediaEl.addEventListener('ended', () => { if (feed.active && !feed.review && feed.activeSection === section) feedGoNext(section); });

    // Tap the video to pause/resume. userPaused only controls whether a
    // scrub-release (below) resumes playback — scrolling away and back
    // always restarts autoplay regardless (see feedActivate), matching
    // how every short-video feed actually behaves; a tap-pause doesn't
    // stick past leaving the item.
    let userPaused = false;
    mediaEl.addEventListener('click', () => {
      if (mediaEl.paused) {
        mediaEl.play();
        userPaused = false;
        feedFlashIcon(pauseIcon, '▶');
      } else {
        mediaEl.pause();
        userPaused = true;
        feedFlashIcon(pauseIcon, '⏸');
      }
    });

    // Scrubbing. The visible line is 3px, but it sits inside a much
    // taller invisible touch target (see .feed-progress in style.css) —
    // a 3px hitbox alone is unusable to drag with a thumb. touch-action:
    // none on that target (also in CSS) is what stops a drag here from
    // being read as "swipe to the next video" by the scroll-snap feed.
    let wasPlayingBeforeScrub = false;
    const seekFromEvent = (e) => {
      if (!mediaEl.duration) return;
      const rect = track.getBoundingClientRect();
      const ratio = Math.min(1, Math.max(0, (e.clientX - rect.left) / rect.width));
      mediaEl.currentTime = clipStart(mediaEl) + ratio * clipDuration(mediaEl);
      fill.style.transition = 'none';
      fill.style.width = (ratio * 100) + '%';
    };
    progress.addEventListener('pointerdown', (e) => {
      e.stopPropagation();
      progress.setPointerCapture(e.pointerId);
      progress.classList.add('scrubbing');
      wasPlayingBeforeScrub = !mediaEl.paused;
      mediaEl.pause();
      seekFromEvent(e);
    });
    progress.addEventListener('pointermove', (e) => {
      if (progress.classList.contains('scrubbing')) seekFromEvent(e);
    });
    const endScrub = () => {
      if (!progress.classList.contains('scrubbing')) return;
      progress.classList.remove('scrubbing');
      if (wasPlayingBeforeScrub && !userPaused) mediaEl.play();
    };
    progress.addEventListener('pointerup', endScrub);
    progress.addEventListener('pointercancel', endScrub);
  } else {
    mediaEl = document.createElement('img');
    mediaEl.alt = item.filename;
    ready = feedWaitForMedia(mediaEl, false);
    mediaEl.onload = () => {
      if (mediaEl.naturalWidth > mediaEl.naturalHeight) wrap.classList.add('rotated');
    };
  }
  mediaEl.src = mediaFullSrc(item);
  wrap.appendChild(mediaEl);

  const rating = document.createElement('div');
  rating.className = 'star-rating feed-rating';
  const refreshRating = () => renderStarRating(rating, item.rating || 0, onRateClick);
  const onRateClick = async (r) => {
    await rateMedia(item, r);
    refreshRating();
  };
  refreshRating();
  section.appendChild(rating);

  if (feed.review) { rating.remove(); feedBuildReviewControls(section, item); }
  section._item = item;
  section._mediaEl = mediaEl;
  section._fill = fill;
  section._timer = null;
  return { section, ready };
}

function feedGoNext(section) {
  const next = section.nextElementSibling;
  if (next) { feed.waitingNext = null; next.scrollIntoView({behavior:'smooth', block:'start'}); }
  else {
    feed.waitingNext = section;
    feedPumpPrefetch();
    // With only one playable item, replay in place without allocating media.
    if (!feed.review && !feed.page.more && !feed.page.pending && !feed.inFlight &&
        [...feed.recyclePool.keys()].filter(id => !feed.failedIds.has(id)).length === 1) {
      feedDeactivate(section);
      if (section._mediaEl.tagName === 'VIDEO') section._mediaEl.currentTime = clipStart(section._mediaEl);
      feedActivate(section);
    }
  }
}

function feedActivate(section) {
  if (feed.activeSection === section) return;
  const sections = [...el('#feed-scroll').children];
  const backwards = feed.activeSection && sections.indexOf(section) < sections.indexOf(feed.activeSection);
  if (feed.activeSection) feedDeactivate(feed.activeSection);
  feed.activeSection = section;
  feed.waitingNext = null;
  section._queued = false;
  feed.queuedIds.delete(section._item.id);
  feed.seenIds.add(section._item.id);
  feed.recyclePool.set(section._item.id, section._item);
  feed.recentIds.push(section._item.id);
  feed.recentIds = feed.recentIds.slice(-10);
  feedEvict();
  feedPumpPrefetch(); // the viewer just consumed one slot of buffer — top it back up

  const fill = section._fill;
  fill.style.transition = 'none';
  fill.style.width = '0%';

  const item = section._item;
  const mediaEl = section._mediaEl;
  if (item.type === 'video') {
    attemptVideoPlay(mediaEl, section);
  } else if (!feed.review) {
    const startTimer = () => {
      if (feed.activeSection !== section) return;
      requestAnimationFrame(() => {
        fill.style.transition = `width ${ss.speed}ms linear`;
        fill.style.width = '100%';
      });
      section._timer = setTimeout(() => { if (feed.active) feedGoNext(section); }, ss.speed);
    };
    if (mediaEl.complete && mediaEl.naturalWidth > 0) startTimer();
    else mediaEl.addEventListener('load', startTimer, { once: true });
  }
  if (feed.review) section._onReviewActivate?.(backwards);
}

function feedDeactivate(section) {
  clearInterval(section._countdownTimer); section._countdownTimer = null;
  if (section._timer) { clearTimeout(section._timer); section._timer = null; }
  const mediaEl = section._mediaEl;
  if (mediaEl && mediaEl.tagName === 'VIDEO') mediaEl.pause();
  if (feed.activeSection === section) feed.activeSection = null;
}

// How many already-appended sections the viewer hasn't reached yet.
// Sections are only ever appended at the tail in load-finish order and
// never reordered afterward, so this is a plain sibling walk from
// whichever one is currently active (or from the top, before anything's
// been activated yet).
function feedUnseenBufferCount() {
  const scrollEl = el('#feed-scroll');
  let node = feed.activeSection ? feed.activeSection.nextElementSibling : scrollEl.firstElementChild;
  let count = 0;
  while (node) { count++; node = node.nextElementSibling; }
  return count;
}

// Pulls candidates from state.currentItems and starts loading each one in
// parallel (up to FEED_PRELOAD_POOL at once), stopping once the unseen
// buffer plus what's already in flight reaches FEED_TARGET_BUFFER.
// Self-chains: each candidate's resolution (feedLoadAndAppend) calls this
// again, so the pool keeps refilling on its own as items finish loading —
// or fail/time out and get silently replaced — without needing a
// scroll-position trigger. Also called from feedActivate so the buffer
// tops back up as the viewer advances, regardless of scroll speed.
function feedCandidate() {
  while (feed.sourceIndex < feed.items.length) {
    const item = feed.items[feed.sourceIndex++];
    if (!feed.seenIds.has(item.id) && !feed.queuedIds.has(item.id) && !feed.failedIds.has(item.id)) return item;
  }
  if (feed.page.more || feed.page.pending || feed.inFlight || feed.review) return null;
  feed.recycleMode = true;
  const eligible = [...feed.recyclePool.values()].filter(item =>
    item.id !== feed.activeSection?._item.id && !feed.queuedIds.has(item.id) && !feed.failedIds.has(item.id));
  const fresh = eligible.filter(item => !feed.recentIds.includes(item.id));
  const pool = fresh.length ? fresh : eligible;
  return pool.length ? pool[Math.floor(Math.random() * pool.length)] : null;
}

async function feedFetchPage() {
  const page = feed.page, session = feed.session;
  if (page.pending || !page.more) return;
  page.pending = true;
  try {
    const data = await api(page.url + (page.cursor ? '&cursor=' + encodeURIComponent(page.cursor) : ''));
    if (!feed.active || session !== feed.session) return;
    feed.items = feed.items.slice(feed.sourceIndex).concat(data.media);
    feed.sourceIndex = 0;
    page.cursor = data.next_cursor; page.more = data.has_more;
  } catch (e) {
    if (session !== feed.session || !feed.active) return;
    toast('Could not load feed page. Retrying...', true);
    feed.retryTimer = setTimeout(() => { feed.retryTimer = null; feedPumpPrefetch(); }, 3000);
  } finally {
    page.pending = false;
    if (session === feed.session && !feed.retryTimer) feedPumpPrefetch();
  }
}

function feedPumpPrefetch() {
  if (!feed.active) return;
  while (feed.inFlight < FEED_PRELOAD_POOL && feedUnseenBufferCount() + feed.inFlight < FEED_TARGET_BUFFER) {
    const item = feedCandidate();
    if (!item) break;
    // A recycled ID may still have a retained previous section. Release it
    // before reserving a new preload, so backward swipes cannot race that ID.
    const scroll = el('#feed-scroll');
    for (const old of [...scroll.children]) {
      if (old._item.id === item.id && old !== feed.activeSection) {
        const above = [...scroll.children].indexOf(old) < [...scroll.children].indexOf(feed.activeSection);
        const height = old.offsetHeight;
        feedDispose(old);
        if (above) scroll.scrollTop -= height;
      }
    }
    feed.queuedIds.add(item.id);
    feed.inFlight++;
    feedLoadAndAppend(item, feed.session);
  }
  if (feed.sourceIndex >= feed.items.length && feed.page.more && !feed.page.pending && !feed.retryTimer) feedFetchPage();
  if (!feed.page.more && !feed.page.pending && !feed.inFlight && !feedUnseenBufferCount()) {
    el('#feed-status').textContent = feed.review ? 'End of review queue. Skipped items remain for later.' :
      (!feed.activeSection ? 'No playable media in this view.' : '');
  }
}

async function feedLoadAndAppend(item, session) {
  const { section, ready } = feedBuildItem(item);
  section._queued = true;
  feed.loading.add(section);
  const ok = await ready;
  if (session !== feed.session || !feed.active) return;
  feed.loading.delete(section);
  feed.inFlight--;
  if (ok) {
    el('#feed-scroll').appendChild(section);
    feed.itemObserver.observe(section);
    if (feed.waitingNext === feed.activeSection && feed.waitingNext) feedGoNext(feed.waitingNext);
  } else { feed.failedIds.add(item.id); feedDispose(section); }
  feedPumpPrefetch();
}

function feedDispose(section) {
  feedDeactivate(section);
  feed.itemObserver?.unobserve(section);
  const media = section._mediaEl;
  if (media) {
    media._cancelLoad?.();
    if (media.tagName === 'VIDEO') media.pause();
    media.removeAttribute('src');
    if (media.tagName === 'VIDEO') media.load();
  }
  if (section._queued) feed.queuedIds.delete(section._item.id);
  section.remove();
}

function feedEvict() {
  const scroll = el('#feed-scroll');
  const sections = [...scroll.children];
  const index = sections.indexOf(feed.activeSection);
  let removedHeight = 0;
  sections.forEach((section, i) => {
    if (i < index - 2 || i > index + FEED_TARGET_BUFFER) {
      if (i < index) removedHeight += section.offsetHeight;
      // A backward swipe can trim an unshown tail. Return it to the
      // lightweight source queue so it is still preferred over recycling.
      if (section._queued && !feed.seenIds.has(section._item.id)) feed.items.splice(feed.sourceIndex, 0, section._item);
      feedDispose(section);
    }
  });
  scroll.scrollTop -= removedHeight;
}

async function feedAcquireWakeLock() {
  if (!feed.active || document.visibilityState !== 'visible' || !navigator.wakeLock || feed.wakeLock || feed.wakePending) return;
  const epoch = feed.wakeEpoch;
  try {
    const request = navigator.wakeLock.request('screen');
    feed.wakePending = request;
    const lock = await request;
    if (!feed.active || epoch !== feed.wakeEpoch || document.visibilityState !== 'visible') { await lock.release(); return; }
    feed.wakeLock = lock;
    lock.addEventListener('release', () => { if (feed.wakeLock === lock) feed.wakeLock = null; });
  } catch (_) {} finally { if (epoch === feed.wakeEpoch) feed.wakePending = null; }
}

function feedReleaseWakeLock() {
  feed.wakeEpoch++;
  const lock = feed.wakeLock;
  feed.wakeLock = null; feed.wakePending = null;
  if (lock) lock.release().catch(() => {});
}

function startFeed(review = false) {
  if (feed.active) exitFeed();
  feed.session++;
  feed.active = true; feed.review = review;
  feed.sourceIndex = 0; feed.inFlight = 0; feed.activeSection = null;
  feed.seenIds.clear(); feed.recentIds = []; feed.queuedIds.clear();
  feed.recyclePool.clear(); feed.failedIds.clear(); feed.recycleMode = false;
  const params = new URLSearchParams(mediaPage.url.split('?')[1] || '');
  if (review) { params.set('rating_status', 'needs_review'); params.set('sort', 'default'); params.delete('shuffle_seed'); }
  feed.items = review ? [] : state.currentItems.slice();
  feed.page = {url:'/api/media?' + params, cursor:review ? null : mediaPage.cursor, more:review || mediaPage.more, pending:false};
  const scrollEl = el('#feed-scroll');
  scrollEl.innerHTML = '';
  feed.itemObserver = new IntersectionObserver(entries => {
    entries.forEach(entry => {
      if (feed.active && entry.target.isConnected && entry.isIntersecting && entry.intersectionRatio > 0.6) feedActivate(entry.target);
    });
  }, {root:scrollEl, threshold:[0, 0.6, 1]});
  el('#feed').hidden = false;
  el('#feed').classList.toggle('review-mode', review);
  el('#feed-status').textContent = '';
  scrollEl.scrollTop = 0;
  feedAcquireWakeLock(); feedPumpPrefetch();
}

function exitFeed() {
  feed.active = false; feed.session++;
  clearTimeout(feed.retryTimer); feed.retryTimer = null; feed.waitingNext = null;
  feed.itemObserver?.disconnect();
  [...el('#feed-scroll').children, ...feed.loading].forEach(feedDispose);
  feed.itemObserver = null; feed.loading.clear(); feed.inFlight = 0;
  feed.items = []; feed.recyclePool.clear(); feed.seenIds.clear();
  feed.queuedIds.clear(); feed.failedIds.clear(); feed.recentIds = [];
  feed.recycleMode = false; feed.page = null;
  feedReleaseWakeLock(); el('#feed').hidden = true;
}

function feedBuildReviewControls(section, item) {
  const card = document.createElement('div'); card.className = 'feed-review-card';
  while (section.firstChild) card.appendChild(section.firstChild);
  section.appendChild(card);
  const stamp = document.createElement('div'); stamp.className = 'feed-swipe-stamp';
  stamp.setAttribute('aria-hidden', 'true'); card.appendChild(stamp);
  const panel = document.createElement('div'); panel.className = 'feed-review-controls';
  const label = document.createElement('div'); label.textContent = `AUTO ${item.auto_rating} - Needs review`;
  panel.appendChild(label);
  const countdown = document.createElement('small'); countdown.className = 'feed-review-countdown';
  panel.appendChild(countdown);
  const stopTimer = () => {
    clearTimeout(section._timer); section._timer = null;
    clearInterval(section._countdownTimer); section._countdownTimer = null;
    countdown.textContent = '';
  };
  const startTimer = () => {
    stopTimer();
    if (!feed.active || feed.activeSection !== section || saving) return;
    let remaining = 10;
    countdown.textContent = 'Skip in 10s';
    section._countdownTimer = setInterval(() => { countdown.textContent = `Skip in ${Math.max(0, --remaining)}s`; }, 1000);
    section._timer = setTimeout(() => {
      stopTimer();
      if (feed.active && feed.activeSection === section && !saving) feedGoNext(section);
    }, 10000);
  };
  section._onReviewActivate = async backwards => {
    if (backwards && item.rating_reviewed && section._reviewToken && !saving) {
      saving = true; stopTimer();
      try {
        const result = await api(`/api/media/${item.id}/rating/undo`, {method:'POST', body:JSON.stringify({rating_reviewed_at:section._reviewToken})});
        Object.assign(item, result);
        const original = state.currentItems.find(m => m.id === item.id);
        if (original) Object.assign(original, result);
        section._reviewToken = null;
        label.textContent = `AUTO ${item.auto_rating} - Review undone`;
        refreshStars(item.auto_rating);
        panel.querySelectorAll('button').forEach(b => b.disabled = false);
      } catch (e) { toast('Could not undo: ' + e.message, true); }
      finally { saving = false; }
    }
    startTimer();
  };
  let saving = false, start = null, suppressClick = false;
  const resetDrag = () => {
    card.style.transform = ''; card.classList.remove('dragging');
    stamp.style.opacity = '0';
  };
  const animate = async (direction) => {
    resetDrag();
    if (!card.animate || window.matchMedia('(prefers-reduced-motion: reduce)').matches) return;
    const target = direction === 'approve' ? 'translateX(110%) rotate(14deg)' :
      direction === 'next' ? 'translateY(-35%) scale(.94)' : 'translateX(-24px) rotate(-2deg)';
    const animation = card.animate([
      {transform:'none', opacity:1},
      {transform:target, opacity:direction === 'choose' ? 1 : 0}
    ], {duration:direction === 'choose' ? 180 : 260, easing:'ease-out', direction:direction === 'choose' ? 'alternate' : 'normal', iterations:direction === 'choose' ? 2 : 1});
    try { await animation.finished; } catch (_) {}
  };
  const save = async rating => {
    if (saving || item.rating_reviewed) { resetDrag(); return; }
    saving = true; stopTimer();
    const session = feed.session;
    panel.querySelectorAll('button').forEach(b => b.disabled = true);
    resetDrag();
    try {
      const result = await api(`/api/media/${item.id}/rating${rating == null ? '/approve' : ''}`, {
        method:rating == null ? 'POST' : 'PUT', ...(rating == null ? {} : {body:JSON.stringify({rating})})
      });
      Object.assign(item, result);
      section._reviewToken = result.rating_reviewed_at;
      const original = state.currentItems.find(m => m.id === item.id);
      if (original) Object.assign(original, result);
      if (!feed.active || session !== feed.session) return;
      label.textContent = `AUTO ${item.auto_rating} -> HUMAN ${item.rating}`;
      refreshStars(item.rating);
      toast(label.textContent);
      await animate(rating == null ? 'approve' : 'next');
      if (feed.active && session === feed.session && feed.activeSection === section) feedGoNext(section);
    } catch (e) {
      toast('Could not save rating: ' + e.message, true);
      panel.querySelectorAll('button').forEach(b => b.disabled = false);
    } finally { saving = false; if (!item.rating_reviewed) startTimer(); }
  };
  const stars = document.createElement('div'); stars.className = 'feed-review-stars';
  stars.setAttribute('role', 'group'); stars.setAttribute('aria-label', 'Choose human rating');
  const choices = [];
  const refreshStars = rating => choices.forEach((b, i) => {
    b.classList.toggle('filled', i < rating);
    b.setAttribute('aria-pressed', String(i + 1 === rating));
  });
  for (let r = 1; r <= 5; r++) {
    const b = document.createElement('button'); b.className = 'star'; b.textContent = '\u2605';
    b.setAttribute('aria-label', `Rate ${r} ${r === 1 ? 'star' : 'stars'}`);
    b.title = `${r} / 5`; b.addEventListener('click', () => save(r));
    choices.push(b); stars.appendChild(b);
  }
  refreshStars(item.auto_rating); panel.appendChild(stars);
  const button = (text, action) => {
    const b = document.createElement('button'); b.className = 'btn'; b.textContent = text;
    b.addEventListener('click', action); panel.appendChild(b); return b;
  };
  button('Approve', () => save(null));
  button('Previous / undo', () => {
    if (saving) return;
    const previous = section.previousElementSibling;
    if (previous) previous.scrollIntoView({behavior:'smooth',block:'start'});
  });
  button('Skip', async () => {
    if (saving) return;
    saving = true; stopTimer();
    const session = feed.session;
    await animate('next');
    if (feed.active && session === feed.session && feed.activeSection === section) feedGoNext(section);
    saving = false;
  });
  const hint = document.createElement('small'); hint.textContent = 'Swipe right: approve / left: choose stars / up: skip / down: previous & undo';
  panel.appendChild(hint); card.appendChild(panel);
  section.addEventListener('click', e => {
    if (suppressClick) { e.preventDefault(); e.stopPropagation(); suppressClick = false; }
  }, true);
  section.addEventListener('pointerdown', e => {
    suppressClick = false;
    if (saving || !e.isPrimary || e.target.closest('button, .feed-progress')) return;
    start = {x:e.clientX, y:e.clientY};
  });
  section.addEventListener('pointermove', e => {
    if (!start || saving) return;
    const dx = e.clientX - start.x, dy = e.clientY - start.y;
    if (Math.abs(dx) < 8 || Math.abs(dx) < Math.abs(dy) * 1.5) return;
    section.setPointerCapture(e.pointerId);
    suppressClick = true; card.classList.add('dragging');
    const shift = Math.max(-180, Math.min(180, dx));
    card.style.transform = `translateX(${shift}px) rotate(${shift / 22}deg)`;
    stamp.textContent = dx > 0 ? 'APPROVE' : 'CHOOSE STARS';
    stamp.classList.toggle('choose', dx < 0);
    stamp.style.opacity = String(Math.min(1, Math.abs(dx) / 90));
  });
  section.addEventListener('pointercancel', () => { start = null; resetDrag(); });
  section.addEventListener('pointerup', e => {
    if (!start) return;
    const dx = e.clientX - start.x, dy = e.clientY - start.y; start = null;
    if (section.hasPointerCapture(e.pointerId)) section.releasePointerCapture(e.pointerId);
    resetDrag();
    if (saving || Math.abs(dx) < 60 || Math.abs(dx) < Math.abs(dy) * 1.5) return;
    suppressClick = true;
    if (dx > 0) save(null);
    else {
      choices[0].focus(); label.textContent = `AUTO ${item.auto_rating} - Choose a human star rating`;
      animate('choose');
    }
  });
}

// ---------------------------------------------------------------------
// VR — WebXR immersive viewing via Three.js (loaded from CDN in
// index.html). Images only for now: a video in VR needs a live <video>
// element wrapped in THREE.VideoTexture, which is meaningfully more
// complexity on top of something that's already hard to verify without
// real headset hardware — skipped for this first version. Videos in the
// current view are just left out of the VR rotation, not shown broken.
//
// Honesty note for whoever reads this next: this was written and
// syntax/logic-tested (texture loading, aspect-ratio math, the flat
// WebGL fallback path) but never verified on an actual headset — nobody
// building this had one to test against. If something about the in-VR
// experience itself is wrong, that's the most likely place.
// ---------------------------------------------------------------------

const vr = {
  active: false,
  items: [],
  index: 0,
  renderer: null,
  scene: null,
  camera: null,
  plane: null,
  timer: null,
};

async function vrCheckSupport() {
  if (!('xr' in navigator)) return; // no WebXR in this browser at all
  try {
    const supported = await navigator.xr.isSessionSupported('immersive-vr');
    if (supported) el('#vr-btn').hidden = false;
  } catch (e) {
    // isSessionSupported can itself throw in some unsupported/non-secure
    // contexts — treat that the same as "not supported" and stay hidden
  }
}

function startVRMode() {
  if (typeof THREE === 'undefined') {
    toast('Could not load the 3D library (offline, or a blocked CDN?) — VR view needs it.', true);
    return;
  }
  vr.items = state.currentItems.filter((item) => item.type === 'image');
  if (!vr.items.length) {
    toast("No photos in the current view for VR yet (video isn't supported in VR mode).", true);
    return;
  }
  vr.active = true;
  vr.index = 0;

  const container = el('#vr-container');
  container.innerHTML = '';

  vr.renderer = new THREE.WebGLRenderer({ antialias: true });
  vr.renderer.setPixelRatio(window.devicePixelRatio);
  vr.renderer.setSize(container.clientWidth, container.clientHeight);
  vr.renderer.xr.enabled = true;
  container.appendChild(vr.renderer.domElement);

  vr.scene = new THREE.Scene();
  vr.scene.background = new THREE.Color(0x000000);
  vr.camera = new THREE.PerspectiveCamera(70, container.clientWidth / container.clientHeight, 0.1, 100);
  vr.camera.position.set(0, 1.6, 0); // roughly standing eye height

  vr.scene.add(new THREE.AmbientLight(0xffffff, 1.0));

  const geometry = new THREE.PlaneGeometry(1, 1);
  const material = new THREE.MeshBasicMaterial({ color: 0x222222 });
  vr.plane = new THREE.Mesh(geometry, material);
  vr.plane.position.set(0, 1.6, -2.5); // a couple meters ahead, eye height
  vr.scene.add(vr.plane);

  el('#vr-overlay').hidden = false;
  vr.renderer.setAnimationLoop(() => vr.renderer.render(vr.scene, vr.camera));
  window.addEventListener('resize', onVRResize);

  vrLoadCurrent();

  // The flat view above works regardless; this additionally offers the
  // real headset session where one's actually available. Desktop Chrome
  // with no headset attached, for instance, still renders the fallback
  // fine, it just never leaves the 2D page.
  navigator.xr.isSessionSupported('immersive-vr').then((supported) => {
    if (!supported || !vr.active) return;
    navigator.xr.requestSession('immersive-vr', { optionalFeatures: ['local-floor'] })
      .then((session) => {
        vr.renderer.xr.setSession(session);
        session.addEventListener('end', exitVRMode);
      })
      .catch(() => {}); // declined the permission prompt, or nothing available right now
  }).catch(() => {});
}

function onVRResize() {
  if (!vr.active) return;
  const container = el('#vr-container');
  vr.camera.aspect = container.clientWidth / container.clientHeight;
  vr.camera.updateProjectionMatrix();
  vr.renderer.setSize(container.clientWidth, container.clientHeight);
}

function vrLoadCurrent() {
  if (vr.timer) { clearTimeout(vr.timer); vr.timer = null; }
  const item = vr.items[vr.index];
  if (!item) return;
  new THREE.TextureLoader().load(
    mediaFullSrc(item),
    (texture) => {
      if (!vr.active) return; // exited while this was still loading
      const img = texture.image;
      const aspect = (img && img.width && img.height) ? img.width / img.height : 16 / 9;
      const height = 1.4; // meters — width follows the image's own aspect ratio
      vr.plane.geometry.dispose();
      vr.plane.geometry = new THREE.PlaneGeometry(height * aspect, height);
      vr.plane.material.map = texture;
      vr.plane.material.color.set(0xffffff);
      vr.plane.material.needsUpdate = true;
      vr.timer = setTimeout(vrAdvance, ss.speed);
    },
    undefined,
    () => { if (vr.active) vrAdvance(); }, // failed to load — skip to the next one
  );
}

function vrAdvance() {
  if (!vr.active) return;
  vr.index = (vr.index + 1) % vr.items.length;
  vrLoadCurrent();
}

function exitVRMode() {
  vr.active = false;
  if (vr.timer) { clearTimeout(vr.timer); vr.timer = null; }
  window.removeEventListener('resize', onVRResize);
  if (vr.renderer) {
    const session = vr.renderer.xr.getSession();
    if (session) session.end().catch(() => {});
    vr.renderer.setAnimationLoop(null);
    vr.renderer.dispose();
  }
  el('#vr-container').innerHTML = '';
  el('#vr-overlay').hidden = true;
}

function isFullscreen() {
  return !!(document.fullscreenElement || document.webkitFullscreenElement);
}

function requestBrowserFullscreen(target) {
  const req = target.requestFullscreen ? target.requestFullscreen.bind(target)
    : target.webkitRequestFullscreen ? target.webkitRequestFullscreen.bind(target)
    : null;
  if (!req) { toast('Fullscreen is not supported in this browser', true); return; }
  try {
    const p = req();
    if (p && p.catch) p.catch(() => toast('Could not enter fullscreen', true));
  } catch (_) {
    toast('Could not enter fullscreen', true);
  }
}

function exitBrowserFullscreen() {
  const exit = document.exitFullscreen ? document.exitFullscreen.bind(document)
    : document.webkitExitFullscreen ? document.webkitExitFullscreen.bind(document)
    : null;
  if (!exit) return;
  try {
    const p = exit();
    if (p && p.catch) p.catch(() => {});
  } catch (_) { /* ignore */ }
}

function toggleFullscreen(target) {
  if (isFullscreen()) exitBrowserFullscreen();
  else requestBrowserFullscreen(target || el('#slideshow'));
}

function updateFullscreenButton() {
  const ssBtn = el('#ss-fullscreen');
  if (ssBtn) ssBtn.textContent = isFullscreen() ? '⤢ exit fullscreen' : '⛶ fullscreen';
  const pwBtn = el('#pw-fullscreen');
  if (pwBtn) pwBtn.textContent = isFullscreen() ? '⤢' : '⛶';
}

let fullscreenControlsTimer = null;

// While truly fullscreen, the progress bar/controls/hint overlay the media
// instead of squeezing it — see the .slideshow:fullscreen CSS. They fade
// out after a moment of inactivity and reappear on mouse-move/tap, same as
// any fullscreen video player. In windowed mode this is a no-op: controls
// always stay visible there.
function showFullscreenControls() {
  el('#slideshow').classList.remove('controls-hidden');
  if (fullscreenControlsTimer) clearTimeout(fullscreenControlsTimer);
  if (isFullscreen()) {
    fullscreenControlsTimer = setTimeout(() => {
      el('#slideshow').classList.add('controls-hidden');
    }, 2500);
  }
}

function onFullscreenChange() {
  updateFullscreenButton();
  showFullscreenControls();
}

function renderSlide() {
  clearAdvanceTimer();
  detachVideoListeners();
  resetProgressBar();

  const stage = el('#slideshow-stage');
  stage.innerHTML = '';
  const item = ss.items[ss.index];
  rememberPlaybackItem(item);
  const src = mediaFullSrc(item);

  if (item.type === 'video') {
    const v = document.createElement('video');
    bindVirtualClip(v, item);
    v.src = src;
    v.className = 'slideshow-media';
    v.playsInline = true;
    v.controls = true;
    stage.appendChild(v);
    ss.videoEl = v;
    v.addEventListener('timeupdate', onVideoTimeUpdate);
    v.addEventListener('ended', onVideoEnded);
    v.addEventListener('play', onVideoPlay);
    v.addEventListener('pause', onVideoPause);
    if (ss.playing) attemptVideoPlay(v, el('#slideshow-stage'));
  } else {
    const img = document.createElement('img');
    img.src = src;
    img.className = 'slideshow-media';
    img.alt = item.filename;
    stage.appendChild(img);
    ss.videoEl = null;
    if (ss.playing) startImageTimer();
  }

  updateSlideshowUI();
}

function attemptVideoPlay(v, container) {
  const p = v.play();
  if (p && p.catch) {
    p.catch(() => {
      v.muted = true;
      v.play().catch(() => {});
      showUnmuteHint(v, container);
    });
  }
}

function showUnmuteHint(v, container) {
  const hint = document.createElement('button');
  hint.className = 'unmute-hint';
  hint.textContent = '🔇 tap to unmute';
  hint.addEventListener('click', () => { v.muted = false; hint.remove(); });
  container.appendChild(hint);
}

function onVideoTimeUpdate() {
  if (!ss.videoEl || !ss.videoEl.duration) return;
  const fill = el('#slideshow-progress-fill');
  fill.style.transition = 'none';
  fill.style.width = (clipProgress(ss.videoEl) * 100) + '%';
}

function onVideoEnded() {
  if (ss.playing) advanceSlide();
}

function onVideoPlay() {
  // keeps the ⏸/▶ button honest if playback was started from the native
  // video controls instead of our own button
  if (!ss.playing) { ss.playing = true; updateSlideshowUI(); }
}

function onVideoPause() {
  // Reaching the natural end of a video fires 'pause' immediately before
  // 'ended' (playback stops, so paused becomes true, per the HTML spec) —
  // without this guard, that "pause" gets treated as if the user had
  // manually paused, flipping ss.playing to false a moment before
  // onVideoEnded checks that very flag. Net effect: videos never actually
  // advanced the slideshow, silently, on every single completion.
  if (ss.videoEl && (ss.videoEl.ended || ss.videoEl._clipEnded)) return;
  if (ss.playing) { ss.playing = false; updateSlideshowUI(); }
}

function detachVideoListeners() {
  if (ss.videoEl) {
    ss.videoEl.removeEventListener('ended', onVideoEnded);
    ss.videoEl.removeEventListener('timeupdate', onVideoTimeUpdate);
    ss.videoEl.removeEventListener('play', onVideoPlay);
    ss.videoEl.removeEventListener('pause', onVideoPause);
    ss.videoEl.pause();
    ss.videoEl = null;
  }
}

function startImageTimer() {
  resetProgressBar();
  requestAnimationFrame(() => {
    const fill = el('#slideshow-progress-fill');
    fill.style.transition = `width ${ss.speed}ms linear`;
    fill.style.width = '100%';
  });
  ss.timer = setTimeout(advanceSlide, ss.speed);
}

function clearAdvanceTimer() {
  if (ss.timer) { clearTimeout(ss.timer); ss.timer = null; }
}

function resetProgressBar() {
  const fill = el('#slideshow-progress-fill');
  fill.style.transition = 'none';
  fill.style.width = '0%';
}

function restartImageTimerIfNeeded() {
  const item = ss.items[ss.index];
  if (ss.active && item && item.type !== 'video' && ss.playing) {
    clearAdvanceTimer();
    startImageTimer();
  }
}

async function advanceSlide() {
  clearAdvanceTimer();
  let next = ss.index + 1;
  if (next >= ss.items.length && mediaPage.more) { await loadMoreMedia(); if (!ss.active) return; }
  if (next >= ss.items.length) {
    if (ss.loop) { next = 0; }
    else { ss.playing = false; updateSlideshowUI(); return; }
  }
  ss.index = next;
  renderSlide();
}

async function ssStep(delta) {
  if (delta > 0 && ss.index + delta >= ss.items.length && mediaPage.more) await loadMoreMedia();
  const n = ss.items.length;
  if (!n) return;
  ss.index = (ss.index + delta + n) % n;
  renderSlide();
}

function ssTogglePlay() {
  ss.playing = !ss.playing;
  const item = ss.items[ss.index];
  if (ss.playing) {
    if (item.type === 'video' && ss.videoEl) attemptVideoPlay(ss.videoEl, el('#slideshow-stage'));
    else startImageTimer();
  } else {
    clearAdvanceTimer();
    if (ss.videoEl) ss.videoEl.pause();
  }
  updateSlideshowUI();
}

function reshuffleSlideshowInPlace() {
  const current = ss.items[ss.index];
  if (ss.shuffleMode) {
    ss.items = shuffleArray(ss.items.slice());
  } else {
    ss.items = state.currentItems.slice();
  }
  ss.index = Math.max(0, ss.items.indexOf(current));
}

function updateSlideshowUI() {
  el('#slideshow-index').textContent = `${pad4(ss.index + 1)} / ${pad4(ss.items.length)}`;
  el('#ss-playpause').textContent = ss.playing ? '⏸ pause' : '▶ play';
}

// ---------------------------------------------------------------------
// keyboard shortcuts
// ---------------------------------------------------------------------

function onKeydown(e) {
  if (sourceMenuOpenFor !== null && e.key === 'Escape') {
    closeSourceMenu();
    return;
  }
  if (el('.sidebar').classList.contains('open') && e.key === 'Escape') {
    closeSidebarDrawer();
    return;
  }
  if (!el('#slideshow').hidden) {
    if (e.key === ' ') { e.preventDefault(); ssTogglePlay(); }
    else if (e.key === 'ArrowRight') ssStep(1);
    else if (e.key === 'ArrowLeft') ssStep(-1);
    else if (e.key === 'f' || e.key === 'F') toggleFullscreen();
    else if (e.key === 'Escape') exitSlideshow();
    return;
  }
  if (!el('#lightbox').hidden) {
    if (e.key === 'ArrowRight') stepLightbox(1);
    else if (e.key === 'ArrowLeft') stepLightbox(-1);
    else if (e.key === 'Escape') closeLightbox();
    return;
  }
  if (!el('#portrait-wall').hidden) {
    if (e.key === 'Escape') exitPortraitWall();
    else if (e.key === 'f' || e.key === 'F') toggleFullscreen(el('#portrait-wall'));
    return;
  }
  if (!el('#feed').hidden) {
    if (e.key === 'Escape') exitFeed();
    return;
  }
  if (!el('#vr-overlay').hidden) {
    if (e.key === 'Escape') exitVRMode();
    return;
  }
  if (!el('#add-modal').hidden) {
    if (e.key === 'Escape') closeAddModal();
    return;
  }
  if (!el('#settings-modal').hidden) {
    if (e.key === 'Escape') closeSettingsModal();
  }
}
