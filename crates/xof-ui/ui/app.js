// ButterSat control UI. Talks to the daemon through the Tauri `rpc` command
// and receives pushed `daemon-event`s (stats, aircraft, logs, health).
"use strict";

const T = window.__TAURI__;
const rpc = (method, params) => T.core.invoke("rpc", { method, params: params ?? null });
const $ = (id) => document.getElementById(id);
const fmtMB = (b) => (b / 1048576).toFixed(b < 10 * 1048576 ? 1 : 0) + " MB";
const fmtGB = (b) => (b / 1073741824).toFixed(1) + " GB";

let status = null;
let config = null;
let pendingUpdate = null;

function renderUpdateBanner() {
  const banner = $("update-banner");
  if (!banner) return;
  if (!pendingUpdate || !pendingUpdate.available) {
    banner.classList.add("hidden");
    return;
  }
  banner.classList.remove("hidden");
  const simRunning = !!(status && status.sim_connected);
  const btn = $("btn-update");
  btn.disabled = simRunning;
  btn.title = simRunning ? "Close X-Plane before installing an update" : "Download and install ButterSat " + pendingUpdate.version;
  $("update-text").textContent = simRunning
    ? `ButterSat ${pendingUpdate.version} is available (close X-Plane to install).`
    : `ButterSat ${pendingUpdate.version} is available${pendingUpdate.body ? " — " + pendingUpdate.body : ""}.`;
}

async function checkUpdates(manual) {
  const aboutMsg = $("about-update");
  if (manual && aboutMsg) {
    aboutMsg.className = "result";
    aboutMsg.textContent = "Checking for updates…";
  }
  try {
    const u = await T.core.invoke("update_check");
    pendingUpdate = u;
    renderUpdateBanner();
    if (aboutMsg) {
      aboutMsg.className = "result";
      aboutMsg.textContent = u.available
        ? `Update ${u.version} available — see Overview to install.`
        : `Up to date (${u.current_version}).`;
    }
  } catch (e) {
    if (manual && aboutMsg) {
      aboutMsg.className = "result err";
      aboutMsg.textContent = "Update check failed: " + e;
    }
  }
}

// ---------------------------------------------------------------- navigation
function show(view) {
  document.querySelectorAll(".view").forEach((v) => v.classList.toggle("active", v.id === view));
  document.querySelectorAll(".side a").forEach((a) => a.classList.toggle("active", a.getAttribute("href") === "#" + view));
  if (view === "map") initMap();
  if (view === "settings") {
    loadSettings();
    loadStorage();
  }
  if (view === "packs") loadPacks();
  if (view === "about") loadAbout();
}
window.addEventListener("hashchange", () => show(location.hash.slice(1) || "overview"));

// ---------------------------------------------------------------- activity charts
// Daemon samples cumulative counters every 5 s (2 h kept); we chart the
// last 30 min: download rate (differences) and cache size.
const WINDOW_S = 30 * 60;
let history = [];

const fmtRate = (bps) => (bps >= 1048576 ? (bps / 1048576).toFixed(1) + " MB/s" : (bps / 1024).toFixed(0) + " KB/s");
const fmtSize = (b) => (b >= 1073741824 ? (b / 1073741824).toFixed(2) + " GB" : (b / 1048576).toFixed(0) + " MB");
const ago = (s) => (s < 60 ? `${Math.round(s)} s ago` : `${Math.round(s / 60)} min ago`);
const NS = "http://www.w3.org/2000/svg";
function el(tag, attrs, parent) {
  const e = document.createElementNS(NS, tag);
  for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, v);
  if (parent) parent.appendChild(e);
  return e;
}

// Nice upper bound for the y axis.
function niceMax(v) {
  if (v <= 0) return 1;
  const p = Math.pow(10, Math.floor(Math.log10(v)));
  for (const m of [1, 2, 2.5, 5, 10]) if (m * p >= v) return m * p;
  return 10 * p;
}

function drawChart(fig, points, fmt, valueId) {
  // points: [{t, v}] oldest first, t in unix seconds.
  const svg = fig.querySelector("svg");
  const tip = fig.querySelector(".tip");
  svg.replaceChildren();
  const W = svg.clientWidth || 300, H = 96, L = 4, R = 4, T = 14, B = 14;
  svg.setAttribute("viewBox", `0 0 ${W} ${H}`);
  const now = Date.now() / 1000;
  const x = (t) => L + ((t - (now - WINDOW_S)) / WINDOW_S) * (W - L - R);
  const ymax = niceMax(Math.max(0, ...points.map((p) => p.v)));
  const y = (v) => T + (1 - v / ymax) * (H - T - B);
  el("line", { x1: L, x2: W - R, y1: y(0), y2: y(0), class: "grid-line" }, svg);
  el("line", { x1: L, x2: W - R, y1: y(ymax), y2: y(ymax), class: "grid-line" }, svg);
  el("text", { x: L, y: T - 4 }, svg).textContent = fmt(ymax);
  el("text", { x: L, y: H - 2 }, svg).textContent = "30 min ago";
  el("text", { x: W - R, y: H - 2, "text-anchor": "end" }, svg).textContent = "now";
  $(valueId).textContent = points.length ? fmt(points[points.length - 1].v) : "–";
  if (points.length < 2) return;
  const d = points.map((p, i) => `${i ? "L" : "M"}${x(p.t).toFixed(1)},${y(p.v).toFixed(1)}`).join("");
  el("path", { d: `${d}L${x(points[points.length - 1].t).toFixed(1)},${y(0)}L${x(points[0].t).toFixed(1)},${y(0)}Z`, class: "area" }, svg);
  el("path", { d, class: "series" }, svg);
  const cross = el("line", { y1: T, y2: H - B, class: "cross", visibility: "hidden" }, svg);
  const dot = el("circle", { r: 4, class: "dot", visibility: "hidden" }, svg);
  // Hover: nearest sample to the pointer.
  const hit = el("rect", { x: 0, y: 0, width: W, height: H, fill: "transparent" }, svg);
  hit.addEventListener("pointermove", (ev) => {
    const r = svg.getBoundingClientRect();
    const px = ((ev.clientX - r.left) / r.width) * W;
    let best = points[0];
    for (const p of points) if (Math.abs(x(p.t) - px) < Math.abs(x(best.t) - px)) best = p;
    const bx = x(best.t);
    cross.setAttribute("x1", bx);
    cross.setAttribute("x2", bx);
    dot.setAttribute("cx", bx);
    dot.setAttribute("cy", y(best.v));
    cross.setAttribute("visibility", "visible");
    dot.setAttribute("visibility", "visible");
    tip.textContent = `${fmt(best.v)} · ${ago(now - best.t)}`;
    tip.style.left = `${(bx / W) * 100}%`;
    tip.classList.remove("hidden");
  });
  hit.addEventListener("pointerleave", () => {
    cross.setAttribute("visibility", "hidden");
    dot.setAttribute("visibility", "hidden");
    tip.classList.add("hidden");
  });
}

function renderActivity() {
  const cut = Date.now() / 1000 - WINDOW_S;
  const h = history.filter((s) => s.t >= cut - 10);
  const rate = [];
  for (let i = 1; i < h.length; i++) {
    const dt = h[i].t - h[i - 1].t;
    // Counter resets (daemon restart) show as zero, never negative.
    if (dt > 0) rate.push({ t: h[i].t, v: Math.max(0, h[i].download_bytes - h[i - 1].download_bytes) / dt });
  }
  drawChart($("ch-rate"), rate.filter((p) => p.t >= cut), fmtRate, "v-rate");
  const cache = h.filter((s) => s.t >= cut).map((s) => ({ t: s.t, v: s.cache_bytes }));
  drawChart($("ch-cache"), cache, fmtSize, "v-cache");
  const last = h[h.length - 1];
  if (last && last.cache_cap_bytes) $("v-cache").textContent += ` of ${fmtGB(last.cache_cap_bytes)}`;
}

async function refreshHistory() {
  try {
    const since = history.length ? history[history.length - 1].t : 0;
    const add = await rpc("history", { since });
    history = history.concat(add).slice(-1440);
    renderActivity();
  } catch (_) {}
}
refreshHistory();
setInterval(refreshHistory, 5000);
window.addEventListener("resize", () => renderActivity());

// ---------------------------------------------------------------- simple / advanced
// Simple mode shows the overview only in a small window; "Advanced" reveals
// every page and widens the window. The choice is remembered per viewer.
const SIZES = { simple: [640, 720], advanced: [1240, 820] };
function store(k, v) {
  try {
    if (v === undefined) return localStorage.getItem(k);
    localStorage.setItem(k, v);
  } catch (_) {
    return null;
  }
}
async function setAdvanced(on, resize) {
  document.body.classList.toggle("simple", !on);
  $("btn-advanced").textContent = on ? "Simple" : "Advanced";
  $("btn-advanced").title = on ? "Back to the overview only" : $("btn-advanced").title;
  store("buttersat.advanced", on ? "1" : "0");
  if (!on && location.hash && location.hash !== "#overview") location.hash = "#overview";
  if (resize) {
    try {
      const w = T.window.getCurrentWindow();
      const [want_w, want_h] = SIZES[on ? "advanced" : "simple"];
      const f = await w.scaleFactor();
      const cur = (await w.innerSize()).toLogical(f);
      // Grow for advanced, shrink back for simple; never fight a user-made larger window.
      if (on ? cur.width < want_w : cur.width > want_w) await w.setSize(new (T.window.LogicalSize || T.dpi.LogicalSize)(want_w, want_h));
    } catch (_) {}
  }
}
$("btn-advanced").addEventListener("click", () => setAdvanced(document.body.classList.contains("simple"), true));
setAdvanced(store("buttersat.advanced") === "1", false);

// ---------------------------------------------------------------- overview
function pill(id, cls, text) {
  const p = $(id);
  p.className = "pill " + cls;
  if (text) p.textContent = text;
}

function renderStatus(s) {
  status = s;
  renderStorage(s);
  renderUpdateBanner();
  pill("pill-sim", s.sim_connected ? "ok" : "", s.sim_connected ? "X-Plane connected" : "X-Plane idle");

  $("o-mounted").textContent = s.mounted ? "Mounted" : s.mount_error ? "Error: " + s.mount_error : "Not mounted";
  $("o-mountpoint").textContent = s.mount_point || "—";
  $("o-xplane").textContent = s.xplane ? s.xplane.path : "not found";
  $("o-sim").textContent = s.sim_connected ? "Connected" : "Not running";
  const a = s.aircraft;
  $("o-pos").textContent = a ? `${a.lat.toFixed(4)}, ${a.lon.toFixed(4)}  trk ${a.track_deg.toFixed(0)}°` : "—";
  $("o-spd").textContent = a ? `${(a.groundspeed_ms * 1.94384).toFixed(0)} kt  ·  ${(a.agl_m * 3.28084).toFixed(0)} ft AGL` : "—";
  $("o-dz").textContent = s.prefetch.reduced_zoom ? "Active (reduced resolution at altitude)" : "Off";

  const e = s.engine;
  $("o-provider").textContent = s.provider;
  $("o-reads").textContent = `${e.reads.toLocaleString()}  (${fmtMB(e.read_bytes)})`;
  $("o-fetched").textContent = `${e.fetch_requests.toLocaleString()}  (${fmtMB(e.fetch_bytes)})${e.fetch_errors ? ` · ${e.fetch_errors} errors` : ""}`;
  $("o-degraded").textContent = `${e.degraded_built} / ${e.upgrades}`;
  const total = e.latency_buckets_ms.reduce((t, [, c]) => t + c, 0) || 1;
  $("o-hist").innerHTML = e.latency_buckets_ms
    .map(([b, c]) => `<div style="height:${Math.max(2, (46 * c) / total)}px" title="${c} reads"></div>`)
    .join("");

  const m = s.mesh;
  $("o-catalog").textContent = m ? `${m.catalog_version} (${m.online ? "online" : "offline"})${m.pending_update ? ` · update ${m.pending_update} after X-Plane exits` : ""}` : "disabled";
  $("o-tiles").textContent = m ? `${m.tiles_listed} listed of ${m.tiles_in_catalog}` : "—";
  $("o-downloads").textContent = m ? `${m.downloads} (${fmtMB(m.download_bytes)})${m.failures ? ` · ${m.failures} failed` : ""}` : "—";
  $("o-queue").textContent = `${s.prefetch.queued} queued · ${s.prefetch.mesh_tiles_done} meshes · ${s.prefetch.imagery_tiles_done} imagery tiles`;
  if ($("o-attrib-imagery")) {
    $("o-attrib-imagery").textContent = `Imagery: ${s.provider_attribution || s.provider}`;
  }
  if ($("o-attrib-mesh")) {
    const catAttr = m && m.attribution && m.attribution.length
      ? m.attribution.join(" · ")
      : "Mesh data © OpenStreetMap contributors (ODbL 1.0) · Elevation data: USGS 3DEP";
    $("o-attrib-mesh").textContent = catAttr;
  }
  updateMapLive();
}

function renderHealth(h) {
  const ul = $("health");
  ul.innerHTML = "";
  for (const c of h.checks) {
    const li = document.createElement("li");
    li.innerHTML = `<span class="sev ${c.severity}">${c.severity}</span><b></b><span></span>`;
    li.children[1].textContent = c.title;
    li.children[2].textContent = c.message;
    if (c.fix && c.severity !== "ok") {
      const f = document.createElement("span");
      f.className = "fix";
      f.textContent = "→ " + c.fix;
      li.appendChild(f);
    }
    ul.appendChild(li);
  }
  const bad = h.checks.filter((c) => c.severity === "error");
  const banner = $("banner");
  banner.classList.toggle("hidden", bad.length === 0);
  banner.textContent = bad.map((c) => `${c.title}: ${c.message}${c.fix ? " — " + c.fix : ""}`).join("  ·  ");
}

// ---------------------------------------------------------------- on/off
// One switch. On: start the daemon, mount, enable our scenery_packs.ini entry.
// Off: unmount, disable the entry, stop the daemon (refused while X-Plane runs).
let poweredOn = false;
let powerBusy = false;

function renderPower(on) {
  poweredOn = on;
  const sw = $("power");
  sw.setAttribute("aria-checked", on ? "true" : "false");
  $("power-label").textContent = powerBusy ? (on ? "Stopping…" : "Starting…") : on ? "On" : "Off";
  sw.classList.toggle("busy", powerBusy);
  sw.disabled = powerBusy;
  document.body.classList.toggle("off", !on);
  $("off-card").classList.toggle("hidden", on);
  if (!on) {
    pill("pill-sim", "", "X-Plane");
    $("banner").classList.add("hidden");
  }
}

async function setPower(on) {
  if (powerBusy) return;
  powerBusy = true;
  renderPower(poweredOn);
  try {
    const st = await T.core.invoke(on ? "power_on" : "power_off");
    powerBusy = false;
    renderPower(st.on);
  } catch (e) {
    powerBusy = false;
    renderPower(poweredOn);
    alert(String(e));
  }
  if (poweredOn) refresh();
}

$("power").addEventListener("click", () => setPower(!poweredOn));
$("btn-turn-on").addEventListener("click", () => setPower(true));

// ---------------------------------------------------------------- logs
function addLog(l) {
  const pre = $("log");
  const line = document.createElement("div");
  line.className = l.level;
  const t = new Date(l.unix_time * 1000).toLocaleTimeString();
  line.textContent = `${t}  ${l.level.padEnd(5)}  ${l.message}`;
  pre.appendChild(line);
  while (pre.childElementCount > 1000) pre.removeChild(pre.firstChild);
  pre.scrollTop = pre.scrollHeight;
}

// ---------------------------------------------------------------- map
let map = null;
let mapReady = false;
let catalogTiles = [];

function tileToLatLon(x, y, z) {
  const n = Math.PI - (2 * Math.PI * y) / 2 ** z;
  return [(180 / Math.PI) * Math.atan(0.5 * (Math.exp(n) - Math.exp(-n))), (x / 2 ** z) * 360 - 180];
}

function squares(features) {
  return { type: "FeatureCollection", features };
}

function box(s, w, n, e, props) {
  return { type: "Feature", properties: props || {}, geometry: { type: "Polygon", coordinates: [[[w, s], [e, s], [e, n], [w, n], [w, s]]] } };
}

async function initMap() {
  if (map) {
    map.resize();
    refreshMapData();
    return;
  }
  if (!config) {
    try {
      config = await rpc("get_config");
    } catch (_) {}
  }
  const useEsri = !!(config && config.imagery && config.imagery.provider_notice_accepted);
  const tiles = useEsri
    ? ["https://services.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/{z}/{y}/{x}"]
    : ["https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/MapServer/tile/{z}/{y}/{x}"];
  const attribution = useEsri
    ? "Imagery © Esri, Maxar, Earthstar Geographics, and the GIS User Community"
    : "Imagery: USGS The National Map (3DEP / NAIP, public domain)";
  map = new maplibregl.Map({
    container: "mapview",
    style: {
      version: 8,
      sources: {
        imagery: {
          type: "raster",
          tiles,
          tileSize: 256,
          maxzoom: useEsri ? 17 : 16,
          attribution,
        },
      },
      layers: [{ id: "imagery", type: "raster", source: "imagery", paint: { "raster-saturation": -0.35, "raster-brightness-max": 0.85 } }],
    },
    center: [-73.9, 40.7],
    zoom: 6,
  });
  map.addControl(new maplibregl.NavigationControl(), "top-right");
  map.on("load", () => {
    for (const id of ["catalog", "cache", "prefetch", "aircraft"]) map.addSource(id, { type: "geojson", data: squares([]) });
    map.addLayer({ id: "catalog-fill", type: "fill", source: "catalog", paint: { "fill-color": "#5fb18a", "fill-opacity": ["case", ["get", "ready"], 0.45, 0.15] } });
    map.addLayer({ id: "catalog-line", type: "line", source: "catalog", paint: { "line-color": "#5fb18a", "line-width": 1 } });
    map.addLayer({ id: "cache-fill", type: "fill", source: "cache", paint: { "fill-color": "#3a6ea5", "fill-opacity": ["get", "opacity"] } });
    map.addLayer({ id: "prefetch-fill", type: "fill", source: "prefetch", paint: { "fill-color": "#ffc400", "fill-opacity": 0.2 } });
    map.addLayer({ id: "prefetch-line", type: "line", source: "prefetch", paint: { "line-color": "#ffc400", "line-width": 1.5 } });
    map.addLayer({ id: "aircraft", type: "circle", source: "aircraft", paint: { "circle-radius": 7, "circle-color": "#ffffff", "circle-stroke-color": "#1c4a3a", "circle-stroke-width": 3 } });
    map.on("click", "catalog-fill", (e) => {
      const p = e.features[0].properties;
      new maplibregl.Popup().setLngLat(e.lngLat).setText(`${p.tile} — ${p.ready ? "downloaded" : "available"}`).addTo(map);
    });
    mapReady = true;
    refreshMapData();
    updateMapLive();
  });
}

async function refreshMapData() {
  if (!mapReady) return;
  try {
    catalogTiles = await rpc("catalog_tiles");
    map.getSource("catalog").setData(squares(catalogTiles.map((t) => box(t.lat, t.lon, t.lat + 1, t.lon + 1, { tile: t.tile, ready: t.ready }))));
    const cache = await rpc("cache_stats");
    const now = Date.now() / 1000;
    const feats = cache.areas.map((a) => {
      const [n, w] = tileToLatLon(a.area.x, a.area.y, a.area.z);
      const [s, e] = tileToLatLon(a.area.x + 1, a.area.y + 1, a.area.z);
      const ageDays = (now - a.last_access) / 86400;
      return box(s, w, n, e, { opacity: Math.max(0.12, 0.55 - ageDays / 60) });
    });
    map.getSource("cache").setData(squares(feats));
    $("c-imagery").textContent = `${fmtMB(cache.imagery_bytes)} of ${fmtGB(cache.imagery_cap_bytes)} · ${cache.imagery_tiles.toLocaleString()} tiles`;
    $("c-mesh").textContent = `${fmtMB(cache.mesh_bytes)} of ${fmtGB(cache.mesh_cap_bytes)}`;
  } catch (e) {
    console.warn(e);
  }
}

function updateMapLive() {
  if (!mapReady || !status) return;
  const a = status.aircraft;
  map.getSource("aircraft").setData(squares(a ? [{ type: "Feature", properties: {}, geometry: { type: "Point", coordinates: [a.lon, a.lat] } }] : []));
  const area = status.prefetch.area;
  map.getSource("prefetch").setData(
    squares(area.length ? [{ type: "Feature", properties: {}, geometry: { type: "Polygon", coordinates: [[...area.map(([la, lo]) => [lo, la]), [area[0][1], area[0][0]]]] } }] : [])
  );
}

// ---------------------------------------------------------------- prepare
$("f-prepare").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const out = $("prepare-result");
  out.className = "result";
  out.textContent = "Looking up airport…";
  try {
    const r = await rpc("prepare_departure", { icao: $("icao").value.trim() });
    out.textContent = `${r.name || ""} (${r.lat.toFixed(4)}, ${r.lon.toFixed(4)}): queued ${r.mesh_tiles} mesh tiles and ${r.imagery_tiles} imagery tiles.`;
  } catch (e) {
    out.className = "result err";
    out.textContent = String(e);
  }
});

$("f-route").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const out = $("route-result");
  out.className = "result";
  out.textContent = "Fetching flight plan…";
  try {
    const user = $("sb-user").value.trim();
    const r = await rpc("prefetch_route", user ? { simbrief_user: user } : {});
    out.textContent = `${r.waypoints} waypoints: queued ${r.mesh_tiles} mesh tiles and ${r.imagery_tiles} imagery tiles.`;
  } catch (e) {
    out.className = "result err";
    out.textContent = String(e);
  }
});

// ---------------------------------------------------------------- packs
async function loadPacks() {
  const ul = $("packs-list");
  try {
    const packs = await rpc("packs_list");
    ul.innerHTML = "";
    if (!packs.length) ul.innerHTML = '<li class="muted">No imported packs.</li>';
    for (const p of packs) {
      const li = document.createElement("li");
      li.textContent = `${p.path} — ${p.tiles.length} tile${p.tiles.length === 1 ? "" : "s"}`;
      ul.appendChild(li);
    }
    refreshMapData();
  } catch (e) {
    ul.textContent = String(e);
  }
}

$("btn-import").addEventListener("click", async () => {
  const out = $("import-result");
  const dir = await T.dialog.open({ directory: true, title: "Choose an Ortho4XP/AutoOrtho pack folder" });
  if (!dir) return;
  out.className = "result";
  out.textContent = "Importing (copying files)…";
  try {
    const dest = await rpc("packs_import", { path: dir });
    out.textContent = "Imported to " + dest;
    loadPacks();
  } catch (e) {
    out.className = "result err";
    out.textContent = String(e);
  }
});

// ---------------------------------------------------------------- settings
const get = (o, path) => path.split(".").reduce((x, k) => (x == null ? x : x[k]), o);
function set(o, path, v) {
  const ks = path.split(".");
  const last = ks.pop();
  ks.reduce((x, k) => x[k], o)[last] = v;
}

async function loadSettings() {
  config = await rpc("get_config");
  for (const el of $("f-settings").elements) {
    if (!el.name) continue;
    const v = get(config, el.name);
    if (el.type === "checkbox") el.checked = !!v;
    else if (el.tagName === "TEXTAREA") el.value = (v || []).join("\n");
    else el.value = v ?? "";
  }
}

$("f-settings").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const out = $("settings-result");
  const c = structuredClone(config);
  for (const el of ev.target.elements) {
    if (!el.name) continue;
    const old = get(c, el.name);
    let v;
    if (el.type === "checkbox") v = el.checked;
    else if (el.tagName === "TEXTAREA") v = el.value.split("\n").map((s) => s.trim()).filter(Boolean);
    else if (el.type === "number") v = Number(el.value);
    else v = el.value.trim() === "" && (old === null || typeof old !== "string") ? null : el.value.trim();
    set(c, el.name, v);
  }
  out.className = "result";
  out.textContent = "Saving…";
  try {
    const r = await rpc("set_config", c);
    config = c;
    out.textContent = r.note || "Saved and applied.";
  } catch (e) {
    out.className = "result err";
    out.textContent = String(e);
  }
});

// ---------------------------------------------------------------- storage
let storageTarget = null;

function renderStorage(s) {
  const st = s.storage;
  if (!st) return;
  $("st-path").textContent = st.cache_path || "–";
  $("st-free").textContent = st.total_bytes ? `${fmtGB(st.available_bytes)} of ${fmtGB(st.total_bytes)}` : "–";
  const j = st.job;
  const prog = $("st-progress");
  const msg = $("st-msg");
  if (j && j.state === "running") {
    prog.classList.remove("hidden");
    prog.value = j.total_bytes ? Math.round((100 * j.done_bytes) / j.total_bytes) : 0;
    msg.className = "result";
    msg.textContent = `${j.mode === "move" ? "Moving" : "Switching"} to ${j.to}: ${fmtGB(j.done_bytes)} of ${fmtGB(j.total_bytes)}…`;
  } else {
    prog.classList.add("hidden");
    if (j && j.state === "failed") {
      msg.className = "result err";
      msg.textContent = "Cache move failed: " + j.message;
    } else if (j && j.state === "done") {
      msg.className = "result";
      msg.textContent = j.message;
    } else if (st.error) {
      msg.className = "result err";
      msg.textContent = st.error;
    }
  }
  $("btn-storage").disabled = !!(j && j.state === "running");
}

async function loadStorage() {
  try {
    const cache = await rpc("cache_stats");
    $("st-usage").textContent = `${fmtGB(cache.imagery_bytes + cache.mesh_bytes)} (imagery ${fmtGB(cache.imagery_bytes)}, meshes ${fmtGB(cache.mesh_bytes)})`;
  } catch (_) {}
  if (status) renderStorage(status);
}

$("btn-storage").addEventListener("click", async () => {
  const msg = $("st-msg");
  if (status && status.sim_connected) {
    msg.className = "result err";
    msg.textContent = "Close X-Plane first: the scenery is unmounted while the cache moves.";
    return;
  }
  const dir = await T.dialog.open({ directory: true, title: "Choose a folder for the ButterSat cache (any drive)" });
  if (!dir) return;
  try {
    const info = await rpc("storage_info", { path: dir });
    if (!info.writable) throw new Error("ButterSat can't write to " + info.path);
    storageTarget = info.path;
    const enough = info.available_bytes > info.current_cache_bytes + 512 * 1048576;
    $("st-confirm-text").textContent =
      `New folder: ${info.path} (${fmtGB(info.available_bytes)} free). ` +
      `The current cache holds ${fmtGB(info.current_cache_bytes)} in ${info.current_path}.` +
      (enough ? "" : " There may not be enough room to move it unless it's on the same drive.");
    $("st-confirm").classList.remove("hidden");
    msg.textContent = "";
  } catch (e) {
    msg.className = "result err";
    msg.textContent = String(e);
  }
});

async function relocate(mode) {
  $("st-confirm").classList.add("hidden");
  const msg = $("st-msg");
  try {
    renderStorage(Object.assign({}, status, { storage: await rpc("storage_relocate", { path: storageTarget, mode }) }));
  } catch (e) {
    msg.className = "result err";
    msg.textContent = String(e);
  }
}
$("st-move").addEventListener("click", () => relocate("move"));
$("st-fresh").addEventListener("click", () => relocate("fresh"));
$("st-cancel").addEventListener("click", () => $("st-confirm").classList.add("hidden"));

// ---------------------------------------------------------------- first launch: imagery choice
const COMMERCIAL = {
  esri: { name: "Esri", label: "Use Esri imagery", terms: "https://www.esri.com/en-us/legal/terms/full-master-agreement" },
  bing: { name: "Microsoft's (Bing Maps)", label: "Use Bing imagery", terms: "https://www.microsoft.com/maps/product/terms.html" },
};
let frPick = null;

async function saveImageryChoice(patch) {
  const c = await rpc("get_config");
  Object.assign(c.imagery, patch, { source_chosen: true });
  await rpc("set_config", c);
  config = c;
}

async function firstRun() {
  let c;
  try {
    c = await rpc("get_config");
  } catch (_) {
    return setTimeout(firstRun, 2000); // daemon still starting
  }
  if (c.imagery && c.imagery.source_chosen) return;
  const modal = $("first-run");
  const out = $("fr-result");
  modal.classList.remove("hidden");
  const done = () => {
    modal.classList.add("hidden");
    if (location.hash === "#settings") loadSettings();
    refresh();
  };
  $("fr-open").onclick = async () => {
    try {
      await saveImageryChoice({ provider: "open", prefer_open: true, provider_notice_accepted: false });
      done();
    } catch (e) {
      out.className = "result err";
      out.textContent = String(e);
    }
  };
  const pick = (id) => {
    frPick = id;
    for (const k of Object.keys(COMMERCIAL)) $("fr-" + k).classList.toggle("selected", k === id);
    $("fr-terms-name").textContent = COMMERCIAL[id].name;
    $("fr-confirm").textContent = COMMERCIAL[id].label;
    $("fr-accept").checked = false;
    $("fr-accept-row").classList.remove("hidden");
    $("fr-confirm").classList.remove("hidden");
    $("fr-confirm").disabled = true;
  };
  $("fr-esri").onclick = () => pick("esri");
  $("fr-bing").onclick = () => pick("bing");
  $("fr-accept").onchange = () => ($("fr-confirm").disabled = !$("fr-accept").checked);
  $("fr-terms").onclick = (ev) => {
    ev.preventDefault();
    try {
      if (frPick) T.opener.openUrl(COMMERCIAL[frPick].terms);
    } catch (_) {}
  };
  $("fr-confirm").onclick = async () => {
    if (!$("fr-accept").checked || !frPick) return;
    try {
      await saveImageryChoice({ provider: frPick, prefer_open: false, provider_notice_accepted: true });
      done();
    } catch (e) {
      out.className = "result err";
      out.textContent = String(e);
    }
  };
}

// ---------------------------------------------------------------- about
async function loadAbout() {
  const a = await T.core.invoke("about");
  $("about-version").textContent = a.version;
  if ($("about-active-attrib") && status) {
    const m = status.mesh;
    const cat = m
      ? `Active catalog ${m.catalog_version} (${m.license || "ODbL-1.0"}): ${(m.attribution || []).join(" · ")}`
      : "Mesh catalog: © OpenStreetMap contributors (ODbL 1.0) · USGS 3DEP";
    $("about-active-attrib").textContent = `Active imagery: ${status.provider_attribution || status.provider} — ${cat}`;
  }
  $("notices").innerHTML = a.notices.map((n) => `<li></li>`).join("");
  [...$("notices").children].forEach((li, i) => (li.textContent = a.notices[i]));
}

$("btn-check-update").addEventListener("click", () => checkUpdates(true));
$("btn-update").addEventListener("click", async (ev) => {
  if (status && status.sim_connected) {
    alert("Close X-Plane before installing an update so your flight is not interrupted.");
    return;
  }
  const b = ev.currentTarget;
  b.disabled = true;
  $("update-text").textContent = `Downloading and installing ButterSat ${pendingUpdate ? pendingUpdate.version : ""}…`;
  try {
    await T.core.invoke("update_install");
  } catch (e) {
    alert(e);
    b.disabled = false;
    renderUpdateBanner();
  }
});

// Background updates (src/main.rs auto_update): installs by itself once
// X-Plane is closed; the app restarts with the new daemon.
T.event.listen("update-available", ({ payload }) => {
  pendingUpdate = { available: true, version: payload, body: "it installs automatically when X-Plane is not running" };
  renderUpdateBanner();
});
T.event.listen("update-installing", ({ payload }) => {
  pendingUpdate = { available: true, version: payload };
  $("update-banner").classList.remove("hidden");
  $("btn-update").disabled = true;
  $("update-text").textContent = `Installing ButterSat ${payload}… ButterSat restarts by itself in a moment.`;
});

// ---------------------------------------------------------------- wiring
async function refresh() {
  if (powerBusy) return;
  try {
    const st = await T.core.invoke("power_state");
    if (!st.on) {
      // Switched off; a daemon may still be finishing its exit.
      renderPower(false);
      return;
    }
    renderPower(true);
    renderStatus(await rpc("status"));
    renderHealth(await rpc("health"));
  } catch (e) {
    $("banner").classList.remove("hidden");
    $("banner").textContent = "Cannot reach the ButterSat daemon: " + e;
  }
}

T.event.listen("daemon-event", ({ payload }) => {
  switch (payload.kind) {
    case "stats":
      renderStatus(payload.data);
      break;
    case "health":
      renderHealth(payload.data);
      break;
    case "log":
      addLog(payload.data);
      break;
  }
});
T.event.listen("daemon-disconnected", () => {
  setTimeout(refresh, 2000);
});

show(location.hash.slice(1) || "overview");
refresh();
firstRun();
checkUpdates(false);
setInterval(refreshMapData, 30000);
