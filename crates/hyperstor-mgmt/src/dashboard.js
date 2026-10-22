// The dashboard: polls /api/status and /api/metrics every two seconds and
// draws them. With logins (hyperstor-mgmt --authd) it asks for one first
// (/auth/login) and shows who is logged in. Data reaches the page only
// through textContent, never as markup.
"use strict";

const $ = (id) => document.getElementById(id);
let prev = null; // the last metrics answer, with its time, for rates
let csrf = null; // the session's CSRF value, for writes
let timer = null;

class NeedLogin extends Error {}

async function getJson(path) {
  const r = await fetch(path);
  if (r.status === 401) throw new NeedLogin();
  const j = await r.json();
  if (!r.ok) throw new Error(j.message || `${path}: ${r.status}`);
  return j;
}

function el(tag, attrs, ...kids) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (k === "class") e.className = v;
    else if (k === "style") e.setAttribute("style", v);
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else e.setAttribute(k, v);
  }
  for (const k of kids) e.append(k instanceof Node ? k : document.createTextNode(String(k)));
  return e;
}

const level = (h) => ({ HEALTH_OK: "ok", HEALTH_WARN: "warn", HEALTH_ERROR: "error" }[h] || "unknown");

function fmtBytes(n) {
  const u = ["B", "KiB", "MiB", "GiB", "TiB"];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return (i ? n.toFixed(1) : n.toFixed(0)) + " " + u[i];
}
const fmtRate = (n) => (n >= 100 ? n.toFixed(0) : n.toFixed(1));
const fmtUs = (s) => (s == null ? "–" : s < 1e-3 ? (s * 1e6).toFixed(0) + " µs" : (s * 1e3).toFixed(1) + " ms");

function header(cols) {
  return el("tr", null, ...cols.map(([t, num]) => el("th", num ? { class: "num" } : null, t)));
}

function drawStatus(s) {
  $("cluster").textContent = `cluster ${s.cluster_id} · map epoch ${s.epoch}`;
  const lv = level(s.health);
  const b = $("badge");
  b.className = "badge " + lv;
  b.textContent = lv;
  const r = $("reasons");
  r.replaceChildren(...(s.reasons || []).map((x) =>
    el("li", null, el("span", { class: "lvl " + level(x.level) }, level(x.level)), x.text)));
  if (!(s.reasons || []).length) r.replaceChildren(el("li", { class: "muted" }, "Every PG has all its copies."));

  const pools = $("pools");
  pools.replaceChildren(header([["Pool"], ["Kind"], ["Copies", 1], ["PGs", 1], ["States"], ["Clean", 1], ["Degraded", 1], ["Backfill", 1], ["Inactive", 1], [""]]));
  for (const p of s.pools || []) {
    const c = p.pgs || {};
    const total = p.pg_count || 1;
    const bar = el("div", { class: "bar", title: "PG states" });
    for (const k of ["clean", "degraded", "backfilling", "inactive"]) {
      const n = c[k] || 0;
      if (n) bar.append(el("span", { class: "s-" + k, style: `width:${(100 * n) / total}%` }));
    }
    pools.append(el("tr", null,
      el("td", null, `${p.id || 0} ${p.name}`), el("td", null, p.kind),
      el("td", { class: "num" }, `${p.size}/${p.min_size}`), el("td", { class: "num" }, total),
      el("td", null, bar),
      el("td", { class: "num" }, c.clean || 0), el("td", { class: "num" }, c.degraded || 0),
      el("td", { class: "num" }, c.backfilling || 0), el("td", { class: "num" }, c.inactive || 0),
      el("td", null, el("button", { class: "link", onclick: () => showPgs(p.id || 0) }, "PGs"))));
  }
  return s;
}

async function showPgs(pool) {
  const pre = $("pgs");
  pre.hidden = false;
  pre.textContent = "loading…";
  const j = await getJson(`/api/pgs?pool=${pool}`);
  const set = (v) => "[" + (v || []).map((d) => (d === 4294967295 ? "-" : d)).join(",") + "]";
  pre.textContent = `PG        epoch  acting        up\n` + (j.pgs || [])
    .map((g) => `${pool}.${g.pg || 0}`.padEnd(10) + String(g.epoch || 0).padStart(5) + "  " +
      set(g.acting).padEnd(14) + set(g.up)).join("\n");
}

// Sum of a metric's samples on one node, filtered by labels.
function sum(node, name, want) {
  let t = 0;
  for (const [n, labels, v] of node.samples) {
    if (n !== name) continue;
    if (want && Object.entries(want).some(([k, x]) => labels[k] !== x)) continue;
    t += v;
  }
  return t;
}

// A quantile from the change in histogram buckets between two scrapes.
function quantile(now, before, kind, q, device) {
  const buckets = (node) => node.samples
    .filter(([n, l]) => n === "hyperstor_request_duration_seconds_bucket" && l.kind === kind && l.device === device)
    .map(([, l, v]) => [l.le === "+Inf" ? Infinity : parseFloat(l.le), v])
    .sort((a, b) => a[0] - b[0]);
  const a = buckets(now), b = before ? buckets(before) : [];
  const d = a.map(([le, v], i) => [le, v - (b[i] ? b[i][1] : 0)]);
  const total = d.length ? d[d.length - 1][1] : 0;
  if (total <= 0) return null;
  for (const [le, v] of d) if (v >= q * total) return le === Infinity ? null : le;
  return null;
}

function drawMetrics(m, status) {
  const t = performance.now() / 1000;
  const before = prev ? Object.fromEntries(prev.m.nodes.map((n) => [n.addr, n])) : {};
  const dt = prev ? t - prev.t : 0;
  prev = { m, t };

  // Per device, from every node that answered: samples carry the
  // device's id, the same as the map's.
  const dev = {};
  let wops = 0, rops = 0, wbytes = 0, rbytes = 0, free = 0, data = 0;
  for (const n of m.nodes) {
    if (!n.up) continue;
    const ids = new Set(n.samples.map(([, l]) => l.device).filter((d) => d !== undefined));
    for (const id of ids) {
      const f = (name, want) => sum(n, name, { ...want, device: id });
      const r = (name, want) => (dt > 0 && before[n.addr] ? (f(name, want) - sum(before[n.addr], name, { ...want, device: id })) / dt : 0);
      const x = {
        w: r("hyperstor_requests_total", { kind: "write" }), r: r("hyperstor_requests_total", { kind: "read" }),
        wb: r("hyperstor_request_bytes_total", { kind: "write" }), rb: r("hyperstor_request_bytes_total", { kind: "read" }),
        wp99: quantile(n, before[n.addr], "write", 0.99, id), rp99: quantile(n, before[n.addr], "read", 0.99, id),
        free: f("hyperstor_device_free_bytes"), data: f("hyperstor_device_data_bytes"),
        logUsed: f("hyperstor_log_used_bytes"), log: f("hyperstor_log_bytes"),
        epoch: f("hyperstor_map_epoch"), errors: f("hyperstor_write_errors_total"),
      };
      dev[id] = x;
      wops += x.w; rops += x.r; wbytes += x.wb; rbytes += x.rb; free += x.free; data += x.data;
    }
  }

  const tile = (v, l) => el("div", { class: "tile" }, el("div", { class: "v" }, v), el("div", { class: "l" }, l));
  $("tiles").replaceChildren(
    tile(fmtRate(wops), "writes / s"), tile(fmtRate(rops), "reads / s"),
    tile(fmtBytes(wbytes) + "/s", "written"), tile(fmtBytes(rbytes) + "/s", "read"),
    tile(data ? ((100 * (data - free)) / data).toFixed(1) + " %" : "–", `used of ${fmtBytes(data)}`));
  // Space: health doesn't know about it yet (D27 isn't built), so warn here.
  const used = data ? (data - free) / data : 0;
  const tiles = $("tiles").lastChild;
  tiles.classList.toggle("warn", used >= 0.85 && used < 0.95);
  tiles.classList.toggle("error", used >= 0.95);
  const r = $("reasons");
  if (used >= 0.85) {
    r.append(el("li", null, el("span", { class: "lvl " + (used >= 0.95 ? "error" : "warn") }, used >= 0.95 ? "full" : "nearfull"),
      `devices are ${(100 * used).toFixed(0)} % used; writes will fail when they fill`));
  }

  const tbl = $("nodes");
  tbl.replaceChildren(header([["Node"], ["Device"], ["Address"], ["State"], ["Heard"], ["Writes/s", 1], ["Reads/s", 1], ["Write p99", 1], ["Read p99", 1], ["Used", 1], ["Log", 1], ["Epoch", 1], ["Errors", 1]]));
  for (const node of status.nodes || []) {
    for (const d of node.devices || []) {
      const x = dev[String(d.id || 0)];
      const state = !node.up ? ["down", "down"] : d.up_in ? ["up", "in"] : ["out", "out"];
      const heard = node.last_heard_ms == null ? "never" : (Number(node.last_heard_ms) / 1000).toFixed(1) + " s ago";
      tbl.append(el("tr", null,
        el("td", null, node.id || 0), el("td", null, d.id || 0), el("td", null, d.addr),
        el("td", null, el("span", { class: "dot " + state[0] }), state[1]),
        el("td", { class: "muted" }, heard),
        el("td", { class: "num" }, x ? fmtRate(x.w) : "–"), el("td", { class: "num" }, x ? fmtRate(x.r) : "–"),
        el("td", { class: "num" }, x ? fmtUs(x.wp99) : "–"), el("td", { class: "num" }, x ? fmtUs(x.rp99) : "–"),
        el("td", { class: "num" }, x && x.data ? fmtBytes(x.data - x.free) + " of " + fmtBytes(x.data) : "no metrics"),
        el("td", { class: "num" }, x && x.log ? ((100 * x.logUsed) / x.log).toFixed(0) + " %" : "–"),
        el("td", { class: "num" }, x ? x.epoch : "–"), el("td", { class: "num" }, x ? x.errors : "–")));
    }
  }
}

async function tick() {
  try {
    const [status, m] = await Promise.all([getJson("/api/status"), getJson("/api/metrics")]);
    drawStatus(status);
    drawMetrics(m, status);
    $("updated").textContent = "updated " + new Date().toLocaleTimeString();
    $("updated").classList.remove("error-box");
  } catch (e) {
    if (e instanceof NeedLogin) return showLogin();
    $("updated").textContent = "not updating: " + e.message;
    $("updated").classList.add("error-box");
  }
}

function start() {
  $("login").hidden = true;
  $("main").hidden = false;
  tick();
  if (!timer) timer = setInterval(tick, 2000);
}

async function showLogin() {
  clearInterval(timer);
  timer = null;
  $("main").hidden = true;
  $("who").hidden = true;
  $("login").hidden = false;
  try {
    const m = await getJson("/auth/methods");
    $("claim-row").hidden = !m.claim_open;
  } catch (_) { /* the form works without it */ }
}

async function whoami() {
  try {
    const w = await getJson("/auth/whoami");
    csrf = w.csrf;
    $("user").textContent = `${w.user} (${(w.roles || []).join(", ") || "no roles yet"})`;
    $("who").hidden = false;
  } catch (e) {
    if (e instanceof NeedLogin) return false;
    // No logins here (the loopback dashboard): nothing to show.
  }
  return true;
}

$("login-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  $("login-error").textContent = "";
  const body = { user: $("login-user").value, password: $("login-password").value };
  const claim = $("login-claim").value.trim();
  if (claim) body.claim = claim;
  const r = await fetch("/auth/login", {
    method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body),
  });
  $("login-password").value = "";
  const j = await r.json().catch(() => ({}));
  if (!r.ok) { $("login-error").textContent = j.message || `login failed (${r.status})`; return; }
  $("login-claim").value = "";
  await whoami();
  start();
});

$("logout").addEventListener("click", async () => {
  await fetch("/auth/logout", { method: "POST", headers: { "X-Hyperstor-CSRF": csrf || "" } });
  csrf = null;
  showLogin();
});

whoami().then((ok) => (ok ? start() : showLogin()));
