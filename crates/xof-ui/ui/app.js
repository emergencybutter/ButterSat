// xporthofs control UI. Talks to the daemon through the Tauri `rpc` command
// and receives pushed `daemon-event`s (stats, aircraft, logs, health).
"use strict";

const T = window.__TAURI__;
const rpc = (method, params) => T.core.invoke("rpc", { method, params: params ?? null });
const $ = (id) => document.getElementById(id);
const fmtMB = (b) => (b / 1048576).toFixed(b < 10 * 1048576 ? 1 : 0) + " MB";
const fmtGB = (b) => (b / 1073741824).toFixed(1) + " GB";

let status = null;
let config = null;

// ---------------------------------------------------------------- navigation
function show(view) {
  document.querySelectorAll(".view").forEach((v) => v.classList.toggle("active", v.id === view));
  document.querySelectorAll(".side a").forEach((a) => a.classList.toggle("active", a.getAttribute("href") === "#" + view));
  if (view === "map") initMap();
  if (view === "settings") loadSettings();
  if (view === "packs") loadPacks();
  if (view === "about") loadAbout();
}
window.addEventListener("hashchange", () => show(location.hash.slice(1) || "overview"));

// ---------------------------------------------------------------- overview
function pill(id, cls, text) {
  const p = $(id);
  p.className = "pill " + cls;
  if (text) p.textContent = text;
}

function renderStatus(s) {
  status = s;
  pill("pill-daemon", "ok", "daemon");
  pill("pill-mount", s.mounted ? "ok" : "warn", s.mounted ? "mounted" : "not mounted");
  pill("pill-sim", s.sim_connected ? "ok" : "", s.sim_connected ? "X-Plane connected" : "X-Plane idle");
  const btn = $("btn-mount");
  btn.textContent = s.mounted ? "Unmount" : "Mount";
  btn.classList.toggle("primary", !s.mounted);

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

$("btn-mount").addEventListener("click", async (ev) => {
  const b = ev.currentTarget;
  b.disabled = true;
  try {
    await rpc(status && status.mounted ? "unmount" : "mount");
  } catch (e) {
    alert(e);
  } finally {
    b.disabled = false;
    refresh();
  }
});

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
  map = new maplibregl.Map({
    container: "mapview",
    style: {
      version: 8,
      sources: {
        imagery: {
          type: "raster",
          tiles: ["https://services.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/{z}/{y}/{x}"],
          tileSize: 256,
          maxzoom: 17,
          attribution: "Imagery © Esri",
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

// ---------------------------------------------------------------- about
async function loadAbout() {
  const a = await T.core.invoke("about");
  $("about-version").textContent = a.version;
  $("notices").innerHTML = a.notices.map((n) => `<li></li>`).join("");
  [...$("notices").children].forEach((li, i) => (li.textContent = a.notices[i]));
}

// ---------------------------------------------------------------- wiring
async function refresh() {
  try {
    renderStatus(await rpc("status"));
    renderHealth(await rpc("health"));
  } catch (e) {
    pill("pill-daemon", "err", "daemon offline");
    $("banner").classList.remove("hidden");
    $("banner").textContent = "Cannot reach the xporthofs daemon: " + e;
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
  pill("pill-daemon", "err", "daemon offline");
  setTimeout(refresh, 2000);
});

show(location.hash.slice(1) || "overview");
refresh();
setInterval(refreshMapData, 30000);
