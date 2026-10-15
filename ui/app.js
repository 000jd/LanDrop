// LanDrop UI — vanilla JS, no frameworks, no CDNs.
"use strict";

const $ = (id) => document.getElementById(id);

const state = {
  info: null,
  settings: null,
  peers: [],
  transfers: [],
  selectedPeerId: null,
  selectedFile: null,
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
  } catch (e) {
    toast("Failed to load transfers: " + e.message, "error");
  }
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

  if (state.selectedPeerId && !peers.some((p) => p.id === state.selectedPeerId)) {
    state.selectedPeerId = null;
  }

  // Skip the DOM rebuild when nothing actually changed.
  const key = JSON.stringify(
    peers.map((p) => [p.id, p.name, p.addr, p.port, p.source])
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
        el.className = "peer" + (state.selectedPeerId === p.id ? " selected" : "");
        el.tabIndex = 0;
        el.setAttribute("role", "button");
        const initial = (p.name || "?").trim().charAt(0) || "?";
        const src = p.source === "mdns" || p.source === "lan" ? "nearby" : p.source;
        el.innerHTML = `
          <div class="peer-avatar ${esc(p.source)}">${esc(initial)}</div>
          <div class="p-info">
            <div class="p-name">${esc(p.name)}</div>
            <div class="p-addr">${esc(p.addr)}:${esc(p.port)}</div>
          </div>
          <span class="src-tag">${esc(src)}</span>
        `;
        el.onclick = () => selectPeer(p);
        el.onkeydown = (e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            selectPeer(p);
          }
        };
        list.appendChild(el);
      }
    }
  }
  updateSendTargetLabel();
}

function selectPeer(p) {
  state.selectedPeerId = p.id;
  lastPeersKey = ""; // force a rebuild so the highlight updates
  renderPeers();
}

function updateSendTargetLabel() {
  const p = state.peers.find((x) => x.id === state.selectedPeerId);
  const manual = $("manualAddr").value.trim();
  const label = p ? `${p.name} (${p.addr}:${p.port})` : manual;
  $("sendTargetLabel").textContent = label ? "→ " + label : "No device selected";
  $("sendBtn").disabled = !(state.selectedFile && (p || manual));
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
  return `
    <div class="transfer status-${esc(t.status)}">
      <div class="t-head">
        <div>
          <div class="t-file">${esc(t.filename)}</div>
          <div class="t-meta">
            <span>${dir} ${esc(t.peer_name)}</span>
            <span>${fmtSize(t.size)}</span>
            <span>${fmtTime(t.created_at)}</span>
          </div>
          ${err}
        </div>
        <span class="t-status">${STATUS_TEXT[t.status] || esc(t.status)}</span>
      </div>
      ${showProgress ? `<div class="progress"><div style="width:${pct}%"></div></div><div class="pct">${pct}%${rate}${eta}</div>` : ""}
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
  list.innerHTML = pending.map(transferCard).join("");
  $("pendingEmpty").style.display = pending.length ? "none" : "";
}

function renderIncoming() {
  const list = $("incomingList");
  const items = state.transfers.filter((t) => t.direction === "incoming" && t.status !== "waiting");
  list.innerHTML = items.map(transferCard).join("");
  $("incomingEmpty").style.display = items.length ? "none" : "";
}

function renderOutgoing() {
  const list = $("outgoingList");
  const items = state.transfers.filter((t) => t.direction === "outgoing");
  list.innerHTML = items.map(transferCard).join("");
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

async function sendFile() {
  const file = state.selectedFile;
  if (!file) return;
  const manual = $("manualAddr").value.trim();
  const p = state.peers.find((x) => x.id === state.selectedPeerId);
  // Prefer the selected device. The manual box is only a fallback (and for Add).
  let target = (p ? `${p.addr}:${p.port}` : "") || manual;
  if (!target) { toast("Choose a device first", "warn"); return; }

  const btn = $("sendBtn");
  btn.disabled = true;
  btn.textContent = "Uploading…";
  try {
    const fd = new FormData();
    fd.append("target", target);
    fd.append("file", file, file.name);
    const data = await api("/api/send", { method: "POST", body: fd });
    toast(`Transfer started (${data.transfer_id.slice(0, 8)}…)`, "success");
    // Only clear the picker if the user did not select another file meanwhile.
    if (state.selectedFile === file) {
      state.selectedFile = null;
      $("fileInput").value = "";
      renderFileChosen();
    }
    await loadTransfers();
  } catch (e) {
    toast("Send failed: " + e.message, "error");
  } finally {
    btn.disabled = false;
    btn.textContent = "Send";
    updateSendTargetLabel();
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
    let saveDir = null;
    if (state.settings && state.settings.ask_save_path) {
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
      saveDir ? `Accepted — saving to ${saveDir}` : "Accepted — waiting for the file…",
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

function pickFile(file) {
  state.selectedFile = file;
  renderFileChosen();
  updateSendTargetLabel();
}

function renderFileChosen() {
  const f = state.selectedFile;
  $("fileChosen").hidden = !f;
  if (f) {
    $("chosenName").textContent = f.name;
    $("chosenSize").textContent = fmtSize(f.size);
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
  input.onchange = () => { if (input.files[0]) pickFile(input.files[0]); };
  dz.ondragover = (e) => { e.preventDefault(); dz.classList.add("dragover"); };
  dz.ondragleave = () => dz.classList.remove("dragover");
  dz.ondrop = (e) => {
    e.preventDefault();
    dz.classList.remove("dragover");
    const f = e.dataTransfer.files[0];
    if (f) pickFile(f);
  };
  $("clearFile").onclick = () => {
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
  $("sendBtn").onclick = sendFile;
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
    if (cancel) cancelTransfer(cancel.dataset.cancel);
    else if (accept) acceptTransfer(accept.dataset.accept);
    else if (reject) rejectTransfer(reject.dataset.reject);
  });

  loadInfo();
  loadPeers();
  loadTransfers();
  connectEvents();

  // The desktop app appends ?older_instance=<port> when another (older) LanDrop
  // build is still running: that process keeps serving the old UI on its port,
  // so say it out loud instead of letting it look like a failed update.
  const older = new URLSearchParams(location.search).get("older_instance");
  if (older) {
    toast(
      `An older LanDrop is still running on port ${older} — quit it from its tray menu to free that port`,
      "warn",
      15000,
    );
    history.replaceState(null, "", location.pathname);
  }
}

document.addEventListener("DOMContentLoaded", init);
