/* ── Cock Hero Session Engine ─────────────────────────────────────── */
"use strict";

// ── State ────────────────────────────────────────────────────────────
const state = {
  playlist:     [],
  idx:          0,
  sessionStart: 0,
  durationMs:   0,
  intervalMs:   5000,
  paused:       false,
  timer:        null,
  paceTimer:    null,
  beatsPerMin:  0,
  started:      false,
  itemsShown:   0,
};

// Pace schedule: [fractionThrough, label, bpm]
const PACE_SCHEDULE = [
  [0.00, "SLOW",      60],
  [0.20, "MEDIUM",    90],
  [0.40, "FAST",     120],
  [0.60, "FASTER",   150],
  [0.75, "FURIOUS",  180],
  [0.85, "EDGING",    70],
  [0.92, "CUM",      200],
];

// ── DOM refs ─────────────────────────────────────────────────────────
const $ = id => document.getElementById(id);

const els = {
  setupScreen:   $("setup-screen"),
  sessionScreen: $("session-screen"),
  mediaImg:      $("media-img"),
  mediaVid:      $("media-vid"),
  mediaWrap:     $("media-wrap"),
  hudTimer:      $("hud-timer"),
  hudPace:       $("hud-pace"),
  hudCount:      $("hud-count"),
  paceFlash:     $("pace-flash"),
  paceFlashText: $("pace-flash-text"),
  progressBar:   $("progress-bar"),
  countdown:     $("countdown-overlay"),
  countdownNum:  $("countdown-number"),
  endOverlay:    $("end-overlay"),
  endStats:      $("end-stats"),
  errorMsg:      $("setup-error"),
  startBtn:      $("btn-start"),
  // form fields
  fTags:         $("f-tags"),
  fType:         $("f-type"),
  fLimit:        $("f-limit"),
  fInterval:     $("f-interval"),
  fShuffle:      $("f-shuffle"),
  fMinRating:    $("f-min-rating"),
  fDuration:     $("f-duration"),
  fBeats:        $("f-beats"),
};

// ── Setup screen ──────────────────────────────────────────────────────

async function loadDefaults() {
  try {
    const s = await api("/api/settings");
    els.fInterval.value  = Math.round((s.ch_default_interval  ?? 5));
    els.fLimit.value     = s.ch_default_limit    ?? 200;
    els.fShuffle.checked = s.ch_default_shuffle  ?? true;
    els.fType.value      = s.ch_default_media_type ?? "image";
  } catch (_) { /* no curator connection — use HTML defaults */ }
}

$("btn-start").addEventListener("click", startSession);

async function startSession() {
  els.errorMsg.textContent = "";
  els.startBtn.disabled    = true;

  const tags       = els.fTags.value.trim();
  const kind       = els.fType.value;
  const limit      = parseInt(els.fLimit.value) || 200;
  const intervalS  = parseFloat(els.fInterval.value) || 5;
  const shuffle    = els.fShuffle.checked;
  const minRating  = parseInt(els.fMinRating.value) || 0;
  const durationM  = parseFloat(els.fDuration.value) || 0;
  const beats      = els.fBeats.checked;

  const params = new URLSearchParams({ limit, shuffle });
  if (tags)      params.set("tags",       tags);
  if (kind !== "all") params.set("type",  kind);
  if (minRating) params.set("min_rating", minRating);

  let playlist;
  try {
    const data = await api(`/api/ch/playlist?${params}`);
    playlist   = data.items ?? [];
  } catch (e) {
    showError("Could not reach Curator — is it running on this machine?");
    els.startBtn.disabled = false;
    return;
  }

  if (!playlist.length) {
    showError("No matching media found. Try adjusting your filters.");
    els.startBtn.disabled = false;
    return;
  }

  state.playlist    = playlist;
  state.idx         = 0;
  state.intervalMs  = intervalS * 1000;
  state.durationMs  = durationM > 0 ? durationM * 60 * 1000 : 0;
  state.beatsPerMin = beats ? PACE_SCHEDULE[0][2] : 0;
  state.itemsShown  = 0;
  state.paused      = false;

  switchScreen("session-screen");
  runCountdown(3, () => {
    $("countdown-overlay").style.display = "none";
    state.sessionStart = Date.now();
    showItem(0);
    scheduleNext();
    startHudClock();
    if (beats) initBeatSync();
  });
}

// ── Countdown ─────────────────────────────────────────────────────────

function runCountdown(n, done) {
  const overlay = $("countdown-overlay");
  overlay.style.display = "flex";
  els.countdownNum.textContent = n;
  if (n === 0) { return done(); }
  setTimeout(() => runCountdown(n - 1, done), 800);
}

// ── Media display ─────────────────────────────────────────────────────

function showItem(idx) {
  if (idx >= state.playlist.length) return endSession();

  const item = state.playlist[idx];
  state.itemsShown++;

  if (item.type === "video") {
    els.mediaImg.style.display = "none";
    els.mediaVid.style.display = "block";
    els.mediaVid.src           = item.url;
    els.mediaVid.loop          = true;
    els.mediaVid.autoplay      = true;
    els.mediaVid.muted         = true;
    els.mediaVid.play().catch(() => {});
  } else {
    els.mediaVid.pause();
    els.mediaVid.style.display = "none";
    els.mediaImg.style.display = "block";
    els.mediaImg.src           = item.url;
  }

  els.hudCount.textContent = `${idx + 1} / ${state.playlist.length}`;
  updatePaceHud();
}

// ── Advance timer ─────────────────────────────────────────────────────

function scheduleNext() {
  clearTimeout(state.timer);
  if (state.paused) return;
  state.timer = setTimeout(() => {
    state.idx++;
    if (state.durationMs > 0 && Date.now() - state.sessionStart >= state.durationMs) {
      return endSession();
    }
    if (state.idx >= state.playlist.length) return endSession();
    showItem(state.idx);
    scheduleNext();
  }, state.intervalMs);
}

// ── HUD clock ─────────────────────────────────────────────────────────

function startHudClock() {
  (function tick() {
    if (!state.started) return;
    const elapsed  = Date.now() - state.sessionStart;
    els.hudTimer.textContent = fmtTime(elapsed);

    if (state.durationMs > 0) {
      const pct = Math.min(elapsed / state.durationMs, 1);
      els.progressBar.style.width = `${pct * 100}%`;

      const pace = paceForProgress(pct);
      if (pace.label !== els.hudPace.textContent) {
        flashPace(pace.label);
      }
      els.hudPace.textContent = pace.label;
    }

    requestAnimationFrame(tick);
  })();
  state.started = true;
}

function updatePaceHud() {
  if (!state.durationMs) { els.hudPace.textContent = "FREEPLAY"; return; }
  const pct  = (Date.now() - state.sessionStart) / state.durationMs;
  const pace = paceForProgress(pct);
  els.hudPace.textContent = pace.label;
}

function paceForProgress(pct) {
  let current = PACE_SCHEDULE[0];
  for (const p of PACE_SCHEDULE) {
    if (pct >= p[0]) current = p;
    else break;
  }
  return { label: current[1], bpm: current[2] };
}

function flashPace(label) {
  els.paceFlashText.textContent = label;
  els.paceFlash.classList.add("show");
  setTimeout(() => els.paceFlash.classList.remove("show"), 700);
}

// ── Beat sync (Web Audio API) ─────────────────────────────────────────

let audioCtx = null;

function initBeatSync() {
  audioCtx = new (window.AudioContext || window.webkitAudioContext)();
  scheduleBeat();
}

function scheduleBeat() {
  if (!audioCtx || state.paused) return;
  const bpm = state.durationMs
    ? paceForProgress((Date.now() - state.sessionStart) / state.durationMs).bpm
    : 90;
  const interval = (60 / bpm) * 1000;
  tickBeat();
  state.paceTimer = setTimeout(scheduleBeat, interval);
}

function tickBeat() {
  if (!audioCtx) return;
  const osc  = audioCtx.createOscillator();
  const gain = audioCtx.createGain();
  osc.connect(gain);
  gain.connect(audioCtx.destination);
  osc.type = "square";
  osc.frequency.setValueAtTime(200, audioCtx.currentTime);
  gain.gain.setValueAtTime(0.18, audioCtx.currentTime);
  gain.gain.exponentialRampToValueAtTime(0.001, audioCtx.currentTime + 0.08);
  osc.start(audioCtx.currentTime);
  osc.stop(audioCtx.currentTime + 0.08);
}

// ── Controls ──────────────────────────────────────────────────────────

$("btn-pause").addEventListener("click", () => {
  state.paused = !state.paused;
  $("btn-pause").textContent = state.paused ? "▶ Resume" : "⏸ Pause";
  if (!state.paused) {
    scheduleNext();
    if (audioCtx && audioCtx.state === "suspended") audioCtx.resume();
  } else {
    clearTimeout(state.timer);
    clearTimeout(state.paceTimer);
    if (audioCtx) audioCtx.suspend();
  }
});

$("btn-skip").addEventListener("click", () => {
  clearTimeout(state.timer);
  state.idx++;
  if (state.idx >= state.playlist.length) return endSession();
  showItem(state.idx);
  if (!state.paused) scheduleNext();
});

$("btn-end").addEventListener("click", endSession);

// ── End session ───────────────────────────────────────────────────────

async function endSession() {
  clearTimeout(state.timer);
  clearTimeout(state.paceTimer);
  state.started = false;

  const durationS = Math.round((Date.now() - state.sessionStart) / 1000);

  els.endStats.innerHTML = `
    <div>Duration: <strong>${fmtTime(durationS * 1000)}</strong></div>
    <div>Items shown: <strong>${state.itemsShown}</strong></div>
    <div>Playlist size: <strong>${state.playlist.length}</strong></div>
  `;

  $("end-overlay").classList.add("show");

  try {
    await api("/api/ch/session", {
      method: "POST",
      body:   JSON.stringify({ duration_s: durationS, item_count: state.itemsShown }),
    });
  } catch (_) { /* non-fatal */ }
}

$("btn-again").addEventListener("click", () => {
  $("end-overlay").classList.remove("show");
  switchScreen("setup-screen");
  els.startBtn.disabled = false;
});

// ── Utilities ─────────────────────────────────────────────────────────

function switchScreen(id) {
  document.querySelectorAll(".screen").forEach(s => s.classList.remove("active"));
  $(id).classList.add("active");
}

function showError(msg) {
  els.errorMsg.textContent = msg;
}

function fmtTime(ms) {
  const s = Math.floor(ms / 1000);
  const m = Math.floor(s / 60);
  const h = Math.floor(m / 60);
  if (h > 0) return `${h}:${pad(m % 60)}:${pad(s % 60)}`;
  return `${pad(m)}:${pad(s % 60)}`;
}
function pad(n) { return String(n).padStart(2, "0"); }

async function api(url, opts = {}) {
  const base = window.location.origin;
  const res  = await fetch(base + url, {
    headers: { "Content-Type": "application/json", ...(opts.headers ?? {}) },
    ...opts,
  });
  if (!res.ok) {
    const text = await res.text().catch(() => res.statusText);
    throw new Error(text);
  }
  return res.json();
}

// ── Boot ──────────────────────────────────────────────────────────────
switchScreen("setup-screen");
loadDefaults();
