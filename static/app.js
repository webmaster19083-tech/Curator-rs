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
  return item.downloaded === 0 ? item.origin_url : `/library/${encodeURI(item.filepath)}`;
}
function mediaThumbSrc(item) {
  // No local file to thumbnail yet, so this is the one place a
  // placeholder is slightly more expensive than a real item: the grid
  // loads the actual full-size remote image instead of a small cached
  // JPEG. Same tradeoff live browse already makes; once scan_and_index
  // retires this row it gets the real, fast thumbnail like everything
  // else automatically.
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
    try { const j = await res.json(); msg = j.detail || msg; } catch (_) { /* ignore */ }
    throw new Error(msg);
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
  typeFilter: 'all',       // 'all' | 'image' | 'video' — applied client-side in loadView()
  sortOrder: 'default',    // one of _MEDIA_SORT_ORDERS' keys server-side
  tagFilter: '',           // tag name, or '' for no filter
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
  default_slideshow_speed: 3000,
  default_slideshow_loop: true,
  default_slideshow_shuffle: false,
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
  bindLiveBrowseUI();
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
  applyTheme(appSettings.theme);

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

// "system" isn't a real palette — it resolves to the OS's own light/dark
// preference, live-updating if that preference changes while the app is
// open (e.g. the OS auto-switches at sunset).
let systemThemeMedia = null;

function applyTheme(theme) {
  if (systemThemeMedia) {
    systemThemeMedia.onchange = null;
    systemThemeMedia = null;
  }
  if (theme === 'system') {
    systemThemeMedia = window.matchMedia('(prefers-color-scheme: light)');
    const resolve = () => {
      if (systemThemeMedia.matches) document.documentElement.dataset.theme = 'light';
      else delete document.documentElement.dataset.theme; // dark = the base palette, no override needed
    };
    resolve();
    systemThemeMedia.onchange = resolve;
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

  el('#export-btn').addEventListener('click', exportSources);
  el('#chpack-export-btn').addEventListener('click', exportChpack);
  el('#import-trigger-btn').addEventListener('click', triggerImportPicker);
  el('#import-file-input').addEventListener('change', handleImportFile);
  el('#export-reminder-export-btn').addEventListener('click', exportSources);
  el('#export-reminder-snooze-btn').addEventListener('click', snoozeExportReminder);

  el('#live-browse-btn').addEventListener('click', openLiveBrowse);

  el('.tab[data-view="all"]').addEventListener('click', () => switchView({ type: 'all' }));

  el('#shuffle-btn').addEventListener('click', shuffleCurrentGrid);
  el('#sort-select').addEventListener('change', (e) => {
    state.sortOrder = e.target.value;
    loadView();
  });
  el('#tag-filter-select').addEventListener('change', (e) => {
    state.tagFilter = e.target.value;
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
  el('#feed-btn').addEventListener('click', startFeed);
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

  if (state.sources.length === 0) {
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
  el('#settings-theme').value = appSettings.theme;
  el('#settings-default-speed').value = appSettings.default_slideshow_speed;
  el('#settings-default-loop').checked = !!appSettings.default_slideshow_loop;
  el('#settings-default-shuffle').checked = !!appSettings.default_slideshow_shuffle;
  el('#settings-export-reminder-days').value = appSettings.export_reminder_days;
  el('#settings-modal').hidden = false;
}
function closeSettingsModal() { el('#settings-modal').hidden = true; }

async function saveSettings() {
  const rawConcurrent = parseInt(el('#settings-max-concurrent').value, 10);
  const maxConcurrent = Number.isFinite(rawConcurrent) ? Math.max(1, Math.min(20, rawConcurrent)) : 6;
  const rawReminderDays = parseInt(el('#settings-export-reminder-days').value, 10);
  const reminderDays = Number.isFinite(rawReminderDays) ? Math.max(1, Math.min(365, rawReminderDays)) : 30;
  const body = {
    max_concurrent: maxConcurrent,
    theme: el('#settings-theme').value,
    default_slideshow_speed: parseInt(el('#settings-default-speed').value, 10),
    default_slideshow_loop: el('#settings-default-loop').checked,
    default_slideshow_shuffle: el('#settings-default-shuffle').checked,
    export_reminder_days: reminderDays,
  };
  try {
    const data = await api('/api/settings', { method: 'PATCH', body: JSON.stringify(body) });
    appSettings = { ...appSettings, ...data };
    applyTheme(appSettings.theme);
    closeSettingsModal();
    toast('Settings saved');
    renderExportReminderBanner();
  } catch (e) {
    toast('Could not save settings: ' + e.message, true);
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
// CockHero .chpack export
// ---------------------------------------------------------------------

async function exportChpack() {
  // Build payload — scope to current source if one is selected, else whole library.
  const sourceId = state.view.type === 'creator' ? state.view.id : null;
  const sourceName = sourceId != null ? (state.sourcesById[sourceId]?.name || '') : '';

  // Prompt for pack name — pre-fill with source name or a default.
  const defaultName = sourceName || 'Curator Pack';
  const packName = window.prompt('Pack name for CockHero:', defaultName);
  if (packName === null) return; // cancelled

  const author = window.prompt('Author name:', 'Curator') ?? 'Curator';
  const description = window.prompt('Description (optional):', '') ?? '';

  const body = {
    name: packName.trim() || defaultName,
    author: author.trim() || 'Curator',
    description: description.trim(),
    unlock_cost: 0,
  };
  if (sourceId != null) body.source_id = sourceId;

  const scope = sourceId != null
    ? `source "${state.sourcesById[sourceId]?.name || sourceId}"`
    : 'entire library';
  toast(`Building .chpack for ${scope}…`);

  try {
    const resp = await fetch('/api/export/chpack', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
    if (!resp.ok) {
      const err = await resp.json().catch(() => ({ detail: resp.statusText }));
      throw new Error(err.detail || resp.statusText);
    }
    const blob = await resp.blob();
    // Derive filename from Content-Disposition or fall back.
    const cd = resp.headers.get('Content-Disposition') || '';
    const match = cd.match(/filename="([^"]+)"/);
    const filename = match ? match[1] : `${body.name.replace(/[^\w\-. ]/g, '_')}.chpack`;
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = filename;
    document.body.appendChild(a);
    a.click();
    a.remove();
    URL.revokeObjectURL(url);
    toast(`Downloaded ${filename}`);
  } catch (e) {
    toast('chpack export failed: ' + e.message, true);
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

async function loadView() {
  // Tag this call. If a newer loadView() starts (user navigated again)
  // before this one's fetch resolves, this stale call must not be allowed
  // to overwrite whatever the newer call already rendered — without this,
  // switching sources quickly could show you a source you'd already
  // navigated away from, landing over whatever you're actually looking at.
  const requestId = ++viewRequestSeq;
  let items;
  let loadError = null;
  const sorting = state.sortOrder && state.sortOrder !== 'default';
  const extraParams =
    (sorting ? `&sort=${state.sortOrder}` : '') +
    (state.tagFilter ? `&tag=${encodeURIComponent(state.tagFilter)}` : '');
  try {
    if (state.view.type === 'creator') {
      const data = await api(`/api/media?source_id=${state.view.id}${extraParams}`);
      items = data.media;
    } else if (state.view.type === 'group') {
      const data = await api(`/api/media?group_id=${state.view.id}${extraParams}`);
      // An explicit sort should win — only shuffle when there isn't one.
      items = sorting ? data.media : shuffleArray(data.media);
    } else {
      const data = await api(`/api/media?only_included=true${extraParams}`);
      items = sorting ? data.media : shuffleArray(data.media);
    }
  } catch (e) {
    loadError = e;
    items = [];
  }

  if (requestId !== viewRequestSeq) return; // superseded by a later navigation — drop it

  if (loadError) toast('Could not load media: ' + loadError.message, true);
  if (state.typeFilter !== 'all') {
    items = items.filter((item) => item.type === state.typeFilter);
  }
  state.currentItems = items;
  toggleEmptyState(items.length === 0);
  renderNextPage(true);
}

function toggleEmptyState(isEmpty) {
  el('#empty-state').hidden = !isEmpty;
  el('#grid').hidden = isEmpty;
}

function renderNextPage(reset = false) {
  if (reset) {
    state.page = 0;
    el('#grid').innerHTML = '';
  }
  const start = state.page * state.PAGE_SIZE;
  const slice = state.currentItems.slice(start, start + state.PAGE_SIZE);
  if (slice.length === 0) return;
  const frag = document.createDocumentFragment();
  slice.forEach((item, i) => frag.appendChild(buildTile(item, start + i)));
  el('#grid').appendChild(frag);
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
    badge.textContent = `★${item.rating}`;
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
  state.currentItems = shuffleArray(state.currentItems.slice());
  renderNextPage(true);
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

function stepLightbox(delta) {
  const n = state.currentItems.length;
  if (!n) return;
  state.lightboxIndex = (state.lightboxIndex + delta + n) % n;
  renderLightboxItem();
}

function renderLightboxItem() {
  const item = state.currentItems[state.lightboxIndex];
  if (!item) return;
  const stage = el('#lightbox-stage');
  stage.innerHTML = '';
  const src = mediaFullSrc(item);

  let mediaEl;
  if (item.type === 'video') {
    mediaEl = document.createElement('video');
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
    await api(`/api/media/${item.id}/rating`, { method: 'PUT', body: JSON.stringify({ rating }) });
    item.rating = rating;
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

function startSlideshow(startIndex) {
  if (!state.currentItems.length) { toast('Nothing to show yet.', true); return; }
  closeLightbox();
  closeSourceMenu();

  ss.items = state.currentItems.slice();
  ss.index = Math.max(0, startIndex || 0);
  ss.playing = true;
  ss.speed = parseInt(el('#ss-speed').value, 10);
  ss.loop = el('#ss-loop').checked;
  ss.shuffleMode = el('#ss-shuffle').checked;

  if (ss.shuffleMode) {
    const current = ss.items[ss.index];
    ss.items = shuffleArray(ss.items.slice());
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
  timers: [null, null, null],
  ready: [[], [], []],             // per-pane queues of already-checked {item, el}
  filling: [false, false, false],  // guards against two overlapping fill loops on one pane
};

function pwNextCandidate() {
  if (pw.queueIndex >= state.currentItems.length) return null;
  return state.currentItems[pw.queueIndex++];
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
        resolve(v.videoHeight > v.videoWidth ? { item, el: v } : null);
      };
      v.onerror = () => resolve(null);
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
      const item = pwNextCandidate();
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
// ---------------------------------------------------------------------

const FEED_BATCH_SIZE = 6;

const feed = {
  active: false,
  queueIndex: 0,
  activeSection: null,
  itemObserver: null,
  sentinelObserver: null,
};

function feedFlashIcon(iconEl, symbol) {
  iconEl.textContent = symbol;
  iconEl.classList.remove('show');
  void iconEl.offsetWidth; // force reflow so re-adding 'show' restarts the animation
  iconEl.classList.add('show');
}

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
  if (item.type === 'video') {
    mediaEl = document.createElement('video');
    mediaEl.playsInline = true;
    mediaEl.preload = 'metadata';
    mediaEl.src = mediaFullSrc(item);
    mediaEl.onloadedmetadata = () => {
      if (mediaEl.videoWidth > mediaEl.videoHeight) wrap.classList.add('rotated');
    };
    // Attached once, here, rather than on every activation — these only
    // ever fire while actually playing, which only happens while active.
    mediaEl.addEventListener('timeupdate', () => {
      if (!mediaEl.duration) return;
      fill.style.transition = 'none';
      fill.style.width = ((mediaEl.currentTime / mediaEl.duration) * 100) + '%';
    });
    mediaEl.addEventListener('ended', () => { if (feed.active) feedGoNext(section); });

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
      mediaEl.currentTime = ratio * mediaEl.duration;
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
    mediaEl.src = mediaFullSrc(item);
    mediaEl.onload = () => {
      if (mediaEl.naturalWidth > mediaEl.naturalHeight) wrap.classList.add('rotated');
    };
  }
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

  section._item = item;
  section._mediaEl = mediaEl;
  section._fill = fill;
  section._timer = null;
  return section;
}

function feedGoNext(section) {
  const next = section.nextElementSibling;
  if (next && next.classList.contains('feed-item')) {
    next.scrollIntoView({ behavior: 'smooth', block: 'start' });
  }
  // If there's nothing next yet, the sentinel/pagination will add more as
  // the user scrolls anyway — nothing to do here in that case.
}

function feedActivate(section) {
  if (feed.activeSection === section) return;
  if (feed.activeSection) feedDeactivate(feed.activeSection);
  feed.activeSection = section;

  const fill = section._fill;
  fill.style.transition = 'none';
  fill.style.width = '0%';

  const item = section._item;
  const mediaEl = section._mediaEl;
  if (item.type === 'video') {
    attemptVideoPlay(mediaEl, section);
  } else {
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
}

function feedDeactivate(section) {
  if (section._timer) { clearTimeout(section._timer); section._timer = null; }
  const mediaEl = section._mediaEl;
  if (mediaEl && mediaEl.tagName === 'VIDEO') mediaEl.pause();
  if (feed.activeSection === section) feed.activeSection = null;
}

function feedAppendBatch(count) {
  const scrollEl = el('#feed-scroll');
  const sentinel = el('#feed-sentinel');
  let appended = 0;
  while (appended < count) {
    if (feed.queueIndex >= state.currentItems.length) break;
    const item = state.currentItems[feed.queueIndex++];
    const section = feedBuildItem(item);
    scrollEl.insertBefore(section, sentinel);
    feed.itemObserver.observe(section);
    appended++;
  }
}

function startFeed() {
  if (!state.currentItems.length) { toast('Nothing to show here.', true); return; }
  feed.active = true;
  feed.queueIndex = 0;
  feed.activeSection = null;

  const scrollEl = el('#feed-scroll');
  scrollEl.innerHTML = '';
  const sentinel = document.createElement('div');
  sentinel.id = 'feed-sentinel';
  scrollEl.appendChild(sentinel);

  feed.itemObserver = new IntersectionObserver((entries) => {
    entries.forEach((entry) => {
      if (entry.isIntersecting && entry.intersectionRatio > 0.6) feedActivate(entry.target);
    });
  }, { root: scrollEl, threshold: [0, 0.6, 1] });

  feed.sentinelObserver = new IntersectionObserver((entries) => {
    if (entries[0].isIntersecting) feedAppendBatch(FEED_BATCH_SIZE);
  }, { root: scrollEl, rootMargin: '200% 0px' }); // start the next batch well before actually hitting bottom
  feed.sentinelObserver.observe(sentinel);

  el('#feed').hidden = false;
  scrollEl.scrollTop = 0;
  feedAppendBatch(FEED_BATCH_SIZE);
}

function exitFeed() {
  feed.active = false;
  if (feed.activeSection) feedDeactivate(feed.activeSection);
  if (feed.itemObserver) { feed.itemObserver.disconnect(); feed.itemObserver = null; }
  if (feed.sentinelObserver) { feed.sentinelObserver.disconnect(); feed.sentinelObserver = null; }
  el('#feed-scroll').querySelectorAll('video').forEach((v) => v.pause());
  el('#feed-scroll').innerHTML = '';
  el('#feed').hidden = true;
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
  const src = mediaFullSrc(item);

  if (item.type === 'video') {
    const v = document.createElement('video');
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
  fill.style.width = ((ss.videoEl.currentTime / ss.videoEl.duration) * 100) + '%';
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
  if (ss.videoEl && ss.videoEl.ended) return;
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

function advanceSlide() {
  clearAdvanceTimer();
  let next = ss.index + 1;
  if (next >= ss.items.length) {
    if (ss.loop) { next = 0; }
    else { ss.playing = false; updateSlideshowUI(); return; }
  }
  ss.index = next;
  renderSlide();
}

function ssStep(delta) {
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

// ---------------------------------------------------------------------
// live browse — merged in from the separate "Contact Sheet" project.
// Streams straight from gallery-dl's -j listing output (/api/preview/*
// on the backend) instead of downloading — nothing here touches
// state.sources/state.currentItems or the real grid/lightbox above.
// Namespaced with a pv- prefix (short for "preview") throughout.
// ---------------------------------------------------------------------

const pv = {
  creators: new Map(),   // url -> {url, name, items, enabled, loading, error}
  view: 'creator',        // 'creator' | 'all'
  selected: null,          // url of the creator currently shown in 'creator' view
  shuffleOn: false,
  order: [],               // current display order (array of item objects)
  lbIndex: -1,
  slideshowOn: false,
  slideshowTimer: null,
  lastCandidates: [],
  selectedSearchKeys: new Set(),
};

const PV_CACHE_TTL_MS = 6 * 60 * 60 * 1000; // 6h, same as Contact Sheet's original
const PV_TEMPLATES_KEY = 'curator:pv_search_templates';
const PV_ADD_CONCURRENCY = 2; // gentle scan concurrency, same reasoning as the original

function pvLoadTemplates() {
  try { return JSON.parse(localStorage.getItem(PV_TEMPLATES_KEY)) || []; }
  catch (_) { return []; }
}
function pvSaveTemplates(list) {
  try { localStorage.setItem(PV_TEMPLATES_KEY, JSON.stringify(list)); } catch (_) { /* ignore */ }
}
let pvSearchTemplates = pvLoadTemplates();

function pvCacheGet(url) {
  try {
    const raw = localStorage.getItem('curator:pv_cache:' + url);
    if (!raw) return null;
    const parsed = JSON.parse(raw);
    if (Date.now() - parsed.ts > PV_CACHE_TTL_MS) return null;
    return parsed.items;
  } catch (_) { return null; }
}
function pvCacheSet(url, items) {
  try { localStorage.setItem('curator:pv_cache:' + url, JSON.stringify({ items, ts: Date.now() })); }
  catch (_) { /* storage full/disabled — non-critical, just skip caching */ }
}

function bindLiveBrowseUI() {
  el('#pv-close').addEventListener('click', closeLiveBrowse);
  el('#live-browse').addEventListener('click', (e) => { if (e.target.id === 'live-browse') closeLiveBrowse(); });

  el('#pv-add-form').addEventListener('submit', pvHandleAddSubmit);
  el('#pv-add-url').addEventListener('input', (e) => {
    e.target.rows = Math.min(6, Math.max(1, e.target.value.split('\n').length));
  });
  el('#pv-add-url').addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); el('#pv-add-form').requestSubmit(); }
  });

  document.querySelectorAll('.pv-view-btn').forEach((b) => b.addEventListener('click', () => {
    pv.view = b.dataset.pvView;
    if (pv.view === 'all') pv.shuffleOn = true; // sensible default, user can turn back off
    pvSyncViewButtons();
    el('#pv-shuffle-btn').classList.toggle('active', pv.shuffleOn);
    pvRenderSidebar();
    pvRebuildOrder();
  }));

  el('#pv-shuffle-btn').addEventListener('click', () => {
    pv.shuffleOn = !pv.shuffleOn;
    el('#pv-shuffle-btn').classList.toggle('active', pv.shuffleOn);
    pvRebuildOrder();
  });
  el('#pv-slideshow-btn').addEventListener('click', () => { pv.slideshowOn ? pvStopSlideshow() : pvStartSlideshow(); });
  el('#pv-speed-select').addEventListener('change', () => {
    if (pv.slideshowOn) pvRenderLightboxItem(); // re-arm with the new speed if mid-slideshow on an image
  });

  el('#pv-lb-close').addEventListener('click', closePvLightbox);
  el('#pv-lb-prev').addEventListener('click', () => { pvPrevSlide(); });
  el('#pv-lb-next').addEventListener('click', () => { pvNextSlide(); });
  el('#pv-lightbox').addEventListener('click', (e) => { if (e.target.id === 'pv-lightbox') closePvLightbox(); });
  el('#pv-lb-add-to-library').addEventListener('click', () => {
    const item = pv.order[pv.lbIndex];
    if (item) pvAddToLibrary(item.source, el('#pv-lb-add-to-library'));
  });
  enableSwipeNav(el('#pv-lb-stage'), () => pvPrevSlide(), () => pvNextSlide());

  el('#pv-search-toggle').addEventListener('click', () => {
    const panel = el('#pv-search-panel');
    panel.hidden = !panel.hidden;
    if (!panel.hidden) { pvRenderChips(); el('#pv-search-query').focus(); }
  });
  el('#pv-search-btn').addEventListener('click', pvRunSearch);
  el('#pv-search-query').addEventListener('keydown', (e) => { if (e.key === 'Enter') pvRunSearch(); });
  el('#pv-search-select-all').addEventListener('change', (e) => {
    const boxes = document.querySelectorAll('.pv-result-select:not(:disabled)');
    boxes.forEach((b) => {
      b.checked = e.target.checked;
      if (b.checked) pv.selectedSearchKeys.add(b.dataset.key); else pv.selectedSearchKeys.delete(b.dataset.key);
    });
    pvUpdateBatchBar();
  });
  el('#pv-search-add-selected').addEventListener('click', () => {
    let count = 0;
    for (const cand of pv.lastCandidates) {
      const key = pvCandidateKey(cand);
      if (pv.selectedSearchKeys.has(key)) { pvAddCandidateAsSource(key, cand, { silent: true }); count++; }
    }
    pvRenderSidebar();
    pvRebuildOrder();
    el('#pv-search-status').textContent = `${count} source${count === 1 ? '' : 's'} added`;
    pvRenderResults(pv.lastCandidates);
  });

  el('#pv-manage-sites-btn').addEventListener('click', () => {
    pvRenderSitesEditor();
    el('#pv-sites-modal').hidden = false;
  });
  el('#pv-sites-close-btn').addEventListener('click', () => {
    el('#pv-sites-modal').hidden = true;
    pvRenderChips();
  });
  el('#pv-sites-modal').addEventListener('click', (e) => { if (e.target.id === 'pv-sites-modal') { el('#pv-sites-modal').hidden = true; pvRenderChips(); } });
  el('#pv-site-add-btn').addEventListener('click', () => {
    const label = el('#pv-site-label').value.trim();
    const url = el('#pv-site-url').value.trim();
    if (!label || !url) return;
    if (!url.includes('{query}')) {
      toast('The URL template needs a {query} placeholder, e.g. https://example.com/search?q={query}', true);
      return;
    }
    pvSearchTemplates.push({ label, url, enabled: true });
    pvSaveTemplates(pvSearchTemplates);
    el('#pv-site-label').value = '';
    el('#pv-site-url').value = '';
    pvRenderSitesEditor();
  });
}

function openLiveBrowse() {
  el('#live-browse').hidden = false;
  pvSyncViewButtons();
  pvRenderSidebar();
  pvRenderGrid();
}
function closeLiveBrowse() {
  el('#live-browse').hidden = true;
  closePvLightbox();
}

function pvParseUrlList(raw) {
  return Array.from(new Set(raw.split(/[\n,]+/).map((s) => s.trim()).filter(Boolean)));
}

async function pvRunWithConcurrency(items, limit, worker) {
  let i = 0;
  const results = new Array(items.length);
  async function lane() {
    while (i < items.length) {
      const idx = i++;
      results[idx] = await worker(items[idx], idx);
    }
  }
  await Promise.all(Array.from({ length: Math.min(limit, items.length) }, lane));
  return results;
}

async function pvScanUrl(url, force) {
  if (!force) {
    const cached = pvCacheGet(url);
    if (cached) return cached;
  }
  const items = await pvScanUrlSse(url);
  pvCacheSet(url, items);
  return items;
}

// The backend now serves /api/preview/scan as Server-Sent Events (one `item`
// event per discovered file, then a closing `done`/`error` event) instead of
// a single blocking JSON response — this lets the connection stay alive with
// keep-alive pings for the full duration of a slow gallery-dl scan rather
// than sitting on a bare fetch with no feedback. Everything downstream of
// pvScanUrl still just wants a plain items array back, so this stays a thin
// wrapper: it consumes the stream itself and resolves once it closes, and
// nothing else in the live-browse code needs to know the wire format changed.
async function pvScanUrlSse(url) {
  const res = await fetch('/api/preview/scan?url=' + encodeURIComponent(url));
  if (!res.ok || !res.body) {
    let msg = res.statusText;
    try { const j = await res.json(); msg = j.error || msg; } catch (_) { /* ignore */ }
    throw new Error(msg);
  }

  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buf = '';
  const items = [];
  let errorMsg = null;

  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    buf += decoder.decode(value, { stream: true });

    let idx;
    while ((idx = buf.indexOf('\n\n')) !== -1) {
      const rawEvent = buf.slice(0, idx);
      buf = buf.slice(idx + 2);

      let eventName = 'message';
      let data = '';
      for (const line of rawEvent.split('\n')) {
        if (line.startsWith('event:')) eventName = line.slice(6).trim();
        else if (line.startsWith('data:')) data += line.slice(5).trim();
      }

      if (eventName === 'item') {
        try { items.push(JSON.parse(data)); } catch (_) { /* skip malformed event */ }
      } else if (eventName === 'error') {
        errorMsg = data || 'gallery-dl returned an error';
      }
      // 'done' event carries no payload we need — its arrival just means
      // the loop above will end naturally when the stream closes.
    }
  }

  if (items.length === 0 && errorMsg) throw new Error(errorMsg);
  return items;
}

function pvCreatorName(url, items) {
  const first = items && items[0];
  if (first && first.creator && !first.creator.startsWith('http')) return first.creator;
  try {
    const u = new URL(url);
    return u.hostname.replace(/^www\./, '') + u.pathname.replace(/\/+$/, '');
  } catch (_) { return url; }
}

async function pvScanAndAddEntry(url, force = false) {
  const existing = pv.creators.get(url);
  if (existing && !existing.error && !force) return { url, ok: true, skipped: true };

  const entry = existing || { url, name: url, items: [], enabled: true };
  entry.loading = true;
  entry.error = null;
  pv.creators.set(url, entry);
  pvRenderSidebar();

  try {
    const items = await pvScanUrl(url, force);
    entry.items = items;
    entry.name = pvCreatorName(url, items);
    entry.loading = false;
    entry.error = null;
    pv.creators.set(url, entry);
    return { url, ok: true };
  } catch (err) {
    entry.loading = false;
    entry.error = err.message;
    pv.creators.set(url, entry);
    return { url, ok: false, error: err.message };
  }
}

function pvRemoveCreator(url) {
  pv.creators.delete(url);
  if (pv.selected === url) pv.selected = null;
  pvRenderSidebar();
  pvRebuildOrder();
}

function pvSyncViewButtons() {
  document.querySelectorAll('.pv-view-btn').forEach((b) => b.classList.toggle('active', b.dataset.pvView === pv.view));
}

function pvRenderSidebar() {
  const list = el('#pv-source-list');
  list.innerHTML = '';
  if (pv.creators.size === 0) {
    list.innerHTML = '<div class="pv-empty-hint mono small muted">no sources yet — paste a URL above</div>';
    return;
  }
  for (const entry of pv.creators.values()) {
    const row = document.createElement('div');
    row.className = 'pv-source-item' + (entry.url === pv.selected ? ' selected' : '');

    const cb = document.createElement('input');
    cb.type = 'checkbox';
    cb.checked = entry.enabled;
    cb.title = 'include in "all, shuffled"';
    cb.addEventListener('click', (e) => e.stopPropagation());
    cb.addEventListener('change', () => { entry.enabled = cb.checked; if (pv.view === 'all') pvRebuildOrder(); });

    const info = document.createElement('div');
    info.className = 'pv-source-info';
    const nameEl = document.createElement('div');
    nameEl.className = 'pv-source-name';
    nameEl.textContent = entry.name;
    info.appendChild(nameEl);
    if (entry.loading) {
      const l = document.createElement('div');
      l.className = 'pv-source-loading';
      l.textContent = 'scanning…';
      info.appendChild(l);
    } else if (entry.error) {
      const e2 = document.createElement('div');
      e2.className = 'pv-source-error';
      e2.textContent = 'error — click to retry';
      info.appendChild(e2);
    } else {
      const c = document.createElement('div');
      c.className = 'pv-source-count';
      c.textContent = entry.items.length + ' items';
      info.appendChild(c);
    }

    const addLibBtn = document.createElement('button');
    addLibBtn.className = 'pv-source-remove';
    addLibBtn.textContent = '+';
    addLibBtn.title = 'Add to library (start a real download)';
    addLibBtn.addEventListener('click', (e) => { e.stopPropagation(); pvAddToLibrary(entry.url, addLibBtn); });

    const removeBtn = document.createElement('button');
    removeBtn.className = 'pv-source-remove';
    removeBtn.textContent = '✕';
    removeBtn.title = 'Remove';
    removeBtn.addEventListener('click', (e) => { e.stopPropagation(); pvRemoveCreator(entry.url); });

    row.appendChild(cb);
    row.appendChild(info);
    row.appendChild(addLibBtn);
    row.appendChild(removeBtn);
    row.addEventListener('click', () => {
      if (entry.error) { pvScanAndAddEntry(entry.url, true).then(() => { pvRenderSidebar(); pvRebuildOrder(); }); return; }
      pv.selected = entry.url;
      pv.view = 'creator';
      pvSyncViewButtons();
      pvRenderSidebar();
      pvRebuildOrder();
    });
    list.appendChild(row);
  }
}

function pvRebuildOrder() {
  let base;
  if (pv.view === 'creator') {
    base = pv.selected && pv.creators.has(pv.selected) ? pv.creators.get(pv.selected).items : [];
  } else {
    base = [];
    for (const entry of pv.creators.values()) if (entry.enabled) base = base.concat(entry.items);
  }
  pv.order = pv.shuffleOn ? shuffleArray(base.slice()) : base.slice();
  pvRenderGrid();
}

const pvIO = new IntersectionObserver((entries) => {
  for (const e of entries) {
    if (e.isIntersecting) {
      const img = e.target.querySelector('img[data-src]');
      if (img) { img.src = img.dataset.src; img.removeAttribute('data-src'); }
      pvIO.unobserve(e.target);
    }
  }
}, { rootMargin: '300px' });

function pvRenderGrid() {
  const grid = el('#pv-grid');
  grid.innerHTML = '';
  grid.classList.toggle('is-empty', pv.order.length === 0);
  el('#pv-grid-empty').hidden = pv.order.length !== 0;
  const scope = pv.view === 'all' ? 'all sources' : (pv.selected && pv.creators.get(pv.selected)?.name) || '';
  el('#pv-status').textContent = `${pv.order.length} item${pv.order.length === 1 ? '' : 's'}${scope ? ' · ' + scope : ''}`;

  const frag = document.createDocumentFragment();
  pv.order.forEach((item, idx) => {
    const cell = document.createElement('div');
    cell.className = 'pv-cell';
    cell.addEventListener('click', () => openPvLightbox(idx));

    if (item.type === 'image') {
      const img = document.createElement('img');
      img.dataset.src = item.url;
      img.loading = 'lazy';
      img.alt = item.title || '';
      cell.appendChild(img);
      pvIO.observe(cell);
    } else {
      if (item.poster) {
        const img = document.createElement('img');
        img.dataset.src = item.poster;
        img.loading = 'lazy';
        cell.appendChild(img);
        pvIO.observe(cell);
      }
      const play = document.createElement('div');
      play.className = 'pv-play-icon';
      play.textContent = '▶';
      cell.appendChild(play);
      const badge = document.createElement('div');
      badge.className = 'pv-badge';
      badge.textContent = 'video';
      cell.appendChild(badge);
    }
    frag.appendChild(cell);
  });
  grid.appendChild(frag);
}

function openPvLightbox(idx) {
  pv.lbIndex = idx;
  pvRenderLightboxItem();
  el('#pv-lightbox').hidden = false;
}
function closePvLightbox() {
  el('#pv-lightbox').hidden = true;
  el('#pv-lb-stage').innerHTML = '';
  pvStopSlideshow();
}

function pvRenderLightboxItem() {
  const item = pv.order[pv.lbIndex];
  if (!item) return;
  const stage = el('#pv-lb-stage');
  stage.innerHTML = '';
  el('#pv-lb-meta').textContent = item.creator || '';
  clearTimeout(pv.slideshowTimer);

  if (item.type === 'image') {
    const img = document.createElement('img');
    img.src = item.url;
    img.alt = item.title || '';
    stage.appendChild(img);
    if (pv.slideshowOn) {
      const ms = parseInt(el('#pv-speed-select').value, 10);
      pv.slideshowTimer = setTimeout(pvNextSlide, ms);
    }
  } else {
    const vid = document.createElement('video');
    vid.src = item.url;
    vid.controls = true;
    vid.autoplay = true;
    vid.playsInline = true;
    if (pv.slideshowOn) vid.addEventListener('ended', () => pvNextSlide());
    stage.appendChild(vid);
  }
  const addBtn = el('#pv-lb-add-to-library');
  addBtn.textContent = '+ add to library';
  addBtn.disabled = false;
}

function pvNextSlide() {
  if (pv.order.length === 0) return;
  pv.lbIndex = (pv.lbIndex + 1) % pv.order.length;
  pvRenderLightboxItem();
}
function pvPrevSlide() {
  if (pv.order.length === 0) return;
  pv.lbIndex = (pv.lbIndex - 1 + pv.order.length) % pv.order.length;
  pvRenderLightboxItem();
}

function pvStartSlideshow() {
  if (pv.order.length === 0) return;
  pv.slideshowOn = true;
  el('#pv-slideshow-btn').textContent = '⏸ pause';
  if (el('#pv-lightbox').hidden) openPvLightbox(0);
  else pvRenderLightboxItem();
}
function pvStopSlideshow() {
  pv.slideshowOn = false;
  clearTimeout(pv.slideshowTimer);
  el('#pv-slideshow-btn').textContent = '▶ slideshow';
}

async function pvAddToLibrary(url, btnEl) {
  if (!url) return;
  if (btnEl) { btnEl.disabled = true; btnEl.textContent = btnEl.textContent.startsWith('+') ? '…' : btnEl.textContent; }
  try {
    const data = await api('/api/sources', { method: 'POST', body: JSON.stringify({ urls: [url] }) });
    if (data.sources && data.sources.length) {
      toast(`Added to library — downloading "${data.sources[0].name}" in the background`);
    } else if (data.duplicates && data.duplicates.length) {
      toast(`Already in your library: ${data.duplicates[0].name}`);
    }
    await refreshSources();
    maybeStartPolling();
    if (btnEl) btnEl.textContent = '✓';
  } catch (e) {
    toast('Could not add to library: ' + e.message, true);
    if (btnEl) { btnEl.disabled = false; btnEl.textContent = '+'; }
  }
}

async function pvHandleAddSubmit(e) {
  e.preventDefault();
  const textarea = el('#pv-add-url');
  const urls = pvParseUrlList(textarea.value);
  if (urls.length === 0) return;
  textarea.value = '';
  textarea.rows = 1;

  const addBtn = el('#pv-add-btn');
  addBtn.disabled = true;
  let done = 0;
  const failed = [];
  const status = el('#pv-add-status');
  status.textContent = urls.length > 1 ? `scanning 0/${urls.length}…` : 'scanning…';

  const results = await pvRunWithConcurrency(urls, PV_ADD_CONCURRENCY, async (url) => {
    const r = await pvScanAndAddEntry(url);
    done++;
    status.textContent = urls.length > 1 ? `scanning ${done}/${urls.length}…` : 'scanning…';
    pvRenderSidebar();
    pvRebuildOrder();
    if (!r.ok && !r.skipped) failed.push(`${url}: ${r.error}`);
    return r;
  });

  addBtn.disabled = false;
  const firstOk = results.find((r) => r.ok);
  if (firstOk) { pv.selected = firstOk.url; pv.view = 'creator'; pvSyncViewButtons(); }
  pvRenderSidebar();
  pvRebuildOrder();

  if (failed.length) {
    const okCount = results.filter((r) => r.ok).length;
    status.innerHTML = `<span class="pv-err">${okCount}/${urls.length} added — ${escapeHtml(failed[0])}${failed.length > 1 ? ` (+${failed.length - 1} more)` : ''}</span>`;
  } else {
    status.textContent = urls.length > 1 ? `${urls.length} sources added` : '';
    setTimeout(() => { if (status.textContent && !status.querySelector('.pv-err')) status.textContent = ''; }, 4000);
  }
}

// ---------------------------------------------------------------------
// live browse — search panel
// ---------------------------------------------------------------------

function pvRenderChips() {
  const chips = el('#pv-search-chips');
  chips.innerHTML = '';
  if (pvSearchTemplates.length === 0) {
    chips.innerHTML = '<span class="pv-chip pv-chip-empty mono small">no sites configured — click ⚙ sites to add one</span>';
    return;
  }
  pvSearchTemplates.forEach((t, i) => {
    const chip = document.createElement('label');
    chip.className = 'pv-chip mono small';
    const cb = document.createElement('input');
    cb.type = 'checkbox';
    cb.checked = t.enabled !== false;
    cb.addEventListener('change', () => { pvSearchTemplates[i].enabled = cb.checked; pvSaveTemplates(pvSearchTemplates); });
    chip.appendChild(cb);
    chip.appendChild(document.createTextNode(t.label));
    chips.appendChild(chip);
  });
}

async function pvRunSearch() {
  const query = el('#pv-search-query').value.trim();
  if (!query) return;
  const active = pvSearchTemplates.filter((t) => t.enabled !== false);
  const status = el('#pv-search-status');
  if (active.length === 0) {
    status.innerHTML = '<span class="pv-err">No search sites enabled — click ⚙ sites to add one.</span>';
    return;
  }
  const btn = el('#pv-search-btn');
  btn.disabled = true;
  btn.textContent = '…';
  status.textContent = 'searching ' + active.map((t) => t.label).join(', ') + ' …';
  el('#pv-search-results').innerHTML = '';

  try {
    const data = await api('/api/preview/search', {
      method: 'POST',
      body: JSON.stringify({ query, templates: active }),
    });
    pvRenderResults(data.candidates || []);
    const errLines = (data.errors || []).map((e) => `<span class="pv-err">${escapeHtml(e)}</span>`).join('');
    status.innerHTML = (data.candidates && data.candidates.length
      ? data.candidates.length + ' match' + (data.candidates.length === 1 ? '' : 'es')
      : 'no matches') + errLines;
  } catch (e) {
    status.innerHTML = '<span class="pv-err">' + escapeHtml(e.message) + '</span>';
  } finally {
    btn.disabled = false;
    btn.textContent = 'Search';
  }
}

function pvCandidateKey(cand) { return cand.site + '||' + cand.creator; }

function pvUpdateBatchBar() {
  const addable = pv.lastCandidates.filter((c) => !pv.creators.has(pvCandidateKey(c)));
  el('#pv-search-batch-bar').hidden = pv.lastCandidates.length === 0;
  el('#pv-search-selected-count').textContent = pv.selectedSearchKeys.size;
  el('#pv-search-add-selected').disabled = pv.selectedSearchKeys.size === 0;
  el('#pv-search-select-all').disabled = addable.length === 0;
  el('#pv-search-select-all').checked = addable.length > 0 && pv.selectedSearchKeys.size === addable.length;
}

function pvRenderResults(candidates) {
  pv.lastCandidates = candidates;
  pv.selectedSearchKeys = new Set();
  const results = el('#pv-search-results');
  results.innerHTML = '';
  for (const cand of candidates) {
    const key = pvCandidateKey(cand);
    const already = pv.creators.has(key);
    const card = document.createElement('div');
    card.className = 'pv-result-card';

    const cb = document.createElement('input');
    cb.type = 'checkbox';
    cb.className = 'pv-result-select';
    cb.dataset.key = key;
    if (already) { cb.disabled = true; cb.title = 'already added'; }
    cb.addEventListener('change', () => {
      if (cb.checked) pv.selectedSearchKeys.add(key); else pv.selectedSearchKeys.delete(key);
      pvUpdateBatchBar();
    });
    card.appendChild(cb);

    if (cand.poster) {
      const img = document.createElement('img');
      img.className = 'pv-result-thumb';
      img.loading = 'lazy';
      img.src = cand.poster;
      card.appendChild(img);
    } else {
      const ph = document.createElement('div');
      ph.className = 'pv-result-thumb placeholder';
      ph.textContent = '◍';
      card.appendChild(ph);
    }

    const info = document.createElement('div');
    info.className = 'pv-result-info';
    const name = document.createElement('div');
    name.className = 'pv-result-name';
    name.textContent = cand.creator;
    const meta = document.createElement('div');
    meta.className = 'pv-result-meta';
    meta.textContent = cand.site + ' · ' + cand.items.length + ' found';
    const addBtn2 = document.createElement('button');
    addBtn2.className = 'pv-result-add';
    addBtn2.textContent = already ? 'added ✓' : '+ add source';
    if (already) addBtn2.classList.add('added');
    addBtn2.addEventListener('click', () => {
      pvAddCandidateAsSource(key, cand);
      addBtn2.textContent = 'added ✓';
      addBtn2.classList.add('added');
      cb.checked = false;
      cb.disabled = true;
      pv.selectedSearchKeys.delete(key);
      pvUpdateBatchBar();
    });

    info.appendChild(name);
    info.appendChild(meta);
    info.appendChild(addBtn2);
    card.appendChild(info);
    results.appendChild(card);
  }
  pvUpdateBatchBar();
}

function pvAddCandidateAsSource(key, cand, opts = {}) {
  const entry = { url: key, name: cand.creator + ' — ' + cand.site, items: cand.items, enabled: true, loading: false, error: null };
  pv.creators.set(key, entry);
  pvCacheSet(key, cand.items);
  if (!opts.silent) {
    pv.selected = key;
    pv.view = 'creator';
    pvSyncViewButtons();
    pvRenderSidebar();
    pvRebuildOrder();
  }
}

function pvRenderSitesEditor() {
  const editor = el('#pv-sites-editor');
  editor.innerHTML = '';
  if (pvSearchTemplates.length === 0) {
    editor.innerHTML = '<div class="pv-sites-empty">no sites yet — add one below</div>';
    return;
  }
  pvSearchTemplates.forEach((t, i) => {
    const row = document.createElement('div');
    row.className = 'pv-site-row';
    const label = document.createElement('span');
    label.className = 'pv-site-row-label';
    label.textContent = t.label;
    const url = document.createElement('span');
    url.className = 'pv-site-row-url';
    url.textContent = t.url;
    const removeBtn = document.createElement('button');
    removeBtn.textContent = '✕';
    removeBtn.title = 'Remove';
    removeBtn.addEventListener('click', () => {
      pvSearchTemplates.splice(i, 1);
      pvSaveTemplates(pvSearchTemplates);
      pvRenderSitesEditor();
      pvRenderChips();
    });
    row.appendChild(label);
    row.appendChild(url);
    row.appendChild(removeBtn);
    editor.appendChild(row);
  });
}

function onKeydown(e) {
  if (!el('#pv-sites-modal').hidden) {
    if (e.key === 'Escape') { el('#pv-sites-modal').hidden = true; pvRenderChips(); }
    return;
  }
  if (!el('#pv-lightbox').hidden) {
    if (e.key === 'ArrowRight') pvNextSlide();
    else if (e.key === 'ArrowLeft') pvPrevSlide();
    else if (e.key === 'Escape') closePvLightbox();
    return;
  }
  if (!el('#live-browse').hidden) {
    if (e.key === 'Escape') closeLiveBrowse();
    return;
  }
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
