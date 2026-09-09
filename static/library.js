'use strict';

// The original app remains Curator's media and playback engine. This adapter
// replaces its surrounding surface with a single Explorer-style library so
// existing downloads, media URLs, lightbox, feed, and Tauri protocol paths
// remain intact.

const explorerLegacy = {
  buildTile,
  loadView,
  switchView,
  renderSidebar,
  populateTagFilterOptions,
  exitSlideshow,
};

const explorer = {
  installed: false,
  active: 'media',
  nav: 'all',
  selected: new Set(),
  selectionAnchor: null,
  searchQuery: '',
  searchTimer: null,
  panelRequest: 0,
  searchResults: [],
  searchSelected: new Set(),
  playMode: localStorage.getItem('curator-last-play-mode') || 'slideshow',
  goon: null,
};

function normalizePlayMode(mode) {
  return ({ 'mobile-feed': 'feed', 'portrait-wall': 'portrait' })[mode] || mode;
}

state.selectedMediaIds = explorer.selected;
state.explorerSection = 'media';

function explorerEl(selector, root = document) { return root.querySelector(selector); }
function explorerAll(selector, root = document) { return [...root.querySelectorAll(selector)]; }

function effectiveRating(item) {
  const value = item?.effective_rating ?? item?.human_rating ?? item?.rating ?? item?.auto_rating ?? 0;
  return Number.isFinite(Number(value)) ? Number(value) : 0;
}

function formatBytes(value) {
  if (value == null || value === '') return '—';
  const number = Number(value);
  if (!Number.isFinite(number) || number < 0) return '—';
  if (number < 1024) return `${number} B`;
  const unit = Math.min(4, Math.floor(Math.log(number) / Math.log(1024)));
  return `${(number / 1024 ** unit).toLocaleString(undefined, { maximumFractionDigits: 1 })} ${['B', 'KB', 'MB', 'GB', 'TB'][unit]}`;
}

function formatDuration(value) {
  const seconds = Number(value);
  if (!Number.isFinite(seconds) || seconds <= 0) return '—';
  const total = Math.round(seconds);
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const remaining = total % 60;
  return hours ? `${hours}:${String(minutes).padStart(2, '0')}:${String(remaining).padStart(2, '0')}` : `${minutes}:${String(remaining).padStart(2, '0')}`;
}

function formatDate(value) {
  if (!value) return '—';
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return String(value).slice(0, 10);
  return new Intl.DateTimeFormat(undefined, { year: 'numeric', month: 'short', day: 'numeric' }).format(date);
}

function sourceFor(item) { return state.sourcesById?.[item.source_id] || null; }
function sourceLabel(item) { return sourceFor(item)?.name || item.creator || item.source || '—'; }
function sourceHost(item) {
  const raw = item.origin_url || sourceFor(item)?.url || item.source_url || '';
  try { return new URL(raw).hostname.replace(/^www\./, ''); } catch (_) { return raw || '—'; }
}

function setExplorerVisible(mediaVisible) {
  const panel = explorerEl('#explorer-panel');
  const columns = explorerEl('#explorer-columns');
  const grid = explorerEl('#grid');
  const sentinel = explorerEl('#grid-sentinel');
  if (panel) panel.hidden = mediaVisible;
  if (columns) columns.hidden = !mediaVisible;
  if (grid) grid.hidden = !mediaVisible;
  if (sentinel) sentinel.hidden = !mediaVisible;
  if (!mediaVisible) explorerEl('#empty-state')?.setAttribute('hidden', '');
}

function updateExplorerLocation(label) {
  const node = explorerEl('#explorer-location');
  if (node) node.textContent = label;
}

function updateNavigation() {
  if (!explorer.installed) return;
  explorerAll('[data-nav]').forEach((button) => button.classList.toggle('active', button.dataset.nav === explorer.nav));
  const activeDownloads = state.sources.filter((source) => source.status === 'pending' || source.status === 'downloading').length;
  const badge = explorerEl('#sidebar-download-count');
  if (badge) {
    badge.hidden = activeDownloads === 0;
    badge.textContent = activeDownloads ? String(activeDownloads) : '';
  }
  const stats = explorerEl('#explorer-stats');
  if (stats) {
    const items = state.sources.reduce((sum, source) => sum + Number(source.item_count || 0), 0);
    stats.textContent = `${state.sources.length} sources · ${items.toLocaleString()} items`;
  }
}

function updateBulkUI() {
  if (!explorer.installed) return;
  const count = explorer.selected.size;
  const bar = explorerEl('#explorer-bulk-bar');
  const label = explorerEl('#explorer-selection-count');
  if (bar) bar.hidden = count === 0;
  if (label) label.textContent = `${count} selected`;
  const selectAll = explorerEl('#explorer-select-all');
  if (selectAll) {
    const visibleIds = state.currentItems.map((item) => item.id);
    selectAll.checked = visibleIds.length > 0 && visibleIds.every((id) => explorer.selected.has(id));
    selectAll.indeterminate = !selectAll.checked && visibleIds.some((id) => explorer.selected.has(id));
  }
  explorerAll('.explorer-row').forEach((row) => {
    const selected = explorer.selected.has(Number(row.dataset.mediaId));
    row.classList.toggle('selected', selected);
    const checkbox = explorerEl('input[type="checkbox"]', row);
    if (checkbox) checkbox.checked = selected;
  });
}

function clearExplorerSelection() {
  explorer.selected.clear();
  explorer.selectionAnchor = null;
  updateBulkUI();
}

function selectExplorerItem(id, index, event) {
  const range = event?.shiftKey && explorer.selectionAnchor != null;
  const toggle = event?.ctrlKey || event?.metaKey;
  if (range) {
    const from = Math.min(explorer.selectionAnchor, index);
    const to = Math.max(explorer.selectionAnchor, index);
    if (!toggle) explorer.selected.clear();
    state.currentItems.slice(from, to + 1).forEach((item) => explorer.selected.add(item.id));
  } else if (toggle) {
    if (explorer.selected.has(id)) explorer.selected.delete(id);
    else explorer.selected.add(id);
    explorer.selectionAnchor = index;
  } else {
    explorer.selected.clear();
    explorer.selected.add(id);
    explorer.selectionAnchor = index;
  }
  updateBulkUI();
}

function selectAllVisible() {
  const items = state.currentItems;
  const allSelected = items.length > 0 && items.every((item) => explorer.selected.has(item.id));
  if (allSelected) explorer.selected.clear();
  else items.forEach((item) => explorer.selected.add(item.id));
  updateBulkUI();
}

function explorerBuildTile(item, index) {
  const row = document.createElement('article');
  row.className = `explorer-row${item.type === 'video' ? ' explorer-row-video' : ''}`;
  row.dataset.mediaId = item.id;
  row.dataset.index = index;
  row.tabIndex = 0;
  row.setAttribute('role', 'row');
  row.setAttribute('aria-label', item.filename || `Media ${item.id}`);

  const select = document.createElement('input');
  select.type = 'checkbox';
  select.className = 'explorer-row-select';
  select.checked = explorer.selected.has(item.id);
  select.setAttribute('aria-label', `Select ${item.filename}`);
  select.addEventListener('click', (event) => event.stopPropagation());
  select.addEventListener('change', (event) => {
    if (event.target.checked) explorer.selected.add(item.id);
    else explorer.selected.delete(item.id);
    explorer.selectionAnchor = index;
    updateBulkUI();
  });
  row.append(select);

  const name = document.createElement('div');
  name.className = 'explorer-name';
  const preview = document.createElement(item.type === 'video' ? 'video' : 'img');
  preview.className = 'explorer-thumb';
  if (item.type === 'video') {
    preview.muted = true;
    preview.preload = 'metadata';
    preview.dataset.src = mediaFullSrc(item);
    preview.setAttribute('aria-hidden', 'true');
    videoLazyObserver.observe(preview);
  } else {
    preview.src = mediaThumbSrc(item);
    preview.loading = 'lazy';
    preview.alt = '';
  }
  const nameText = document.createElement('span');
  nameText.className = 'explorer-name-text';
  nameText.textContent = item.filename || 'Untitled media';
  name.append(preview, nameText);
  row.append(name);

  const creator = document.createElement('span');
  creator.className = 'explorer-creator'; creator.textContent = sourceLabel(item); creator.title = creator.textContent; row.append(creator);
  const source = document.createElement('span');
  source.className = 'explorer-source'; source.textContent = sourceHost(item); source.title = source.textContent; row.append(source);
  const duration = document.createElement('span'); duration.className = 'explorer-duration mono'; duration.textContent = formatDuration(item.duration_secs); row.append(duration);
  const size = document.createElement('span'); size.className = 'explorer-size mono'; size.textContent = formatBytes(item.file_size_bytes); row.append(size);
  const rating = document.createElement('span'); rating.className = 'explorer-rating mono';
  const value = effectiveRating(item); rating.textContent = value ? `★ ${value}` : '—';
  rating.title = item.human_rating != null ? `Human ${item.human_rating}; automatic ${item.auto_rating || 0}` : item.auto_rating ? `Automatic ${item.auto_rating}` : 'Unrated';
  if (item.human_rating != null || item.rating_reviewed) rating.classList.add('human'); row.append(rating);
  const date = document.createElement('span'); date.className = 'explorer-date mono'; date.textContent = formatDate(item.added_at); row.append(date);

  row.addEventListener('click', (event) => selectExplorerItem(item.id, index, event));
  row.addEventListener('dblclick', () => openLightbox(index));
  row.addEventListener('keydown', (event) => {
    if (event.key === ' ' || event.key === 'Spacebar') { event.preventDefault(); selectExplorerItem(item.id, index, event); }
    if (event.key === 'Enter') { event.preventDefault(); openLightbox(index); }
  });
  return row;
}

buildTile = explorerBuildTile;

function localMediaMatch(item, query) {
  const needle = query.trim().toLowerCase();
  if (!needle) return true;
  return [item.filename, sourceLabel(item), sourceHost(item), ...(item.tags || []), ...(item.inherited_tags || [])]
    .filter(Boolean)
    .some((value) => String(value).toLowerCase().includes(needle));
}

async function explorerLoadView() {
  if (explorer.active !== 'media') return renderExplorerPanel(explorer.active);
  setExplorerVisible(true);
  const result = await explorerLegacy.loadView();
  if (explorer.searchQuery.trim()) {
    state.currentItems = state.currentItems.filter((item) => localMediaMatch(item, explorer.searchQuery));
    state.renderedCount = 0;
    explorerEl('#grid').replaceChildren();
    toggleEmptyState(state.currentItems.length === 0);
    await renderNextPage(true);
  }
  updateBulkUI();
  return result;
}

loadView = explorerLoadView;

switchView = function explorerSwitchView(view) {
  explorer.active = 'media';
  state.explorerSection = 'media';
  if (view.type === 'creator') explorer.nav = 'creators';
  else if (view.type === 'group') explorer.nav = 'groups';
  updateNavigation();
  return explorerLegacy.switchView(view);
};

renderSidebar = function explorerRenderSidebar() {
  if (!explorer.installed) return;
  updateNavigation();
};

populateTagFilterOptions = function explorerPopulateTagOptions(tags) {
  explorerLegacy.populateTagFilterOptions(tags);
  const select = explorerEl('#explorer-tag-filter');
  if (!select) return;
  const previous = select.value;
  select.replaceChildren(new Option('All tags', ''));
  tags.forEach((tag) => select.add(new Option(`${tag.name} (${tag.media_count || 0})`, tag.name)));
  select.value = [...select.options].some((option) => option.value === previous) ? previous : state.tagFilter || '';
};

function mediaNavigation(section) {
  explorer.active = 'media'; state.explorerSection = 'media'; explorer.nav = section;
  state.view = { type: 'all' }; state.ratingStatus = ''; state.tagFilter = ''; state.maxRatingFilter = '';
  state.typeFilter = ({ all: 'all', images: 'image', clips: 'clip', videos: 'video' })[section] || 'all';
  if (section === 'recent') state.sortOrder = 'date_desc';
  explorer.searchQuery = '';
  const search = explorerEl('#explorer-library-search'); if (search) search.value = '';
  explorerAll('.explorer-type-filter').forEach((button) => button.classList.toggle('active', button.dataset.type === state.typeFilter));
  const sort = explorerEl('#explorer-sort'); if (sort) sort.value = state.sortOrder || 'default';
  const tag = explorerEl('#explorer-tag-filter'); if (tag) tag.value = '';
  const max = explorerEl('#explorer-max-rating'); if (max) max.value = '';
  const rating = explorerEl('#explorer-rating-status'); if (rating) rating.value = '';
  updateExplorerLocation(({ all: 'All Media', images: 'Images', clips: 'Clips', videos: 'Videos', recent: 'Recent' })[section] || 'Library');
  clearExplorerSelection(); updateNavigation(); return explorerLoadView();
}

function navigateTo(section) {
  if (['all', 'images', 'clips', 'videos', 'recent'].includes(section)) return mediaNavigation(section);
  if (section === 'review') {
    explorer.active = 'media'; state.explorerSection = 'media'; explorer.nav = 'review';
    state.view = { type: 'all' }; state.typeFilter = 'all'; state.ratingStatus = 'needs_review';
    updateExplorerLocation('Review Queue'); const rating = explorerEl('#explorer-rating-status'); if (rating) rating.value = 'needs_review';
    updateNavigation(); return explorerLoadView();
  }
  explorer.active = section; state.explorerSection = section; explorer.nav = section;
  clearExplorerSelection(); updateNavigation();
  updateExplorerLocation(({ search: 'Search', sources: 'Sources', creators: 'Creators', groups: 'Groups', tags: 'Tags', ratings: 'Ratings', downloads: 'Downloads' })[section] || 'Library');
  return renderExplorerPanel(section);
}

function makePanelHeading(title, description) {
  const heading = document.createElement('header'); heading.className = 'explorer-panel-heading';
  const h = document.createElement('h2'); h.textContent = title; heading.append(h);
  if (description) { const p = document.createElement('p'); p.textContent = description; heading.append(p); }
  return heading;
}

function panelButton(label, className = '') {
  const button = document.createElement('button');
  button.type = 'button'; button.className = `btn btn-ghost ${className}`.trim(); button.textContent = label;
  return button;
}

function showExplorerDialog(title, build) {
  const dialog = document.createElement('dialog'); dialog.className = 'explorer-dialog';
  const header = document.createElement('header'); const h = document.createElement('h2'); h.textContent = title;
  const close = panelButton('Close'); close.classList.add('explorer-dialog-close'); close.addEventListener('click', () => dialog.close());
  header.append(h, close); dialog.append(header); build(dialog); document.body.append(dialog);
  dialog.addEventListener('close', () => dialog.remove(), { once: true }); dialog.showModal(); return dialog;
}

async function chooseExplorerGroup(title = 'Choose a group') {
  return new Promise((resolve) => {
    const dialog = showExplorerDialog(title, (node) => {
      const list = document.createElement('div'); list.className = 'explorer-picker-list';
      const none = panelButton('No group'); none.addEventListener('click', () => { dialog.close(); resolve(null); }); list.append(none);
      state.groups.forEach((group) => {
        const button = panelButton(group.name); button.addEventListener('click', () => { dialog.close(); resolve(group.id); }); list.append(button);
      });
      const create = panelButton('+ New group'); create.addEventListener('click', async () => {
        const name = prompt('New group name:'); if (!name?.trim()) return;
        const created = await createGroup(name.trim()); if (created) { dialog.close(); resolve(created.id); }
      });
      node.append(list);
    });
    dialog.addEventListener('cancel', () => resolve(undefined), { once: true });
  });
}

async function doBulk(action, extra = {}) {
  const ids = [...explorer.selected]; if (!ids.length) return;
  try {
    await api('/api/media/bulk', { method: 'POST', body: JSON.stringify({ ids, action, ...extra }) });
    toast(`${ids.length} item${ids.length === 1 ? '' : 's'} updated`);
    await refreshGroups(); await refreshSources(); await refreshTagIndex(); await explorerLoadView(); clearExplorerSelection();
  } catch (error) {
    const selectedItems = state.currentItems.filter((item) => ids.includes(item.id));
    try {
      if (action === 'add_tag' && extra.tag) for (const item of selectedItems) await addTagToMedia(item, extra.tag);
      else if (action === 'set_rating' && Number.isInteger(extra.rating)) for (const item of selectedItems) await rateMedia(item, extra.rating);
      else throw error;
      toast(`${ids.length} item${ids.length === 1 ? '' : 's'} updated`); clearExplorerSelection();
    } catch (_) { toast(`Bulk action failed: ${error.message}`, true); }
  }
}

function selectedItems() { return state.currentItems.filter((item) => explorer.selected.has(item.id)); }

async function handleBulkAction(action) {
  if (action === 'clear') return clearExplorerSelection();
  if (action === 'add-group' || action === 'move') {
    const groupId = await chooseExplorerGroup(action === 'move' ? 'Move selected media' : 'Add selected media to a group');
    if (groupId !== undefined) await doBulk(action === 'move' ? 'move' : 'add_group', { group_id: groupId });
  } else if (action === 'add-tag') {
    const tag = prompt('Add tag to selected media:')?.trim(); if (tag) await doBulk('add_tag', { tag });
  } else if (action === 'set-rating') {
    const rating = Number(prompt('Set effective human rating (0–5):', '3'));
    if (Number.isInteger(rating) && rating >= 0 && rating <= 5) await doBulk('set_rating', { rating });
  } else if (action === 'delete') {
    if (confirm(`Delete ${explorer.selected.size} selected media item(s)? This can remove local files.`)) await doBulk('delete');
  } else if (action === 'review') launchPlayMode('review', selectedItems());
  else if (action === 'play') launchPlayMode(explorer.playMode, selectedItems());
  else if (action === 'refresh') await doBulk('refresh_metadata');
  else if (action === 'open-source') {
    const urls = [...new Set(selectedItems().map((item) => item.origin_url || sourceFor(item)?.url).filter(Boolean))];
    urls.slice(0, 8).forEach((url) => window.open(url, '_blank', 'noopener,noreferrer'));
  }
}

function launchSlideshowItems(items) {
  if (!items.length) { toast('Nothing to play yet.', true); return; }
  closeLightbox(); ss.items = preparePlaybackItems(items, false); ss.index = 0; ss.playing = true;
  ss.speed = Number(el('#ss-speed')?.value || appSettings.default_slideshow_speed || 3000);
  ss.loop = !!el('#ss-loop')?.checked; ss.shuffleMode = !!el('#ss-shuffle')?.checked;
  if (ss.shuffleMode && ss.items.length > 1) ss.items = preparePlaybackItems(ss.items, true);
  ss.active = true; el('#slideshow').hidden = false; renderSlide();
}

function setPlayMode(mode, persist = true) {
  const names = { feed: 'Mobile Feed', slideshow: 'Slideshow', portrait: 'Portrait Wall', review: 'Review', goon: 'GOON' };
  mode = normalizePlayMode(mode);
  explorer.playMode = names[mode] ? mode : 'slideshow'; localStorage.setItem('curator-last-play-mode', explorer.playMode);
  const primary = explorerEl('#explorer-play-primary'); if (primary) primary.textContent = `Play · ${names[explorer.playMode]}`;
  explorerAll('#explorer-play-menu [data-play-mode]').forEach((button) => {
    const current = button.dataset.playMode === explorer.playMode; button.classList.toggle('current', current); button.setAttribute('aria-checked', String(current));
  });
  if (persist) api('/api/settings', { method: 'PATCH', body: JSON.stringify({ last_play_mode: explorer.playMode }) }).catch(() => {});
}

async function restorePlayMode() {
  try {
    const settings = await api('/api/settings');
    const mode = normalizePlayMode(settings.last_play_mode);
    if (mode) setPlayMode(mode, false);
  } catch (_) {
    // Local persistence is still useful when a remote server is reconnecting.
  }
}

function launchPlayMode(mode = explorer.playMode, items = null) {
  setPlayMode(mode); const list = items?.length ? items : state.currentItems;
  if (mode === 'feed') { if (items?.length) toast('Mobile Feed uses the active library filter.'); startFeed(); }
  else if (mode === 'slideshow') launchSlideshowItems(list);
  else if (mode === 'portrait') { if (items?.length) toast('Portrait Wall uses the active library filter.'); startPortraitWall(); }
  else if (mode === 'review') startFeed(true);
  else if (mode === 'goon') startGoonSession(list);
}

const GOON_FALLBACK_PLAN = [
  { id: 'warmup', title: 'Warm up', duration_s: 90, intensity: 1, prompt: 'Settle in and find a comfortable pace.', event: 'begin' },
  { id: 'build', title: 'Build', duration_s: 180, intensity: 2, prompt: 'Keep a steady rhythm. Change media when the cue appears.', event: 'increase' },
  { id: 'focus', title: 'Focus', duration_s: 180, intensity: 3, prompt: 'Stay focused on the session cue.', event: 'hold' },
  { id: 'cooldown', title: 'Cooldown', duration_s: 60, intensity: 1, prompt: 'Slow down, breathe, and end when ready.', event: 'cooldown', terminal: true },
];

async function startGoonSession(items) {
  let payload = null;
  try { payload = await api('/api/goon/session', { method: 'POST', body: JSON.stringify({ media_ids: items.map((item) => item.id) }) }); } catch (_) {}
  const media = payload?.media || payload?.items || items.filter((item) => mediaMatchesTypeFilter(item, 'clip') || item.type === 'image');
  if (!media?.length) { toast('GOON needs at least one ready media item.', true); return; }
  const stages = payload?.stages || payload?.plan?.stages || GOON_FALLBACK_PLAN;
  launchSlideshowItems(media);
  const hud = explorerEl('#goon-hud'); hud.hidden = false;
  explorer.goon = { stages, stageIndex: -1, timer: null, interval: null, startedAt: Date.now(), payload, ended: false };
  advanceGoonStage();
}

function advanceGoonStage() {
  const session = explorer.goon; if (!session) return;
  const stage = session.stages[++session.stageIndex]; if (!stage) return endGoonSession();
  const title = explorerEl('#goon-stage-title'); const prompt = explorerEl('#goon-stage-prompt'); const intensity = explorerEl('#goon-intensity'); const time = explorerEl('#goon-stage-time');
  if (title) title.textContent = stage.title || stage.name || 'GOON';
  if (prompt) prompt.textContent = stage.prompt || stage.event || '';
  if (intensity) intensity.textContent = `Intensity ${stage.intensity ?? 1}/5`;
  const durationMs = Math.max(1, Number(stage.duration_s || stage.duration || 60)) * 1000;
  ss.speed = Math.max(500, Number(stage.media_interval_ms || stage.interval_ms || ss.speed || 3000)); restartImageTimerIfNeeded();
  const deadline = Date.now() + durationMs; clearInterval(session.interval);
  session.interval = setInterval(() => { const seconds = Math.max(0, Math.ceil((deadline - Date.now()) / 1000)); if (time) time.textContent = `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, '0')}`; }, 250);
  clearTimeout(session.timer); session.timer = setTimeout(() => { if (stage.terminal || stage.transition === 'end') endGoonSession('completed'); else { ssStep(1); advanceGoonStage(); } }, durationMs);
}

function endGoonSession(endedState = 'completed') {
  const session = explorer.goon; if (!session) return;
  session.ended = true; clearTimeout(session.timer); clearInterval(session.interval); explorerEl('#goon-hud').hidden = true; explorer.goon = null;
  api('/api/goon/session/complete', { method: 'POST', body: JSON.stringify({ duration_s: Math.round((Date.now() - session.startedAt) / 1000), stages_completed: session.stageIndex + 1, ended_state: endedState }) }).catch(() => {});
  explorerLegacy.exitSlideshow();
}

exitSlideshow = function explorerExitSlideshow() {
  if (explorer.goon) return endGoonSession('cancelled');
  return explorerLegacy.exitSlideshow();
};

function normalizeSearchResult(result) {
  return {
    title: result.title || result.name || result.source_url || 'Untitled result', creator: result.creator || result.author || 'Unknown creator',
    thumbnail: result.thumbnail || result.thumbnail_url || '', source: result.source || result.provider || '', source_url: result.source_url || result.url || '',
    provider: result.provider || result.source || 'gallery-dl', result_type: result.result_type || result.type || 'Collection',
    item_count: result.item_count ?? result.count ?? null, date: result.date || result.published_at || '', gallery_dl_compatible: result.gallery_dl_compatible !== false,
  };
}

async function runUnifiedSearch() {
  const panel = explorerEl('#explorer-panel'); const query = explorerEl('#discover-query')?.value.trim() || ''; if (!query) return;
  const provider = explorerEl('#discover-provider')?.value || ''; const resultType = explorerEl('#discover-result-type')?.value || ''; const sort = explorerEl('#discover-sort')?.value || 'relevance';
  const status = explorerEl('#discover-status'); if (status) status.textContent = 'Searching providers…';
  try {
    const params = new URLSearchParams({ query, sort }); if (provider) params.set('provider', provider); if (resultType) params.set('result_type', resultType);
    const data = await api(`/api/search?${params}`); explorer.searchResults = (data.results || data.items || []).map(normalizeSearchResult); explorer.searchSelected.clear();
    if (status) {
      const unavailable = (data.provider_errors || []).map((entry) => entry.provider).filter(Boolean);
      status.textContent = `${explorer.searchResults.length} results${unavailable.length ? ` · ${unavailable.join(', ')} unavailable` : ''}`;
    }
    renderSearchResults(panel);
  } catch (error) { if (status) status.textContent = `Search unavailable: ${error.message}`; }
}

function renderSearchResults(panel) {
  explorerEl('#discover-results', panel)?.remove();
  const results = document.createElement('div'); results.id = 'discover-results'; results.className = 'discover-results';
  const tableHead = document.createElement('div'); tableHead.className = 'discover-result head'; tableHead.innerHTML = '<span></span><span>Result</span><span>Creator</span><span>Provider</span><span>Type</span><span>Date</span><span></span>'; results.append(tableHead);
  explorer.searchResults.forEach((result, index) => {
    const row = document.createElement('article'); row.className = 'discover-result';
    const check = document.createElement('input'); check.type = 'checkbox'; check.checked = explorer.searchSelected.has(index);
    check.addEventListener('change', () => { if (check.checked) explorer.searchSelected.add(index); else explorer.searchSelected.delete(index); updateSearchSelection(); });
    const resultCell = document.createElement('div'); resultCell.className = 'discover-result-title';
    if (result.thumbnail) { const image = document.createElement('img'); image.src = result.thumbnail; image.alt = ''; image.loading = 'lazy'; resultCell.append(image); }
    const text = document.createElement('span'); text.textContent = result.title; resultCell.append(text);
    const creator = document.createElement('span'); creator.textContent = result.creator;
    const provider = document.createElement('span'); provider.textContent = result.provider;
    const type = document.createElement('span'); type.textContent = result.result_type;
    const date = document.createElement('span'); date.textContent = formatDate(result.date);
    const actions = document.createElement('div'); actions.className = 'discover-row-actions';
    const preview = panelButton('Preview'); preview.addEventListener('click', () => previewSearchResult(result));
    const source = panelButton('Open'); source.addEventListener('click', () => result.source_url && window.open(result.source_url, '_blank', 'noopener,noreferrer'));
    actions.append(preview, source); row.append(check, resultCell, creator, provider, type, date, actions); results.append(row);
  });
  panel.append(results); updateSearchSelection();
}

function updateSearchSelection() {
  const count = explorerEl('#discover-selection-count'); if (count) count.textContent = `${explorer.searchSelected.size} selected`;
  const all = explorerEl('#discover-select-all');
  if (all) { all.checked = explorer.searchResults.length > 0 && explorer.searchSelected.size === explorer.searchResults.length; all.indeterminate = explorer.searchSelected.size > 0 && !all.checked; }
}

function previewSearchResult(result) {
  showExplorerDialog(result.title, (dialog) => {
    if (result.thumbnail) { const image = document.createElement('img'); image.src = result.thumbnail; image.alt = ''; image.className = 'search-preview-image'; dialog.append(image); }
    const info = document.createElement('dl'); info.className = 'search-preview';
    [['Creator', result.creator], ['Provider', result.provider], ['Type', result.result_type], ['Items', result.item_count ?? '—'], ['Date', formatDate(result.date)], ['Source', result.source_url]].forEach(([label, value]) => {
      const term = document.createElement('dt'); term.textContent = label; const description = document.createElement('dd'); description.textContent = String(value || '—'); info.append(term, description);
    }); dialog.append(info);
  });
}

async function addSearchResultsToCurator() {
  const results = [...explorer.searchSelected].map((index) => explorer.searchResults[index]).filter(Boolean); if (!results.length) return;
  try { await api('/api/search/download', { method: 'POST', body: JSON.stringify({ results }) }); toast(`${results.length} result${results.length === 1 ? '' : 's'} added to Curator`); await refreshSources(); }
  catch (error) { toast(`Could not add search results: ${error.message}`, true); }
}

async function renderSearchPanel(panel) {
  panel.replaceChildren(makePanelHeading('Search', 'Search gallery-dl-compatible providers and external indexes in one place.'));
  const controls = document.createElement('form'); controls.className = 'discover-controls'; controls.noValidate = true;
  controls.innerHTML = '<input id="discover-query" type="search" placeholder="Search creators, galleries, posts, collections" autocomplete="off"><select id="discover-provider"><option value="">All providers</option><option value="gallery-dl">gallery-dl sources</option><option value="balbums">balbums.st</option></select><select id="discover-result-type"><option value="">All result types</option><option value="creator">Creator</option><option value="album">Album/Gallery</option><option value="post">Post</option><option value="collection">Collection</option></select><select id="discover-sort"><option value="relevance">Relevance</option><option value="date_desc">Newest</option><option value="date_asc">Oldest</option></select><button class="btn btn-accent" type="submit">Search</button>';
  controls.addEventListener('submit', (event) => { event.preventDefault(); runUnifiedSearch(); }); panel.append(controls);
  const bulk = document.createElement('div'); bulk.className = 'discover-bulk'; bulk.innerHTML = '<label><input id="discover-select-all" type="checkbox"> Select all</label><span id="discover-selection-count">0 selected</span>';
  const add = panelButton('Add to Curator'); add.addEventListener('click', addSearchResultsToCurator);
  const download = panelButton('Download Selected'); download.classList.add('btn-accent'); download.addEventListener('click', addSearchResultsToCurator);
  bulk.append(add, download); panel.append(bulk);
  explorerEl('#discover-select-all', bulk).addEventListener('change', (event) => { explorer.searchSelected.clear(); if (event.target.checked) explorer.searchResults.forEach((_, index) => explorer.searchSelected.add(index)); renderSearchResults(panel); });
  const status = document.createElement('p'); status.id = 'discover-status'; status.className = 'muted'; panel.append(status);
  if (explorer.searchResults.length) renderSearchResults(panel);
}

function sourceRow(source, creatorsOnly = false) {
  const row = document.createElement('article'); row.className = 'explorer-card source-card';
  const title = document.createElement('h3'); title.textContent = source.name;
  const meta = document.createElement('p'); meta.textContent = `${source.item_count || 0} items · ${source.status || 'ready'}${source.group_id ? ` · ${state.groupsById[source.group_id]?.name || 'Group'}` : ''}`;
  const url = document.createElement('p'); url.className = 'muted mono small'; url.textContent = source.url;
  const actions = document.createElement('div'); actions.className = 'explorer-card-actions';
  const browse = panelButton(creatorsOnly ? 'Open creator' : 'Browse'); browse.addEventListener('click', () => switchView({ type: 'creator', id: source.id }));
  const open = panelButton('Open source'); open.addEventListener('click', () => window.open(source.url, '_blank', 'noopener,noreferrer'));
  const sync = panelButton('Sync'); sync.addEventListener('click', () => resyncSource(source.id));
  actions.append(browse, open, sync); row.append(title, meta, url, actions); return row;
}

function renderSourcesPanel(panel, creatorsOnly) {
  panel.replaceChildren(makePanelHeading(creatorsOnly ? 'Creators' : 'Sources', creatorsOnly ? 'Creators and galleries already added to this Curator library.' : 'Every source uses Curator’s existing gallery-dl queue.'));
  const add = panelButton('+ Add source'); add.classList.add('btn-accent'); add.addEventListener('click', () => explorerEl('#add-source-btn')?.click()); panel.append(add);
  const list = document.createElement('div'); list.className = 'explorer-card-list'; state.sources.forEach((source) => list.append(sourceRow(source, creatorsOnly))); panel.append(list);
}

function assignSourcesToGroup(groupId) {
  return showExplorerDialog('Add sources to group', (node) => {
    const list = document.createElement('div'); list.className = 'explorer-picker-list';
    state.sources.filter((source) => source.group_id !== groupId).forEach((source) => {
      const button = panelButton(source.name); button.addEventListener('click', async () => {
        try { await api(`/api/sources/${source.id}/group`, { method: 'PATCH', body: JSON.stringify({ group_id: groupId }) }); await refreshSources(); await refreshGroups(); renderExplorerPanel('groups'); }
        catch (error) { toast(`Could not add source: ${error.message}`, true); }
      }); list.append(button);
    }); node.append(list);
  });
}

function renderGroupsPanel(panel) {
  panel.replaceChildren(makePanelHeading('Groups', 'Organize sources in nested groups. Group tags remain inherited by their media.'));
  const newGroup = panelButton('+ New group'); newGroup.classList.add('btn-accent'); newGroup.addEventListener('click', () => createGroup().then(() => renderExplorerPanel('groups'))); panel.append(newGroup);
  const list = document.createElement('div'); list.className = 'explorer-card-list group-card-list';
  state.groups.forEach((group) => {
    const card = document.createElement('article'); card.className = 'explorer-card group-card';
    const title = document.createElement('h3'); title.textContent = group.name;
    const meta = document.createElement('p'); meta.textContent = `${group.source_count || 0} direct sources${group.parent_id ? ` · in ${state.groupsById[group.parent_id]?.name || 'group'}` : ''}`;
    const tags = document.createElement('div'); tags.className = 'quick-tag-row';
    (group.tags || []).slice(0, 8).forEach((tag) => { const button = panelButton(tag, 'quick-tag'); button.addEventListener('click', () => { state.tagFilter = tag; mediaNavigation('all'); const select = explorerEl('#explorer-tag-filter'); if (select) select.value = tag; }); tags.append(button); });
    const actions = document.createElement('div'); actions.className = 'explorer-card-actions';
    const browse = panelButton('Browse'); browse.addEventListener('click', () => switchView({ type: 'group', id: group.id, name: group.name }));
    const add = panelButton('+ Add to group…'); add.addEventListener('click', () => assignSourcesToGroup(group.id));
    const tag = panelButton('+ Add tag'); tag.addEventListener('click', async () => { const name = prompt('Group tag:')?.trim(); if (name) { await addTagToGroup(group.id, name); renderExplorerPanel('groups'); } });
    actions.append(browse, add, tag); card.append(title, meta, tags, actions); list.append(card);
  }); panel.append(list);
}

async function reviewSourceTag(entry, action) {
  const normalizedName = action === 'edit' ? prompt('Normalize source tag:', entry.normalized_name || entry.raw_name || entry.name) : undefined;
  if (action === 'edit' && !normalizedName?.trim()) return;
  const remember = confirm('Remember this choice for this provider?');
  try { await api('/api/source-tags/review', { method: 'POST', body: JSON.stringify({ id: entry.id, action, normalized_name: normalizedName?.trim(), remember, scope: remember ? 'provider' : 'global' }) }); renderExplorerPanel('tags'); }
  catch (error) { toast(`Could not review source tag: ${error.message}`, true); }
}

async function renderTagsPanel(panel) {
  panel.replaceChildren(makePanelHeading('Tags', 'Quick filters use human tags first, then approved source tags and automatic suggestions.'));
  let tags = []; let quick = []; let review = [];
  try { tags = (await api('/api/tags')).tags || []; } catch (_) {}
  try { quick = (await api('/api/tags/quick')).tags || []; } catch (_) { quick = tags.slice(0, 16); }
  try { review = (await api('/api/source-tags/review?limit=30')).tags || []; } catch (_) {}
  const controls = document.createElement('div'); controls.className = 'tag-panel-controls';
  const add = panelButton('+ Add tag'); add.classList.add('btn-accent'); add.addEventListener('click', async () => { const tag = prompt('New tag:')?.trim(); if (!tag) return; if (explorer.selected.size) await doBulk('add_tag', { tag }); else toast('Select media first, then use Add tag.'); }); controls.append(add); panel.append(controls);
  const quickTitle = document.createElement('h3'); quickTitle.className = 'panel-subhead'; quickTitle.textContent = 'Common & recent'; panel.append(quickTitle);
  const quickRow = document.createElement('div'); quickRow.className = 'quick-tag-row';
  quick.forEach((tag) => { const name = typeof tag === 'string' ? tag : tag.name; const button = panelButton(name, 'quick-tag'); button.addEventListener('click', () => { state.tagFilter = name; mediaNavigation('all'); const filter = explorerEl('#explorer-tag-filter'); if (filter) filter.value = name; }); quickRow.append(button); }); panel.append(quickRow);
  if (review.length) {
    const heading = document.createElement('h3'); heading.className = 'panel-subhead'; heading.textContent = 'Source tag review'; panel.append(heading);
    const reviewList = document.createElement('div'); reviewList.className = 'source-tag-review-list';
    review.forEach((entry) => {
      const row = document.createElement('article'); row.className = 'source-tag-review'; const text = document.createElement('span'); text.textContent = `${entry.raw_name || entry.name} · ${entry.provider || 'source metadata'}`;
      const addButton = panelButton('Add'); addButton.addEventListener('click', () => reviewSourceTag(entry, 'add'));
      const editButton = panelButton('Edit'); editButton.addEventListener('click', () => reviewSourceTag(entry, 'edit'));
      const skipButton = panelButton('Skip'); skipButton.addEventListener('click', () => reviewSourceTag(entry, 'skip'));
      row.append(text, addButton, editButton, skipButton); reviewList.append(row);
    }); panel.append(reviewList);
  }
}

function renderRatingsPanel(panel) {
  panel.replaceChildren(makePanelHeading('Ratings', 'Human ratings override automated ratings wherever Curator sorts, filters, reviews, and plays media.'));
  const row = document.createElement('div'); row.className = 'rating-filter-row';
  for (let value = 5; value >= 0; value--) {
    const button = panelButton(value ? `${'★'.repeat(value)} ${value}` : 'Unrated', 'rating-filter');
    button.addEventListener('click', () => { explorer.active = 'media'; state.explorerSection = 'media'; explorer.nav = 'ratings'; state.view = { type: 'all' }; state.ratingStatus = value ? '' : 'unrated'; state.sortOrder = 'rating_desc'; state.maxRatingFilter = value ? String(value) : ''; updateExplorerLocation(value ? `${value}-star media` : 'Unrated media'); explorerLoadView(); }); row.append(button);
  } panel.append(row);
}

async function renderDownloadsPanel(panel) {
  panel.replaceChildren(makePanelHeading('Downloads', 'Downloads continue in Curator’s background service even when this window is closed.'));
  const status = document.createElement('p'); status.className = 'downloads-status'; status.textContent = 'Loading status…'; panel.append(status);
  const actions = document.createElement('div'); actions.className = 'explorer-card-actions';
  const pause = panelButton('Pause downloads'); const resume = panelButton('Resume downloads'); const resync = panelButton('Sync all sources');
  pause.addEventListener('click', async () => { await api('/api/downloads/pause', { method: 'POST' }); renderExplorerPanel('downloads'); });
  resume.addEventListener('click', async () => { await api('/api/downloads/resume', { method: 'POST' }); renderExplorerPanel('downloads'); });
  resync.addEventListener('click', () => resyncAllSources()); actions.append(pause, resume, resync); panel.append(actions);
  try { const data = await api('/api/downloads/status'); status.textContent = data.paused ? `Paused · ${data.paused_source_ids?.length || 0} source(s) ready to resume` : `${data.active_count || 0} active download(s)`; pause.disabled = !!data.paused; resume.disabled = !data.paused; }
  catch (error) { status.textContent = `Status unavailable: ${error.message}`; }
}

async function renderExplorerPanel(section) {
  if (!explorer.installed) return;
  if (section === 'media') return explorerLoadView();
  const request = ++explorer.panelRequest; setExplorerVisible(false);
  const panel = explorerEl('#explorer-panel'); if (!panel) return; panel.replaceChildren();
  if (section === 'search') await renderSearchPanel(panel);
  else if (section === 'sources') renderSourcesPanel(panel, false);
  else if (section === 'creators') renderSourcesPanel(panel, true);
  else if (section === 'groups') renderGroupsPanel(panel);
  else if (section === 'tags') await renderTagsPanel(panel);
  else if (section === 'ratings') renderRatingsPanel(panel);
  else if (section === 'downloads') await renderDownloadsPanel(panel);
  if (request !== explorer.panelRequest) return;
}

function installExplorerUi() {
  const sidebar = explorerEl('.sidebar'); const main = explorerEl('.main'); const legacyToolbar = explorerEl('.toolbar', main);
  if (!sidebar || !main || !legacyToolbar || explorer.installed) return;
  explorer.installed = true;

  // Preserve old controls and their event listeners as compatibility hooks.
  const legacySidebar = document.createElement('div'); legacySidebar.className = 'legacy-sidebar'; legacySidebar.hidden = true;
  while (sidebar.firstChild) legacySidebar.append(sidebar.firstChild); sidebar.append(legacySidebar); sidebar.classList.add('explorer-sidebar');
  const navigation = document.createElement('div'); navigation.className = 'explorer-sidebar-content';
  navigation.innerHTML = '<header class="explorer-brand"><span class="brand-mark">C</span><span>CURATOR</span><button type="button" class="explorer-sidebar-close" aria-label="Close navigation">×</button></header><button id="explorer-add-source" class="explorer-add-source" type="button">+ Add source</button><nav class="explorer-navigation" aria-label="Curator navigation"><section><h2>Library</h2><button data-nav="all" type="button">All Media</button><button data-nav="images" type="button">Images</button><button data-nav="clips" type="button">Clips</button><button data-nav="videos" type="button">Videos</button></section><section><h2>Discover</h2><button data-nav="search" type="button">Search</button><button data-nav="sources" type="button">Sources</button><button data-nav="creators" type="button">Creators</button></section><section><h2>Organization</h2><button data-nav="groups" type="button">Groups</button><button data-nav="tags" type="button">Tags</button><button data-nav="ratings" type="button">Ratings</button><button data-nav="review" type="button">Review Queue</button></section><section><h2>Activity</h2><button data-nav="downloads" type="button">Downloads <span id="sidebar-download-count" class="nav-count" hidden></span></button><button data-nav="recent" type="button">Recent</button></section></nav><footer><button id="explorer-settings" type="button">Settings</button><span id="explorer-stats" class="mono small muted"></span></footer>';
  sidebar.prepend(navigation);
  explorerAll('[data-nav]', navigation).forEach((button) => button.addEventListener('click', () => navigateTo(button.dataset.nav)));
  explorerEl('#explorer-add-source', navigation).addEventListener('click', () => explorerEl('#add-source-btn')?.click());
  explorerEl('#explorer-settings', navigation).addEventListener('click', openSettingsModal);
  explorerEl('.explorer-sidebar-close', navigation).addEventListener('click', closeSidebarDrawer);

  legacyToolbar.hidden = true; legacyToolbar.classList.add('legacy-toolbar');
  const toolbar = document.createElement('header'); toolbar.className = 'explorer-toolbar';
  toolbar.innerHTML = '<div class="explorer-toolbar-top"><div><p class="explorer-kicker">Library</p><h1 id="explorer-location">All Media</h1></div><div class="explorer-toolbar-actions"><button id="explorer-add-source-main" class="btn btn-ghost" type="button">+ Add source</button><label class="explorer-search"><span class="sr-only">Search library</span><input id="explorer-library-search" type="search" placeholder="Search library" autocomplete="off"></label><div class="explorer-play-split"><button id="explorer-play-primary" class="btn btn-accent" type="button">Play</button><button id="explorer-play-toggle" class="btn btn-accent" type="button" aria-label="Choose play mode" aria-haspopup="menu" aria-expanded="false">▾</button><div id="explorer-play-menu" role="menu" hidden><button type="button" data-play-mode="feed">Mobile Feed</button><button type="button" data-play-mode="slideshow">Slideshow</button><button type="button" data-play-mode="portrait">Portrait Wall</button><button type="button" data-play-mode="review">Review</button><button type="button" data-play-mode="goon">GOON</button></div></div></div></div><div class="explorer-toolbar-filters"><div class="explorer-type-buttons"><button type="button" data-type="all" class="explorer-type-filter active">All</button><button type="button" data-type="image" class="explorer-type-filter">Images</button><button type="button" data-type="clip" class="explorer-type-filter">Clips</button><button type="button" data-type="video" class="explorer-type-filter">Videos</button></div><select id="explorer-sort" aria-label="Sort media"><option value="default">Sort: default</option><optgroup label="Name"><option value="filename_asc">Name (A–Z)</option><option value="filename_desc">Name (Z–A)</option></optgroup><optgroup label="Date"><option value="date_desc">Date added (newest)</option><option value="date_asc">Date added (oldest)</option><option value="downloaded_desc">Date downloaded (newest)</option><option value="downloaded_asc">Date downloaded (oldest)</option><option value="modified_desc">Date modified (newest)</option><option value="modified_asc">Date modified (oldest)</option></optgroup><optgroup label="Media"><option value="duration_desc">Duration (longest)</option><option value="duration_asc">Duration (shortest)</option><option value="size_desc">File size (largest)</option><option value="size_asc">File size (smallest)</option><option value="rating_desc">Rating (highest)</option><option value="rating_asc">Rating (lowest)</option></optgroup><optgroup label="Source"><option value="creator_asc">Creator (A–Z)</option><option value="creator_desc">Creator (Z–A)</option><option value="source_asc">Source (A–Z)</option><option value="source_desc">Source (Z–A)</option></optgroup><option value="shuffle">Random</option></select><select id="explorer-tag-filter" aria-label="Filter by tag"><option value="">All tags</option></select><select id="explorer-max-rating" aria-label="Maximum rating"><option value="">All ratings</option><option value="4">Up to 4 stars</option><option value="3">Up to 3 stars</option><option value="2">Up to 2 stars</option><option value="1">Up to 1 star</option></select><select id="explorer-rating-status" aria-label="Rating status"><option value="">All review states</option><option value="unrated">Unrated</option><option value="auto">Auto rated</option><option value="needs_review">Needs review</option><option value="reviewed">Human reviewed</option></select></div><div id="explorer-bulk-bar" hidden><span id="explorer-selection-count" class="mono small"></span><button type="button" data-bulk="add-group">Add to Group</button><button type="button" data-bulk="add-tag">Add Tag</button><button type="button" data-bulk="set-rating">Set Rating</button><button type="button" data-bulk="move">Move</button><button type="button" data-bulk="delete" class="danger">Delete</button><button type="button" data-bulk="review">Review</button><button type="button" data-bulk="play">Play Selected</button><button type="button" data-bulk="refresh">Refresh Metadata</button><button type="button" data-bulk="open-source">Open Source</button><button type="button" data-bulk="clear">Clear</button></div>';
  legacyToolbar.before(toolbar);
  const columns = document.createElement('div'); columns.id = 'explorer-columns'; columns.className = 'explorer-columns';
  columns.innerHTML = '<span><input id="explorer-select-all" type="checkbox" aria-label="Select all media"></span><button type="button" data-sort="filename_asc">Name</button><button type="button" data-sort="creator_asc">Creator</button><button type="button" data-sort="source_asc">Source</button><button type="button" data-sort="duration_desc">Duration</button><button type="button" data-sort="size_desc">Size</button><button type="button" data-sort="rating_desc">Rating</button><button type="button" data-sort="date_desc">Date Added</button>';
  const panel = document.createElement('section'); panel.id = 'explorer-panel'; panel.className = 'explorer-panel'; panel.hidden = true;
  legacyToolbar.after(columns, panel); explorerEl('#grid').classList.add('explorer-rows');

  explorerEl('#explorer-add-source-main').addEventListener('click', () => explorerEl('#add-source-btn')?.click());
  explorerAll('.explorer-type-filter', toolbar).forEach((button) => button.addEventListener('click', () => {
    state.typeFilter = button.dataset.type; explorer.active = 'media'; state.explorerSection = 'media'; explorer.nav = ({ all: 'all', image: 'images', clip: 'clips', video: 'videos' })[state.typeFilter] || 'all';
    explorerAll('.explorer-type-filter', toolbar).forEach((item) => item.classList.toggle('active', item === button)); clearExplorerSelection(); updateNavigation(); explorerLoadView();
  }));
  explorerEl('#explorer-sort').addEventListener('change', (event) => { state.sortOrder = event.target.value; gridShuffleSeed = state.sortOrder === 'shuffle' ? 1 + Math.floor(Math.random() * 2147483645) : null; explorerLoadView(); });
  explorerEl('#explorer-tag-filter').addEventListener('change', (event) => { state.tagFilter = event.target.value; explorerLoadView(); });
  explorerEl('#explorer-max-rating').addEventListener('change', (event) => { state.maxRatingFilter = event.target.value; explorerLoadView(); });
  explorerEl('#explorer-rating-status').addEventListener('change', (event) => { state.ratingStatus = event.target.value; explorerLoadView(); });
  explorerEl('#explorer-library-search').addEventListener('input', (event) => { clearTimeout(explorer.searchTimer); explorer.searchTimer = setTimeout(() => { explorer.searchQuery = event.target.value; explorerLoadView(); }, 180); });
  explorerEl('#explorer-select-all').addEventListener('change', selectAllVisible);
  explorerAll('[data-sort]', columns).forEach((button) => button.addEventListener('click', () => { const sort = explorerEl('#explorer-sort'); sort.value = button.dataset.sort; sort.dispatchEvent(new Event('change')); }));
  explorerEl('#explorer-bulk-bar').addEventListener('click', (event) => { const button = event.target.closest('[data-bulk]'); if (button) handleBulkAction(button.dataset.bulk); });
  const playToggle = explorerEl('#explorer-play-toggle'); const playMenu = explorerEl('#explorer-play-menu');
  playToggle.addEventListener('click', () => { playMenu.hidden = !playMenu.hidden; playToggle.setAttribute('aria-expanded', String(!playMenu.hidden)); });
  explorerEl('#explorer-play-primary').addEventListener('click', () => launchPlayMode());
  explorerAll('[data-play-mode]', playMenu).forEach((button) => button.addEventListener('click', () => { playMenu.hidden = true; playToggle.setAttribute('aria-expanded', 'false'); setPlayMode(button.dataset.playMode); launchPlayMode(button.dataset.playMode); }));
  document.addEventListener('click', (event) => { if (!event.target.closest('.explorer-play-split')) { playMenu.hidden = true; playToggle.setAttribute('aria-expanded', 'false'); } });

  const slideshow = explorerEl('#slideshow'); const goonHud = document.createElement('section'); goonHud.id = 'goon-hud'; goonHud.hidden = true;
  goonHud.innerHTML = '<div><span id="goon-stage-title">GOON</span><span id="goon-stage-time" class="mono">0:00</span></div><p id="goon-stage-prompt"></p><div><span id="goon-intensity" class="mono">Intensity 1/5</span><button id="goon-end" type="button">End session</button></div>';
  slideshow.append(goonHud); explorerEl('#goon-end').addEventListener('click', endGoonSession);
  setPlayMode(explorer.playMode, false); void restorePlayMode(); updateNavigation(); updateBulkUI();
}

document.addEventListener('DOMContentLoaded', installExplorerUi);
