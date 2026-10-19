// LanDrop UI — vanilla JS, no frameworks, no CDNs.
"use strict";

const $ = (id) => document.getElementById(id);

const state = {
  info: null,
  settings: null,
  peers: [],
  transfers: [],
  selectedPeerIds: [],
  /// Relative path per queued file, for folder sends (empty = flat).
  relPaths: new Map(),
  selectedFile: null,
  /// Files queued for the next send (one transfer each).
  selectedFiles: [],
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function fmtSize(bytes) {
  if (!bytes && bytes !== 0) return "?";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  let n = bytes;
  while (n >= 1024 && i < units.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(n >= 100 || i === 0 ? 0 : 1)} ${units[i]}`;
}

function fmtTime(epoch) {
  const d = new Date(epoch * 1000);
  return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

// Compact "time remaining" label ("42s", "3m 5s", "2h 4m").
function fmtEta(secs) {
  if (secs == null) return "";
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ${secs % 60}s`;
  return `${Math.floor(secs / 3600)}h ${Math.floor((secs % 3600) / 60)}m`;
}

// Build marker ("15 Oct, 20:12") so a stale install is immediately obvious.
function fmtBuild(unixSecs) {
  const n = Number(unixSecs);
  if (!n) return "unknown";
  return new Date(n * 1000).toLocaleString([], {
    day: "2-digit",
    month: "short",
    hour: "2-digit",
    minute: "2-digit",
  });
}

const ESCAPES = { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" };

// Upper bound per batch: one transfer per file, and each one is a real upload.
const MAX_BATCH_FILES = 500;
// Quote-safe: safe for both text and attribute contexts.
function esc(s) {
  return String(s == null ? "" : s).replace(/[&<>"']/g, (c) => ESCAPES[c]);
}

let lastToast = { msg: "", at: 0 };

function toast(msg, type = "info", ms = 4000) {
  const now = Date.now();
  // Never stack identical toasts (e.g. while the backend is down).
  if (msg === lastToast.msg && now - lastToast.at < 3000) return;
  lastToast = { msg, at: now };
  const el = document.createElement("div");
  el.className = `toast ${type}`;
  el.setAttribute("role", "status");
  el.textContent = msg;
  $("toasts").appendChild(el);
  setTimeout(() => el.remove(), ms);
}

async function api(path, opts = {}) {
  const res = await fetch(path, opts);
  let data = null;
  try { data = await res.json(); } catch (_) { /* empty */ }
  if (!res.ok) {
    const msg = (data && data.error) || `HTTP ${res.status}`;
    throw new Error(msg);
  }
  return data;
}

// Simple debounce so a burst of SSE events triggers one render pass.
function debounce(fn, ms) {
  let t = null;
  return () => {
    if (t) clearTimeout(t);
    t = setTimeout(fn, ms);
  };
}

// ---------------------------------------------------------------------------
// Theme (dark by default — matches the HUD reference; persisted per browser)
// ---------------------------------------------------------------------------

const THEME_KEY = "landrop.theme";

function currentTheme() {
  return document.documentElement.getAttribute("data-theme") === "light" ? "light" : "dark";
}

function setTheme(theme) {
  document.documentElement.setAttribute("data-theme", theme);
  localStorage.setItem(THEME_KEY, theme);
  for (const id of ["themeToggle", "themeToggleNav"]) {
    const btn = $(id);
    if (btn) {
      btn.setAttribute(
        "aria-label",
        `Switch to ${theme === "dark" ? "light" : "dark"} theme`
      );
    }
  }
}

function toggleTheme() {
  setTheme(currentTheme() === "dark" ? "light" : "dark");
}

// Applied immediately (this script runs at the end of <body>) so the saved
// theme is in place before first paint. Light is the default (new look).
setTheme(localStorage.getItem(THEME_KEY) === "dark" ? "dark" : "light");

// ---------------------------------------------------------------------------
// Data loading (SSE-driven; poll fallback only when SSE is unavailable)
// ---------------------------------------------------------------------------

async function loadInfo() {
  try {
    state.info = await api("/api/info");
    $("me").textContent = `${state.info.name} · port ${state.info.port}`;
    $("infoId").textContent = state.info.id;
    $("infoName").textContent = state.info.name;
    $("infoVersion").textContent = state.info.build
      ? `${state.info.version} · ${fmtBuild(state.info.build)}`
      : state.info.version;
    $("infoDownloads").textContent = state.info.downloads;
    // Controls that only make sense on the machine hosting the app.
    const isMobile = state.info.platform === "android" || state.info.platform === "ios";
    $("openFolderBtn").hidden = isMobile;
    const loginToggle = $("setStartOnLogin");
    if (loginToggle && loginToggle.closest("label")) {
      loginToggle.closest("label").hidden = isMobile;
    }
    showLocalAddrs();
    renderStats();
    loadSettings();
  } catch (e) {
    toast("Failed to load device info: " + e.message, "error");
  }
}

function showLocalAddrs() {
  const ips = (state.info && state.info.ips) || [];
  $("infoAddrs").textContent =
    ips.length > 0
      ? ips.map((ip) => `http://${ip}:${state.info.port}`).join(", ")
      : "No LAN addresses found — check your network connection";
}

async function loadPeers() {
  try {
    state.peers = await api("/api/peers");
    renderPeers();
  } catch (e) {
    toast("Failed to load peers: " + e.message, "error");
  }
}

async function loadTransfers() {
  try {
    state.transfers = await api("/api/transfers");
    renderAll();
    updateAcceptAll();
  } catch (e) {
    toast("Failed to load transfers: " + e.message, "error");
  }
}

/// The waiting incoming offers, in list order.
function waitingIncoming() {
  return state.transfers.filter((t) => t.direction === "incoming" && t.status === "waiting");
}

/// One click to accept a whole batch (multi-file sends arrive as one offer per
/// file). Uses the configured download folder, so the per-file "ask where to
/// save" prompt is deliberately skipped here.
async function acceptAllIncoming() {
  const waiting = waitingIncoming();
  const btn = $("acceptAllBtn");
  if (btn) btn.disabled = true;
  let ok = 0;
  for (const t of waiting) {
    try {
      await api(`/api/transfers/${t.id}/accept`, { method: "POST" });
      ok++;
    } catch {
      // Keep going: one bad offer must not stop the rest.
    }
  }
  if (btn) btn.disabled = false;
  toast(ok === waiting.length ? `Accepted ${ok} files` : `Accepted ${ok} of ${waiting.length}`, ok ? "success" : "error");
  await loadTransfers();
}

/// Show the "Accept all" button only for real batches (two or more files).
function updateAcceptAll() {
  let btn = $("acceptAllBtn");
  if (!btn) {
    btn = document.createElement("button");
    btn.id = "acceptAllBtn";
    btn.type = "button";
    btn.className = "btn accept-all";
    btn.hidden = true;
    btn.onclick = acceptAllIncoming;
    const list = $("incomingList");
    list.parentNode.insertBefore(btn, list);
  }
  const waiting = waitingIncoming();
  btn.hidden = waiting.length < 2;
  btn.textContent = `Accept all ${waiting.length} files`;
}

// Debounced variants used by SSE handlers so bursts of events are batched.
const refreshPeers = debounce(loadPeers, 150);
const refreshTransfers = debounce(loadTransfers, 150);

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

let lastPeersKey = "";

function renderPeers() {
  const list = $("peerList");
  const peers = state.peers;

  // Forget devices that went away.
  state.selectedPeerIds = state.selectedPeerIds.filter((id) => peers.some((p) => p.id === id));

  // Skip the DOM rebuild when nothing actually changed.
  const key = JSON.stringify(
    peers.map((p) => [p.id, p.name, p.addr, p.port, p.source, p.reachable, p.trusted])
  );
  if (key !== lastPeersKey) {
    lastPeersKey = key;
    list.innerHTML = "";
    if (peers.length === 0) {
      $("peerEmpty").style.display = "";
    } else {
      $("peerEmpty").style.display = "none";
      for (const p of peers) {
        const el = document.createElement("div");
        el.className = "peer" + (state.selectedPeerIds.includes(p.id) ? " selected" : "");
        el.tabIndex = 0;
        el.setAttribute("role", "button");
        const initial = (p.name || "?").trim().charAt(0) || "?";
        const src = p.source === "mdns" || p.source === "lan" ? "nearby" : p.source;
        // A device that announces itself but never answers TCP is usually
        // blocked by a firewall on its side — say so instead of failing later.
        const blocked =
          p.reachable === false
            ? `<span class="src-tag blocked" title="This device announces itself but does not answer on ${esc(p.addr)}:${esc(p.port)}. Check its firewall (allow TCP ${esc(p.port)}) — a rule bound to an old install path stops matching after reinstalling.">unreachable</span>`
            : `<span class="src-tag">${esc(src)}</span>`;
        const trustPill = p.trusted
          ? `<button class="trust-pill trusted" type="button" title="Stop trusting this device">trusted</button>`
          : `<button class="trust-pill" type="button" title="Auto-accept transfers from this device">trust</button>`;
        el.innerHTML = `
          <div class="peer-avatar ${esc(p.source)}">${esc(initial)}</div>
          <div class="p-info">
            <div class="p-name">${esc(p.name)}</div>
            <div class="p-addr">${esc(p.addr)}:${esc(p.port)}</div>
          </div>
          ${trustPill}
          ${blocked}
        `;
        el.onclick = () => togglePeer(p);
        el.onkeydown = (e) => {
          // The trust pill is its own control; it must not toggle selection.
          if (e.target !== el) return;
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            togglePeer(p);
          }
        };
        const pill = el.querySelector(".trust-pill");
        if (pill) {
          pill.onclick = (e) => {
            e.stopPropagation();
            toggleTrust(p);
          };
          pill.onkeydown = (e) => e.stopPropagation();
        }
        list.appendChild(el);
      }
    }
  }
  updateSendTargetLabel();
}

/// Clicking a device toggles it, so a batch can go to several devices at once.
function togglePeer(p) {
  const index = state.selectedPeerIds.indexOf(p.id);
  if (index >= 0) {
    state.selectedPeerIds.splice(index, 1);
  } else {
    state.selectedPeerIds.push(p.id);
  }
  lastPeersKey = ""; // force a rebuild so the highlight updates
  renderPeers();
  updateSendTargetLabel();
  publishSelection();
}

/// Mark a device as trusted (or not) so it can skip the accept prompt when
/// "trusted devices only" auto-accept is enabled. Never changes selection.
async function toggleTrust(p) {
  const trusted = !p.trusted;
  try {
    await api("/api/peers/trust", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ peer_id: p.id, trusted }),
    });
    p.trusted = trusted;
    lastPeersKey = ""; // rebuild so the pill flips immediately
    renderPeers();
    toast(
      trusted ? `${p.name} is trusted` : `${p.name} is no longer trusted`,
      "success"
    );
  } catch (e) {
    toast("Trust update failed: " + e.message, "error");
  }
}

/// Tell the node which devices are selected. The desktop window cannot read this
/// state from Rust, so dropped files are sent to whatever is published here.
function publishSelection() {
  const targets = state.peers
    .filter((p) => state.selectedPeerIds.includes(p.id))
    .map((p) => `${p.addr}:${p.port}`);
  api("/api/ui/selection", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ targets }),
  }).catch(() => {});
}

function updateSendTargetLabel() {
  const peers = state.peers.filter((x) => state.selectedPeerIds.includes(x.id));
  const manual = $("manualAddr").value.trim();
  let label = manual;
  if (peers.length === 1) label = `${peers[0].name} (${peers[0].addr}:${peers[0].port})`;
  else if (peers.length > 1) label = `${peers.length} devices`;
  $("sendTargetLabel").textContent = label ? "→ " + label : "No device selected";
  const hasTarget = peers.length > 0 || Boolean(manual);
  $("sendBtn").disabled = !(state.selectedFiles.length > 0 && hasTarget);
  // The snippet button follows the same rule, plus "there is text to send".
  const textToSend = $("textToSend");
  if (textToSend) {
    $("sendTextBtn").disabled = !(textToSend.value.trim() && hasTarget);
  }
}

/// Every selected device (address form), or the manual address as a fallback.
function sendTargets() {
  const peers = state.peers.filter((x) => state.selectedPeerIds.includes(x.id));
  if (peers.length > 0) return peers.map((p) => ({ label: p.name, address: `${p.addr}:${p.port}` }));
  const manual = $("manualAddr").value.trim();
  return manual ? [{ label: manual, address: manual }] : [];
}

let lastWaitingIds = new Set();

function noteNewOffers() {
  const waiting = state.transfers.filter(
    (t) => t.direction === "incoming" && t.status === "waiting"
  );
  for (const t of waiting) {
    if (!lastWaitingIds.has(t.id)) {
      toast(`Incoming file from ${t.peer_name}: ${t.filename}`, "info", 6000);
    }
  }
  lastWaitingIds = new Set(waiting.map((t) => t.id));
}

function renderAll() {
  renderPeers();
  renderPending();
  renderIncoming();
  renderOutgoing();
  updateBadge();
  noteNewOffers();
  renderActivity();
  renderStats();
}

// Panel hints and the free-space tile (no extra widgets in this layout).
function renderStats() {
  const info = state.info || {};
  const peers = state.peers.length;
  $("devicesHint").textContent = peers === 1 ? "1 found" : `${peers} found`;

  const free = info.disk_free;
  const total = info.disk_total;
  if (typeof free === "number" && typeof total === "number" && total > 0) {
    $("infoDisk").textContent = `${fmtSize(free)} free of ${fmtSize(total)}`;
  } else {
    $("infoDisk").textContent = "unknown";
  }

  const f = state.selectedFile;
  $("fileHint").textContent = f ? "ready to send" : "none yet";

  const waiting = state.transfers.filter(
    (t) => t.direction === "incoming" && t.status === "waiting"
  ).length;
  $("pendingHint").textContent = waiting ? `${waiting} waiting` : "";
  const incomingTotal = state.transfers.filter((t) => t.direction === "incoming").length;
  $("incomingHint").textContent = incomingTotal ? `${incomingTotal} total` : "";
  const outgoingActive = state.transfers.filter(
    (t) =>
      t.direction === "outgoing" &&
      ["preparing", "waiting", "accepted", "transferring"].includes(t.status)
  ).length;
  $("outgoingHint").textContent = outgoingActive ? `${outgoingActive} active` : "idle";
}

// Always-visible receive progress, so the Receive tab is optional.
function renderActivity() {
  const el = $("activity");
  const active = state.transfers.filter(
    (t) =>
      t.direction === "incoming" &&
      ["waiting", "accepted", "transferring"].includes(t.status)
  );
  if (active.length === 0) {
    el.hidden = true;
    return;
  }
  el.hidden = false;
  if (active.length > 1) {
    el.textContent = `▼ ${active.length} incoming`;
    return;
  }
  const t = active[0];
  if (t.status === "transferring") {
    const pct = Math.round(t.progress * 100);
    const rate = t.speed_bps > 0 ? ` · ${fmtSize(Math.round(t.speed_bps))}/s` : "";
    const eta = t.eta_secs != null ? ` · ~${fmtEta(t.eta_secs)} left` : "";
    el.textContent = `▼ receiving ${t.filename} — ${pct}%${rate}${eta}`;
  } else if (t.status === "accepted") {
    el.textContent = `▼ waiting for upload: ${t.filename}`;
  } else {
    el.textContent = `▼ incoming request: ${t.filename}`;
  }
}

const STATUS_TEXT = {
  preparing: "Preparing…",
  waiting: "Waiting for acceptance…",
  accepted: "Accepted",
  transferring: "Transferring…",
  completed: "Completed",
  rejected: "Rejected",
  failed: "Failed",
  cancelled: "Cancelled",
};

/// Group transfers into singles and batches, keeping list order. A batch with a
/// single member is just a file: one row, no "Accept all 1".
function groupBatches(items) {
  const groups = [];
  const byBatch = new Map();
  for (const t of items) {
    if (!t.batch_id) {
      groups.push({ kind: "single", t });
      continue;
    }
    let group = byBatch.get(t.batch_id);
    if (!group) {
      group = { kind: "batch", id: t.batch_id, name: t.batch_name || "files", items: [] };
      byBatch.set(t.batch_id, group);
      groups.push(group);
    }
    group.items.push(t);
  }
  return groups.map((g) =>
    g.kind === "batch" && g.items.length === 1 ? { kind: "single", t: g.items[0] } : g
  );
}

/// `1 file` / `3 files`.
function fileCount(n) {
  return `${n} file${n === 1 ? "" : "s"}`;
}

/// One row for a whole folder/selection: the receiver answers once for the batch,
/// so the UI shows one card with the count, the total size and the joint progress.
function batchCard(g) {
  const items = g.items;
  const size = items.reduce((sum, t) => sum + (t.size || 0), 0);
  const done = items.reduce((sum, t) => sum + (t.bytes_done || 0), 0);
  const pct = size > 0 ? Math.round((done / size) * 100) : 0;
  const incoming = items[0].direction === "incoming";
  const waiting = items.filter((t) => t.status === "waiting").length;
  const active = items.filter((t) =>
    ["preparing", "waiting", "accepted", "transferring"].includes(t.status)
  );
  const streaming = items.filter((t) => t.status === "transferring");
  const rate = streaming.reduce((sum, t) => sum + (t.speed_bps || 0), 0);
  const etas = streaming.map((t) => t.eta_secs).filter((v) => v != null);
  const worst = etas.length ? Math.max(...etas) : null;
  const failed = items.filter((t) => ["failed", "cancelled", "rejected"].includes(t.status)).length;
  const names = items.slice(0, 3).map((t) => t.filename).join(", ");
  const statusText =
    waiting === items.length
      ? "waiting"
      : active.length
        ? `${active.length} active`
        : failed === items.length
          ? "failed"
          : "done";
  const canAccept = incoming && waiting > 0;
  const canCancel = incoming
    ? items.some((t) => ["accepted", "transferring"].includes(t.status))
    : active.length > 0;
  return `
    <div class="transfer batch">
      <div class="t-head">
        <div>
          <div class="t-file">${esc(g.name)}</div>
          <div class="t-meta">
            <span>${fileCount(items.length)} ${incoming ? "from" : "to"} ${esc(items[0].peer_name)}</span>
            <span>${fmtSize(size)}</span>
            ${items[0].pin ? `<span class="pin-pill" title="Verification PIN">PIN ${esc(items[0].pin)}</span>` : ""}
          </div>
          <div class="t-meta"><span class="batch-names">${esc(names)}${items.length > 3 ? " …" : ""}</span></div>
        </div>
        <span class="t-status">${esc(statusText)}</span>
      </div>
      ${
        active.length
          ? `<div class="progress"><div style="width:${pct}%"></div></div><div class="pct">${pct}%${rate > 0 ? ` · ${fmtSize(Math.round(rate))}/s` : ""}${worst != null ? ` · ~${fmtEta(worst)} left` : ""}</div>`
          : ""
      }
      ${
        canAccept
          ? `<div class="t-actions">
          <button class="btn dark sm" data-accept-batch="${esc(g.id)}">Accept all ${fileCount(items.length)}</button>
          <button class="btn danger sm" data-reject-batch="${esc(g.id)}">Reject</button>
        </div>`
          : ""
      }
      ${
        canCancel
          ? `<div class="t-actions"><button class="btn ghost sm" data-cancel-batch="${esc(g.id)}">Cancel batch</button></div>`
          : ""
      }
    </div>
  `;
}

/// Render transfers: batches collapse into one card, singles stay as they were.
function renderCards(items) {
  return groupBatches(items)
    .map((g) => (g.kind === "batch" ? batchCard(g) : transferCard(g.t)))
    .join("");
}

/// Members of a batch that are still in the given statuses.
function batchMembers(id, statuses) {
  return state.transfers.filter((t) => t.batch_id === id && statuses.includes(t.status));
}

async function acceptBatch(id) {
  const waiting = batchMembers(id, ["waiting"]);
  try {
    // One request for the whole folder, not one per file.
    const res = await api(`/api/batches/${encodeURIComponent(id)}/accept`, { method: "POST" });
    const ok = (res && res.accepted) || 0;
    toast(`Accepted ${fileCount(ok)}`, ok ? "success" : "error");
  } catch (e) {
    toast("Accept failed: " + e.message, "error");
  }
  await loadTransfers();
}

async function rejectBatch(id) {
  const waiting = batchMembers(id, ["waiting"]);
  try {
    await api(`/api/batches/${encodeURIComponent(id)}/reject`, { method: "POST" });
    toast(`Rejected ${fileCount(waiting.length)}`);
  } catch (e) {
    toast("Reject failed: " + e.message, "error");
  }
  await loadTransfers();
}

async function cancelBatch(id) {
  const active = batchMembers(id, ["preparing", "waiting", "accepted", "transferring"]).length;
  try {
    // One request for the batch, not one per file.
    await api(`/api/batches/${encodeURIComponent(id)}/cancel`, { method: "POST" });
    toast(active > 1 ? `Cancelled ${fileCount(active)}` : "Cancelled");
  } catch (e) {
    toast("Cancel failed: " + e.message, "error");
  }
  await loadTransfers();
}

function transferCard(t) {
  const pct = Math.round(t.progress * 100);
  const streaming = t.status === "transferring";
  const rate = streaming && t.speed_bps > 0 ? ` · ${fmtSize(Math.round(t.speed_bps))}/s` : "";
  const eta = streaming && t.eta_secs != null ? ` · ~${fmtEta(t.eta_secs)} left` : "";
  const showProgress = t.status === "transferring" || t.status === "preparing" || t.status === "waiting" || t.status === "accepted";
  // Outgoing anything active can be cancelled. Incoming waiting offers are
  // handled with Accept/Reject; an accepted/streaming receive can be aborted.
  const canCancel =
    (t.direction === "outgoing" && ["preparing", "waiting", "accepted", "transferring"].includes(t.status)) ||
    (t.direction === "incoming" && ["accepted", "transferring"].includes(t.status));
  const canAccept = t.direction === "incoming" && t.status === "waiting";
  const canReject = canAccept;
  const saved = t.saved_path ? `<div class="saved-path">Saved: ${esc(t.saved_path)}</div>` : "";
  const err = t.error ? `<div class="t-err">${esc(t.error)}</div>` : "";
  const dir = t.direction === "incoming" ? "from" : "to";
  const isLink = t.text ? /^https?:\/\/\S+$/i.test(t.text.trim()) : false;
  const snippet = t.text
    ? `<div class="snippet"><pre class="text-preview">${esc(t.text)}</pre><div class="t-actions">
         <button class="btn ghost sm" data-copy="${esc(t.id)}" type="button">Copy</button>
         ${isLink ? `<a class="btn ghost sm" href="${esc(t.text.trim())}" target="_blank" rel="noopener noreferrer">Open link</a>` : ""}
       </div></div>`
    : "";
  return `
    <div class="transfer status-${esc(t.status)}">
      <div class="t-head">
        <div>
          <div class="t-file">${esc(t.filename)}</div>
          <div class="t-meta">
            <span>${dir} ${esc(t.peer_name)}</span>
            <span>${fmtSize(t.size)}</span>
            <span>${fmtTime(t.created_at)}</span>
            ${t.pin ? `<span class="pin-pill" title="Verification PIN">PIN ${esc(t.pin)}</span>` : ""}
          </div>
          ${err}
        </div>
        <span class="t-status">${STATUS_TEXT[t.status] || esc(t.status)}</span>
      </div>
      ${showProgress ? `<div class="progress"><div style="width:${pct}%"></div></div><div class="pct">${pct}%${rate}${eta}</div>` : ""}
      ${snippet}
      ${saved}
      ${canCancel ? `<div class="t-actions"><button class="btn ghost sm" data-cancel="${esc(t.id)}">Cancel</button></div>` : ""}
      ${canAccept ? `<div class="t-actions">
          <button class="btn dark sm" data-accept="${esc(t.id)}">Accept</button>
          <button class="btn danger sm" data-reject="${esc(t.id)}">Reject</button>
        </div>` : ""}
    </div>
  `;
}

function renderPending() {
  const list = $("pendingList");
  const pending = state.transfers.filter((t) => t.direction === "incoming" && t.status === "waiting");
  list.innerHTML = renderCards(pending);
  $("pendingEmpty").style.display = pending.length ? "none" : "";
}

function renderIncoming() {
  const list = $("incomingList");
  const items = state.transfers.filter((t) => t.direction === "incoming" && t.status !== "waiting");
  list.innerHTML = renderCards(items);
  $("incomingEmpty").style.display = items.length ? "none" : "";
}

function renderOutgoing() {
  const list = $("outgoingList");
  const items = state.transfers.filter((t) => t.direction === "outgoing");
  list.innerHTML = renderCards(items);
}

function updateBadge() {
  const n = state.transfers.filter((t) => t.direction === "incoming" && t.status === "waiting").length;
  for (const id of ["pendingBadge", "navBadge"]) {
    const b = $(id);
    if (!b) continue;
    b.hidden = n === 0;
    b.textContent = n;
  }
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

async function sendFiles() {
  const files = state.selectedFiles.slice();
  if (files.length === 0) return;
  const targets = sendTargets();
  if (targets.length === 0) { toast("Choose a device first", "warn"); return; }

  const btn = $("sendBtn");
  btn.disabled = true;
  // One batch for a multi-file send, so the receiver is asked once (a single file
  // stays a plain individual transfer).
  const batchId = files.length > 1 ? `ui-${Date.now()}-${Math.random().toString(36).slice(2, 8)}` : null;
  const failures = [];              // human-readable, for the summary toast
  const failedFiles = new Set();    // files the server could not queue at all
  try {
    for (let i = 0; i < files.length; i++) {
      const file = files[i];
      btn.textContent =
        files.length > 1 ? `Uploading ${i + 1}/${files.length}…` : "Uploading…";
      // One upload for every device: the node stages and hashes the file once,
      // then hard-links a copy per target.
      const fd = new FormData();
      for (const target of targets) fd.append("target", target.address);
      fd.append("file", file, file.name);
      // Folder sends: the receiver rebuilds the tree from this path.
      const rel = state.relPaths.get(file);
      if (rel) fd.append("rel_path", rel);
      if (batchId) {
        fd.append("batch_id", batchId);
        fd.append("batch_name", fileCount(files.length));
      }
      try {
        await api("/api/send", { method: "POST", body: fd });
      } catch (e) {
        failedFiles.add(file);
        failures.push(`${file.name}: ${e.message}`);
      }
    }

    // Keep only files the node refused, so a retry does not resend the rest.
    const keep = files.filter((f) => failedFiles.has(f));
    state.selectedFiles = keep;
    for (const f of files) if (!keep.includes(f)) state.relPaths.delete(f);
    state.selectedFile = keep[0] || null;
    $("fileInput").value = "";
    renderFileChosen();
    await loadTransfers();

    const sent = files.length - failedFiles.size;
    const howMany = targets.length > 1 ? `${targets.length} devices` : targets[0].label;
    if (failures.length === 0) {
      toast(total === 1 ? "Transfer started" : `${sent} transfers started (${howMany})`, "success");
    } else if (sent === 0) {
      toast(`Send failed: ${failures[0]}`, "error");
    } else {
      toast(`${sent} sent, ${failures.length} failed — ${failures[0]}`, "warn", 9000);
    }
  } finally {
    btn.disabled = false;
    btn.textContent = "Send";
    updateSendTargetLabel();
  }
}

let activeShare = null;

/// URL a phone should open for this node.
function lanUrl() {
  const info = state.info || {};
  const ip = (info.ips && info.ips[0]) || location.hostname;
  return `http://${ip}:${info.port || location.port}`;
}

/// Inline SVG QR (same-origin, so the strict CSP needs no loosening).
function showQr(share) {
  activeShare = share || null;
  const params = new URLSearchParams();
  if (activeShare) params.set("share", activeShare.token);
  $("qrImage").src = "/api/qr" + (params.toString() ? `?${params.toString()}` : "");
  $("qrUrl").textContent = activeShare ? activeShare.url : lanUrl();
  $("qrTitle").textContent = activeShare ? "Share this file" : "Open LanDrop on your phone";
  $("qrNote").textContent = activeShare ? activeShare.filename : "";
  $("qrHint").textContent = activeShare
    ? "Link expires in 15 minutes"
    : "Point your phone camera at the code";
  $("qrStopBtn").hidden = !activeShare;
  $("qrDialog").showModal();
}

async function shareSelectedFile() {
  const file = state.selectedFiles[0];
  if (!file) { toast("Pick a file first", "warn"); return; }
  const fd = new FormData();
  fd.append("file", file, file.name);
  try {
    const info = await api("/api/share-link", { method: "POST", body: fd });
    showQr(info);
    toast("Share link ready — scan or copy it", "success");
  } catch (e) {
    toast("Share failed: " + e.message, "error");
  }
}

async function stopSharing() {
  if (!activeShare) return;
  const token = activeShare.token;
  activeShare = null;
  $("qrStopBtn").hidden = true;
  $("qrDialog").close();
  try {
    await api(`/api/share-link/${encodeURIComponent(token)}`, { method: "DELETE" });
    toast("Link revoked");
  } catch (e) {
    toast("Could not revoke: " + e.message, "error");
  }
}

async function sendText() {
  const el = $("textToSend");
  const text = el.value.trim();
  if (!text) { toast("Type or paste something first", "warn"); return; }
  const targets = sendTargets().map((t) => t.address);
  const btn = $("sendTextBtn");
  btn.disabled = true;
  try {
    const res = await api("/api/send-text", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(targets.length ? { text, targets } : { text }),
    });
    el.value = "";
    const sent = (res && res.transfer_ids && res.transfer_ids.length) || 1;
    toast(sent > 1 ? `Text sent to ${sent} devices` : "Text sent", "success");
    await loadTransfers();
  } catch (e) {
    toast("Send failed: " + e.message, "error");
  } finally {
    btn.disabled = false;
    updateSendTargetLabel();
  }
}

async function copySnippet(id) {
  const t = state.transfers.find((x) => x.id === id);
  if (!t || !t.text) return;
  try {
    await navigator.clipboard.writeText(t.text);
    toast("Copied to the clipboard", "success");
  } catch {
    toast("Clipboard is not available here", "error");
  }
}

async function cancelTransfer(id) {
  try {
    await api(`/api/transfers/${id}/cancel`, { method: "POST" });
    toast("Transfer cancelled");
    await loadTransfers();
  } catch (e) {
    toast("Cancel failed: " + e.message, "error");
  }
}

async function acceptTransfer(id) {
  try {
    // A snippet is not saved anywhere, so never ask for a folder for it.
    const transfer = state.transfers.find((x) => x.id === id);
    const isSnippet = Boolean(transfer && transfer.text);
    let saveDir = null;
    if (!isSnippet && state.settings && state.settings.ask_save_path) {
      const choice = await openPicker("accept", state.settings.downloads);
      if (!choice) return; // cancelled: leave the request pending
      if (choice.action === "use") saveDir = choice.path;
    }
    const opts = { method: "POST" };
    if (saveDir) {
      opts.headers = { "Content-Type": "application/json" };
      opts.body = JSON.stringify({ save_dir: saveDir });
    }
    await api(`/api/transfers/${id}/accept`, opts);
    toast(
      isSnippet
        ? "Snippet accepted"
        : saveDir
          ? `Accepted — saving to ${saveDir}`
          : "Accepted — waiting for the file…",
      "success"
    );
    await loadTransfers();
  } catch (e) {
    toast("Accept failed: " + e.message, "error");
  }
}

async function rejectTransfer(id) {
  try {
    await api(`/api/transfers/${id}/reject`, { method: "POST" });
    toast("Transfer rejected");
    await loadTransfers();
  } catch (e) {
    toast("Reject failed: " + e.message, "error");
  }
}

async function addManualPeer() {
  const addr = $("manualAddr").value.trim();
  if (!addr) return;
  try {
    await api("/api/peers/manual", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ address: addr }),
    });
    toast(`Added manual peer ${addr}`, "success");
    $("manualAddr").value = "";
    await loadPeers();
  } catch (e) {
    toast("Add peer failed: " + e.message, "error");
  }
}

async function loadSettings() {
  try {
    const s = await api("/api/settings");
    state.settings = s;
    $("setName").value = s.name;
    $("setPort").value = s.port;
    $("setDownloads").value = s.downloads;
    $("setAutoAccept").checked = !!s.auto_accept;
    $("setAutoAcceptTrusted").checked = !!s.auto_accept_trusted_only;
    $("setAskSavePath").checked = !!s.ask_save_path;
    $("setStartOnLogin").checked = !!s.start_on_login;
  } catch (_) {
    /* settings are optional on first paint */
  }
}

async function saveSettings() {
  const portRaw = $("setPort").value.trim();
  let port = undefined;
  if (portRaw !== "") {
    port = Number(portRaw);
    if (!Number.isInteger(port) || port < 1 || port > 65535) {
      toast("Port must be a number between 1 and 65535", "warn");
      return;
    }
  }
  const btn = $("saveSettingsBtn");
  btn.disabled = true;
  try {
    await api("/api/settings", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        name: $("setName").value,
        port: port,
        downloads: $("setDownloads").value,
        auto_accept: $("setAutoAccept").checked,
        auto_accept_trusted_only: $("setAutoAcceptTrusted").checked,
        ask_save_path: $("setAskSavePath").checked,
        start_on_login: $("setStartOnLogin").checked,
      }),
    });
    toast("Settings saved", "success");
    await loadInfo();
  } catch (e) {
    toast("Save failed: " + e.message, "error");
  } finally {
    btn.disabled = false;
  }
}

async function openFolder() {
  try {
    const data = await api("/api/downloads/open", { method: "POST" });
    toast("Opening " + data.path, "success");
  } catch (e) {
    toast("Could not open folder: " + e.message, "warn");
  }
}

async function copyLanUrl() {
  const ips = (state.info && state.info.ips) || [];
  const port = (state.info && state.info.port) || 8420;
  if (ips.length === 0) {
    toast("No LAN address available to copy", "warn");
    return;
  }
  const url = `http://${ips[0]}:${port}`;
  try {
    await navigator.clipboard.writeText(url);
    toast("Copied: " + url, "success");
  } catch (e) {
    toast("Could not copy: " + e.message, "error");
  }
}

// ---------------------------------------------------------------------------
// File selection / drag & drop
// ---------------------------------------------------------------------------

function pickFiles(entries) {
  // One transfer per file: the same path a single file has always taken, so any
  // mix of types and sizes works and every file gets its own progress row.
  // Folder drops arrive as {file, relPath} so the receiver can rebuild the tree.
  const incoming = Array.from(entries || []).filter((e) => e && e.file && e.file.size >= 0);
  if (incoming.length === 0) return;
  const room = MAX_BATCH_FILES - state.selectedFiles.length;
  if (room <= 0) {
    toast(`Up to ${MAX_BATCH_FILES} files per batch — send these first`, "warn");
    return;
  }
  if (incoming.length > room) {
    toast(`Added the first ${room} — up to ${MAX_BATCH_FILES} files per batch`, "warn");
  }
  for (const entry of incoming.slice(0, room)) {
    state.selectedFiles.push(entry.file);
    if (entry.relPath) state.relPaths.set(entry.file, entry.relPath);
  }
  state.selectedFile = state.selectedFiles[0] || null;
  renderFileChosen();
  updateSendTargetLabel();
}

/// Files picked with the file dialog (no folders, no relative paths).
function pickedFiles(list) {
  return Array.from(list || []).map((file) => ({ file, relPath: "" }));
}

/// Walk dropped entries, keeping each file's path inside the dragged folder.
async function droppedFiles(dataTransfer) {
  const out = [];
  const readEntries = (reader) =>
    new Promise((resolve, reject) => reader.readEntries(resolve, reject));
  const walk = async (entry, prefix) => {
    if (out.length >= MAX_BATCH_FILES) return;
    if (entry.isFile) {
      const file = await new Promise((resolve, reject) => entry.file(resolve, reject));
      out.push({ file, relPath: prefix });
      return;
    }
    if (!entry.isDirectory) return;
    const reader = entry.createReader();
    const nextPrefix = prefix ? `${prefix}/${entry.name}` : entry.name;
    for (;;) {
      const batch = await readEntries(reader);
      if (!batch.length) break;
      for (const child of batch) await walk(child, nextPrefix);
    }
  };

  const items = dataTransfer.items;
  if (items && items.length && items[0].webkitGetAsEntry) {
    for (const item of items) {
      const entry = item.webkitGetAsEntry();
      if (!entry) continue;
      if (entry.isFile) {
        const file = item.getAsFile();
        if (file) out.push({ file, relPath: "" });
      } else {
        await walk(entry, "");
      }
    }
    return out;
  }
  return Array.from(dataTransfer.files || []).map((file) => ({ file, relPath: "" }));
}

function renderFileChosen() {
  const files = state.selectedFiles;
  $("fileChosen").hidden = files.length === 0;
  if (files.length > 0) {
    const total = files.reduce((sum, f) => sum + f.size, 0);
    const names = files.slice(0, 3).map((f) => f.name).join(", ");
    $("chosenName").textContent =
      files.length === 1 ? files[0].name : `${files.length} files: ${names}${files.length > 3 ? " …" : ""}`;
    $("chosenSize").textContent = fmtSize(total);
  }
  renderStats();
}

function setupDropzone() {
  const dz = $("dropzone");
  const input = $("fileInput");
  dz.onclick = () => input.click();
  dz.onkeydown = (e) => {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      input.click();
    }
  };
  input.multiple = true;
  input.onchange = () => {
    pickFiles(pickedFiles(input.files));
    // Reset so picking the same file again still fires `change`.
    input.value = "";
  };
  dz.ondragover = (e) => { e.preventDefault(); dz.classList.add("dragover"); };
  dz.ondragleave = () => dz.classList.remove("dragover");
  dz.ondrop = (e) => {
    e.preventDefault();
    dz.classList.remove("dragover");
    // Folders are walked here, so dragging a directory sends everything inside
    // it with the tree preserved.
    droppedFiles(e.dataTransfer).then((entries) => {
      if (entries.length === 0) {
        toast("Nothing to send in that drop", "warn");
      }
      pickFiles(entries);
    });
  };
  $("clearFile").onclick = () => {
    state.selectedFiles = [];
    state.relPaths.clear();
    state.selectedFile = null;
    // Reset the input so picking the same file again still fires `change`.
    $("fileInput").value = "";
    renderFileChosen();
    updateSendTargetLabel();
  };
}

// ---------------------------------------------------------------------------
// Tabs (persist active tab in localStorage)
// ---------------------------------------------------------------------------

function showTab(name) {
  document.querySelectorAll(".tab[data-tab], .nav-item[data-tab]").forEach((b) => {
    const on = b.dataset.tab === name;
    b.classList.toggle("active", on);
    b.setAttribute("aria-selected", on ? "true" : "false");
  });
  document.querySelectorAll(".panel").forEach((p) => p.classList.toggle("active", p.id === "panel-" + name));
  localStorage.setItem("landrop.tab", name);
}

function setupTabs() {
  const saved = localStorage.getItem("landrop.tab") || "send";
  document.querySelectorAll(".tab[data-tab], .nav-item[data-tab]").forEach((btn) => {
    btn.onclick = () => showTab(btn.dataset.tab);
  });
  // Restore saved tab if it exists, else keep default (send).
  if (document.querySelector(`.tab[data-tab="${saved}"]`)) showTab(saved);
}

// ---------------------------------------------------------------------------
// Folder picker (used by Settings and by "ask where to save")
// ---------------------------------------------------------------------------

let pickerResolve = null;
let pickerPath = "";

// Resolves with { action: "use", path } | { action: "default" } | null.
function openPicker(mode, startPath) {
  return new Promise((resolve) => {
    pickerResolve = resolve;
    const dlg = $("folderPicker");
    $("pickerNote").textContent =
      mode === "accept" ? "Choose where this file will be saved" : "Choose the download folder";
    $("pickerDefault").hidden = mode !== "accept";
    if (typeof dlg.showModal === "function") dlg.showModal();
    else dlg.setAttribute("open", "");
    loadPicker(startPath || (state.settings && state.settings.downloads) || "");
  });
}

function closePicker(value) {
  const dlg = $("folderPicker");
  if (dlg.open) dlg.close();
  else dlg.removeAttribute("open");
  const resolve = pickerResolve;
  pickerResolve = null;
  if (resolve) resolve(value);
}

async function loadPicker(path) {
  try {
    const query = path ? `?path=${encodeURIComponent(path)}` : "";
    const data = await api(`/api/fs/list${query}`);
    pickerPath = data.path;
    $("pickerPath").value = data.path;
    $("pickerCurrent").textContent = data.path;
    $("pickerUp").disabled = !data.parent;
    $("pickerUp").dataset.parent = data.parent || "";

    const places = $("pickerPlaces");
    places.innerHTML = "";
    for (const p of data.places || []) {
      const b = document.createElement("button");
      b.type = "button";
      b.className = "picker-place";
      b.textContent = p.name;
      b.title = p.path;
      b.onclick = () => loadPicker(p.path);
      places.appendChild(b);
    }

    const list = $("pickerList");
    list.innerHTML = "";
    const dirs = data.dirs || [];
    if (dirs.length === 0) {
      const row = document.createElement("div");
      row.className = "picker-row empty-row";
      row.textContent = "No subfolders here";
      list.appendChild(row);
    }
    for (const d of dirs) {
      const row = document.createElement("div");
      row.className = "picker-row";
      row.tabIndex = 0;
      row.setAttribute("role", "button");
      row.innerHTML = `<span class="dir-icon">▸</span><span>${esc(d.name)}</span>`;
      const go = () => loadPicker(d.path);
      row.onclick = go;
      row.onkeydown = (e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          go();
        }
      };
      list.appendChild(row);
    }
    if (data.truncated) {
      const row = document.createElement("div");
      row.className = "picker-row empty-row";
      row.textContent = "Listing truncated (too many folders)";
      list.appendChild(row);
    }
  } catch (e) {
    toast("Cannot open folder: " + e.message, "error");
  }
}

async function pickerCreateFolder() {
  const name = $("pickerNewName").value.trim();
  if (!name) return;
  try {
    const created = await api("/api/fs/mkdir", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ parent: pickerPath, name }),
    });
    $("pickerNewName").value = "";
    await loadPicker(created.path);
  } catch (e) {
    toast("Create folder failed: " + e.message, "error");
  }
}

function setupPicker() {
  $("pickerUse").onclick = () => closePicker({ action: "use", path: pickerPath });
  $("pickerCancel").onclick = () => closePicker(null);
  $("pickerDefault").onclick = () => closePicker({ action: "default" });
  $("pickerGo").onclick = () => loadPicker($("pickerPath").value.trim());
  $("pickerPath").addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      loadPicker($("pickerPath").value.trim());
    }
  });
  $("pickerUp").onclick = () => {
    const parent = $("pickerUp").dataset.parent;
    if (parent) loadPicker(parent);
  };
  $("pickerMkdir").onclick = pickerCreateFolder;
  $("pickerNewName").addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      pickerCreateFolder();
    }
  });
  // Esc (or the backdrop) cancels.
  $("folderPicker").addEventListener("cancel", (e) => {
    e.preventDefault();
    closePicker(null);
  });
}

// ---------------------------------------------------------------------------
// Events (SSE primary; polling fallback while SSE is unavailable)
// ---------------------------------------------------------------------------

let sseRetryDelay = 5000;

function connectEvents() {
  let es;
  try {
    es = new EventSource("/api/events");
  } catch (_) {
    es = null;
  }
  if (!es) {
    startPollingFallback();
    scheduleSseRetry();
    return;
  }
  es.addEventListener("peer_updated", refreshPeers);
  es.addEventListener("peer_removed", refreshPeers);
  es.addEventListener("transfer_updated", refreshTransfers);
  es.addEventListener("transfer_completed", (e) => { refreshTransfers(); notifyComplete(e.data); });
  es.addEventListener("transfer_failed", refreshTransfers);
  // The server tells us when we missed events (lagged subscriber).
  es.addEventListener("resync", () => { loadPeers(); loadTransfers(); });
  es.onopen = () => {
    sseRetryDelay = 5000;
    stopPollingFallback();
    // Pick up anything that changed while we were disconnected.
    loadPeers();
    loadTransfers();
  };
  es.onerror = () => {
    es.close();
    startPollingFallback();
    scheduleSseRetry();
  };
}

let pollTimer = null;
let sseRetryTimer = null;

function startPollingFallback() {
  if (pollTimer) return; // already polling
  pollTimer = setInterval(() => { loadTransfers(); loadPeers(); }, 3000);
}

function stopPollingFallback() {
  if (!pollTimer) return;
  clearInterval(pollTimer);
  pollTimer = null;
}

function scheduleSseRetry() {
  if (sseRetryTimer) return;
  sseRetryTimer = setTimeout(() => {
    sseRetryTimer = null;
    connectEvents();
  }, sseRetryDelay);
  sseRetryDelay = Math.min(sseRetryDelay * 2, 30000);
}

function notifyComplete(data) {
  try {
    const t = JSON.parse(data);
    if (t.direction === "incoming" && t.status === "completed") {
      toast(`Received ${t.filename}`, "success");
    } else if (t.direction === "outgoing" && t.status === "completed") {
      toast(`${t.filename} sent to ${t.peer_name}`, "success");
    }
  } catch (_) { /* ignore */ }
}

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------

function init() {
  setupTabs();
  setupDropzone();
  setupPicker();
  $("themeToggle").onclick = toggleTheme;
  const navTheme = $("themeToggleNav");
  if (navTheme) navTheme.onclick = toggleTheme;
  $("activity").onclick = () => showTab("receive");
  $("browseDownloadsBtn").onclick = async () => {
    const choice = await openPicker("settings", $("setDownloads").value.trim());
    if (choice && choice.action === "use") $("setDownloads").value = choice.path;
  };
  $("sendBtn").onclick = sendFiles;
  $("sendTextBtn").onclick = sendText;
  $("textToSend").oninput = updateSendTargetLabel;
  $("textToSend").onkeydown = (e) => {
    // Ctrl+Enter / Cmd+Enter sends the snippet, like any chat box.
    if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
      e.preventDefault();
      if (!$("sendTextBtn").disabled) sendText();
    }
  };
  $("showQrBtn").onclick = () => showQr(null);
  $("settingsQrBtn").onclick = () => showQr(null);
  $("shareLinkBtn").onclick = shareSelectedFile;
  $("qrCloseBtn").onclick = () => $("qrDialog").close();
  $("qrStopBtn").onclick = stopSharing;
  $("qrCopyBtn").onclick = async () => {
    try {
      await navigator.clipboard.writeText($("qrUrl").textContent);
      toast("Link copied", "success");
    } catch {
      toast("Clipboard is not available here", "error");
    }
  };
  $("addManualBtn").onclick = addManualPeer;
  $("manualAddr").oninput = updateSendTargetLabel;
  $("saveSettingsBtn").onclick = saveSettings;
  $("openFolderBtn").onclick = openFolder;
  $("copyUrlBtn").onclick = copyLanUrl;

  // Delegate transfer action buttons.
  document.body.addEventListener("click", (e) => {
    const cancel = e.target.closest("[data-cancel]");
    const accept = e.target.closest("[data-accept]");
    const reject = e.target.closest("[data-reject]");
    const copy = e.target.closest("[data-copy]");
    const acceptBatchBtn = e.target.closest("[data-accept-batch]");
    const rejectBatchBtn = e.target.closest("[data-reject-batch]");
    const cancelBatchBtn = e.target.closest("[data-cancel-batch]");
    if (copy) copySnippet(copy.dataset.copy);
    else if (acceptBatchBtn) acceptBatch(acceptBatchBtn.dataset.acceptBatch);
    else if (rejectBatchBtn) rejectBatch(rejectBatchBtn.dataset.rejectBatch);
    else if (cancelBatchBtn) cancelBatch(cancelBatchBtn.dataset.cancelBatch);
    else if (cancel) cancelTransfer(cancel.dataset.cancel);
    else if (accept) acceptTransfer(accept.dataset.accept);
    else if (reject) rejectTransfer(reject.dataset.reject);
  });

  updateSendTargetLabel();
  loadInfo();
  loadPeers();
  loadTransfers();
  connectEvents();
  publishSelection();
}

document.addEventListener("DOMContentLoaded", init);
