"use strict";
let ROWS = [];
try { ROWS = JSON.parse(document.getElementById("golem-data").textContent); } catch (e) { ROWS = []; }

const $ = (id) => document.getElementById(id);
const esc = (s) => String(s ?? "").replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
const fmt = (v, d = 1) => (v == null || isNaN(v)) ? "–" : Number(v).toFixed(d);
const day = (ts) => (ts || "").slice(0, 10);
// A verdict as a class name: only the two the stylesheet knows, whatever a
// row says.
const verdict = (v) => (v === "PASS" || v === "FAIL" ? v : "none");
// The seals that say what a number is worth: passed the 200K gate on this
// machine, failed it, benched only, or never measured.
const SEALS = {
  proven: `<svg viewBox="0 0 16 16" aria-hidden="true"><path fill="currentColor" d="M8 .8l1.7 1.3 2.1-.3.8 2 2 .8-.3 2.1L15.2 8l-1.3 1.7.3 2.1-2 .8-.8 2-2.1-.3L8 15.2l-1.7-1.3-2.1.3-.8-2-2-.8.3-2.1L.8 8l1.3-1.7-.3-2.1 2-.8.8-2 2.1.3z"/><path d="M5 8.2l2 2 4-4.2" fill="none" stroke="var(--surface)" stroke-width="1.7"/></svg>`,
  failed: `<svg viewBox="0 0 16 16" aria-hidden="true"><circle cx="8" cy="8" r="6.2" fill="none" stroke="currentColor" stroke-width="1.5"/><path d="M5.6 5.6l4.8 4.8M10.4 5.6l-4.8 4.8" stroke="currentColor" stroke-width="1.5"/></svg>`,
  measured: `<svg viewBox="0 0 16 16" aria-hidden="true"><circle cx="8" cy="8" r="6.2" fill="none" stroke="currentColor" stroke-width="1.5"/><path d="M8 1.8a6.2 6.2 0 0 1 0 12.4z" fill="currentColor"/></svg>`,
  none: `<svg viewBox="0 0 16 16" aria-hidden="true"><circle cx="8" cy="8" r="6.2" fill="none" stroke="currentColor" stroke-width="1.5" stroke-dasharray="2.4 2.2"/></svg>`,
};
// The agent floor the page judges a suite by (TUNE_MIN_AGENT's default),
// and the APIs by the names a person knows them by.
const AGENT_FLOOR = 0.75;
const API_NAMES = { anthropic: "Messages", openai: "Chat Completions" };
// The records, one per line; a line that will not parse (a write cut short)
// is skipped rather than costing the page every other row.
const parseRows = (text) => text.split("\n").filter((l) => l.trim().startsWith("{")).flatMap((l) => {
  try { return [JSON.parse(l)]; } catch (e) { return []; }
});
// A model file's quant, read from its name as golem_core::quant reads it:
// a row recorded since quants are recorded carries its own, an older one
// has only its model_file. A model in another quant is another model, so
// rows are told apart and compared by the preset *and* its quant.
function quantFromName(file) {
  const parts = String(file || "").split(/[\\/]/).pop().replace(/\.gguf$/i, "").split(/[-.]/);
  for (let i = parts.length - 1; i >= 0; i--) {
    const u = parts[i].toUpperCase();
    const named = /^(BF16|F16|F32|FP16|FP32)$/.test(u) || u.startsWith("MXFP4") || (/^(IQ|TQ|Q)\d/.test(u) && u.includes("_"));
    if (/^[A-Z0-9_]+$/.test(u) && named) return (i > 0 && parts[i - 1].toUpperCase() === "UD" ? "UD-" : "") + u;
  }
  return "";
}
const quantOf = (r) => r?.quant || quantFromName(r?.model_file);
const idOf = (r) => (quantOf(r) ? `${r.model} ${quantOf(r)}` : r.model);
const quantTag = (q) => (q ? ` <span class="quant">${esc(q)}</span>` : "");
const post = (path, body) => fetch(path, { method: "POST", headers: { "Content-Type": "application/x-www-form-urlencoded" }, body });

// ---- the window (golem app) -------------------------------------------
// The app opens the page with ?app; the title bar then moves, resizes and
// closes the window through the webview's message channel - wry's
// window.ipc, or WebView2's own chrome.webview under it.
const send = window.ipc?.postMessage ? (m) => window.ipc.postMessage(m)
  : window.chrome?.webview?.postMessage ? (m) => window.chrome.webview.postMessage(m) : null;
const APP = (new URLSearchParams(location.search).has("app") || !!window.__GOLEM_APP__) && !!send;
if (APP) {
  document.documentElement.classList.add("app");
  const ask = send;
  $("bar").addEventListener("mousedown", (e) => {
    if (e.button !== 0 || e.target.closest("a, button, select, input")) return;
    if (e.detail === 2) ask("maximize"); else ask("drag");
  });
  document.querySelectorAll("[data-win]").forEach((b) => b.onclick = () => ask(b.dataset.win));
  document.querySelectorAll("[data-edge]").forEach((d) => d.addEventListener("mousedown", (e) => {
    if (e.button === 0) { e.preventDefault(); ask("resize:" + d.dataset.edge); }
  }));
}

// ---- the mark ---------------------------------------------------------
// The same cells as golem_core::mark::GRID (a test keeps them equal):
// 0 empty, 1 clay, 2 spark. The spark shows the engine's state.
const GRID = ["011110", "122221", "111111", "011110"];
function drawMark(state) {
  const cw = 10, ch = 16, pad = 2;
  const spark = state === "up" ? "var(--ok)" : state === "busy" ? "var(--busy)" : state === "down" ? "var(--soft)" : "var(--spark)";
  let out = "";
  GRID.forEach((row, r) => [...row].forEach((c, k) => {
    if (c === "0") return;
    out += `<rect x="${pad + k * cw}" y="${pad + r * ch}" width="${cw + .3}" height="${ch + .3}" fill="${c === "2" ? spark : "var(--clay)"}"/>`;
  }));
  $("mark").innerHTML = out;
}
drawMark();

// ---- tabs -------------------------------------------------------------
let LIVE = false;
function showTab(name) {
  const tabs = [...document.querySelectorAll(".tabs a")].filter((a) => !a.hidden).map((a) => a.dataset.tab);
  if (!tabs.includes(name)) name = tabs[0];
  document.querySelectorAll(".tabs a").forEach((a) => {
    if (a.dataset.tab === name) a.setAttribute("aria-current", "page");
    else a.removeAttribute("aria-current");
  });
  ["models", "chat", "results", "log", "settings"].forEach((t) => { $("tab-" + t).hidden = t !== name; });
  $("chat-tools").hidden = name !== "chat";
  if (name === "chat") showChat();
  if (name === "settings") loadSettings();
}
window.addEventListener("hashchange", () => showTab(location.hash.slice(1)));

// The phone's panel of sections: the mark opens it; a section, the scrim or
// Escape closes it.
function setNav(open) {
  document.documentElement.classList.toggle("nav-open", open);
  $("scrim").hidden = !open;
  $("nav-toggle").setAttribute("aria-expanded", String(open));
  if (open) document.querySelector(".tabs a[aria-current]")?.focus();
}
$("nav-toggle").onclick = () => setNav(!document.documentElement.classList.contains("nav-open"));
$("scrim").onclick = () => setNav(false);
document.querySelectorAll(".tabs a").forEach((a) => a.addEventListener("click", () => setNav(false)));
document.addEventListener("keydown", (e) => { if (e.key === "Escape") setNav(false); });

// ---- results ----------------------------------------------------------
const KEY_FLAGS = ["n-cpu-moe", "fit", "fit-target", "ubatch-size", "spec-type", "ctx-size", "cache-type-k"];
const flagStr = (f) => KEY_FLAGS.filter((k) => f && f[k] != null).map((k) => `${k.replace("cache-type-", "kv-")}=${f[k]}`).join(" ");

// What the model is, read from its GGUF header when the row was recorded.
const paramsCell = (r) => {
  if (r.params_b == null) return "–";
  let s = `${fmt(r.params_b, 1)}B`;
  if (r.moe && r.params_active_b != null) s += ` / ${fmt(r.params_active_b, 1)}B active`;
  if (r.experts) s += `, ${esc(r.experts)}`;
  return s;
};

// Comparing tok/s across model sizes is not a comparison, so the leaderboard
// ranks inside size classes, and the cross-size column is an explicit
// ratio - decode per billion *active* parameters.
const CLASSES = ["Up to 2B active", "2–8B active", "8–20B active", "20B+ active", "Unknown size"];
const classOf = (r) => {
  const p = r.params_active_b ?? r.params_b;
  if (p == null) return CLASSES[4];
  if (p < 2) return CLASSES[0];
  if (p < 8) return CLASSES[1];
  if (p < 20) return CLASSES[2];
  return CLASSES[3];
};
const efficiency = (r) => {
  const p = r.params_active_b ?? r.params_b;
  return p && r.decode_median ? r.decode_median / p : null;
};
const modelLine = (r) => {
  const bits = [];
  if (r.params_b != null) bits.push(`${fmt(r.params_b, 1)}B params` + (r.moe && r.params_active_b != null ? ` (${fmt(r.params_active_b, 1)}B active)` : ""));
  if (r.moe) bits.push(`MoE${r.experts ? " " + r.experts : ""}`);
  if (r.n_layer) bits.push(`${r.n_layer} layers`);
  if (r.ctx) bits.push(`served ctx ${(r.ctx / 1024).toFixed(0)}K` + (r.ctx_train && r.ctx_train != r.ctx ? ` (native ${(r.ctx_train / 1024).toFixed(0)}K)` : ""));
  return bits.join(", ");
};

let modelNames = [];
const colorOf = (m) => `var(--s${(modelNames.indexOf(m) % 6) + 1})`;
function fillSelect(id, values) {
  const sel = $(id), cur = sel.value;
  sel.innerHTML = `<option value="">All</option>` + values.map((v) => `<option>${esc(v)}</option>`).join("");
  if (values.includes(cur)) sel.value = cur;
}
function fillFilters() {
  modelNames = [...new Set(ROWS.map(idOf))].sort();
  fillSelect("f-host", [...new Set(ROWS.map((r) => r.host))].sort());
  fillSelect("f-engine", [...new Set(ROWS.map((r) => r.engine))].sort());
  fillSelect("f-model", [...new Set(ROWS.map((r) => r.model))].sort());
}
fillFilters();
fillSelect("f-class", CLASSES);
fillSelect("f-class-chart", CLASSES);
// Numbers only compare on the same box, so one host is shown at a time:
// this machine's, once the service names it (api/state), and until then -
// or in a static report, which has no service - the host most rows come from.
const hostCount = {};
ROWS.forEach((r) => { hostCount[r.host] = (hostCount[r.host] || 0) + 1; });
let topHost = Object.keys(hostCount).sort((a, b) => hostCount[b] - hostCount[a])[0];
if (topHost) $("f-host").value = topHost;
let THIS_HOST = "";
function setThisHost(host) {
  if (!host || host === THIS_HOST) return;
  THIS_HOST = host;
  const shown = $("f-host").value === topHost;
  topHost = host;
  if (shown && ROWS.some((r) => r.host === host)) { $("f-host").value = host; render(); }
}

function filtered() {
  const h = $("f-host").value, e = $("f-engine").value, m = $("f-model").value, q = $("f-quiet").checked, c = $("f-class").value;
  return ROWS.filter((r) => (!h || r.host === h) && (!e || r.engine === e) && (!m || r.model === m)
    && (!q || r.quiet !== false) && (!c || classOf(r) === c));
}

function renderBoard(rows) {
  const groups = {};
  rows.forEach((r) => {
    const k = [r.host, r.engine, r.engine_build, idOf(r)].join("|");
    const g = (groups[k] = groups[k] || { bench: [], eval: [], agent: [] });
    if (r.kind === "eval") g.eval.push(r); else if (r.kind !== "agent") g.bench.push(r);
    if (agentOf(r)) g.agent.push(r);
  });
  const newest = (list) => list.sort((a, b) => (a.ts < b.ts ? 1 : -1))[0];
  const lines = Object.values(groups).filter((g) => g.bench.length).map((g) => {
    const best = g.bench.reduce((a, b) => ((b.decode_median ?? 0) > (a.decode_median ?? 0) ? b : a));
    return { best, ev: newest(g.eval), ag: newest(g.agent) };
  }).sort((a, b) => {
    const ca = CLASSES.indexOf(classOf(a.best)), cb = CLASSES.indexOf(classOf(b.best));
    return ca - cb || (b.best.decode_median ?? 0) - (a.best.decode_median ?? 0);
  });
  if (!lines.length) { $("board").innerHTML = `<tr><td class="empty">No bench runs match the filters.</td></tr>`; return; }
  const bestByClass = {};
  lines.forEach(({ best: r }) => {
    const c = classOf(r);
    if (!(c in bestByClass) || (r.decode_median ?? 0) > bestByClass[c]) bestByClass[c] = r.decode_median;
  });
  let lastClass = null;
  $("board").innerHTML = `<thead><tr><th>Model</th><th>Engine</th><th>Params</th><th class="num">Ctx</th><th class="num">Threads</th><th class="num">Decode</th><th class="num">Best</th>
    <th class="num">MTP</th><th class="num">Prefill</th><th class="num">t/s per active B</th><th class="num">VRAM GB</th><th class="num">RAM GB</th><th>200K needle</th><th class="num">At depth</th><th>Agent</th><th>Date</th><th>Flags</th></tr></thead><tbody>` +
    lines.map(({ best: r, ev, ag }) => {
      const needle = ev?.eval?.needle;
      const work = agentOf(ag);
      const cls = classOf(r);
      const head = cls !== lastClass ? `<tr class="classrow"><td colspan="17">${esc(cls)}</td></tr>` : "";
      lastClass = cls;
      return head + `<tr><td><span class="swatch" style="background:${colorOf(idOf(r))}"></span>${esc(r.model)}${quantTag(quantOf(r))}</td>
        <td>${esc(r.engine)} <span class="flags">${esc(r.engine_build)}</span></td>
        <td>${paramsCell(r)}</td>
        <td class="num">${r.ctx ? (r.ctx / 1024).toFixed(0) + "K" : "–"}</td>
        <td class="num">${r.threads != null ? r.threads : "–"}</td>
        <td class="num ${r.decode_median === bestByClass[cls] ? "best" : ""}">${fmt(r.decode_median)}</td><td class="num">${fmt(r.decode_best)}</td>
        <td class="num">${r.draft_accept == null ? "–" : Math.round(r.draft_accept * 100) + "%"}</td>
        <td class="num">${fmt(r.prefill, 0)}</td><td class="num">${fmt(efficiency(r), 1)}</td>
        <td class="num">${r.vram_mib ? fmt(r.vram_mib / 1024) : "–"}</td>
        <td class="num">${r.ram_used_gib != null ? fmt(r.ram_used_gib) : "–"}</td>
        <td>${needle === "PASS" ? `<span class="seal proven">${SEALS.proven}<span>Proven</span></span>`
          : needle ? `<span class="seal failed">${SEALS.failed}<span>Failed</span></span>`
          : `<span class="seal none">${SEALS.none}<span>Not gated</span></span>`}${ev?.eval?.tools === "FAIL" ? ` <span class="badge FAIL">Tools</span>` : ""}</td>
        <td class="num">${fmt(ev?.eval?.depth_decode)}</td>
        <td>${work ? `<span class="seal ${work.pass ? "proven" : "failed"}" title="${esc(work.apis)}, ${esc(day(ag.ts))}">${work.pass ? SEALS.proven : SEALS.failed}<span>${work.solved}/${work.total}</span></span>` : "–"}</td>
        <td>${esc(day(r.ts))}</td><td class="flags">${esc(flagStr(r.flags))}</td></tr>`;
    }).join("") + "</tbody>";
}

function renderScatter(rows) {
  const active = $("f-class-chart").value;
  $("chart-class").textContent = active ? active : "every size";
  const pts = rows.filter((r) => r.kind === "bench" && r.decode_median && r.prefill && (!active || classOf(r) === active));
  if (!pts.length) { $("scatter").innerHTML = `<div class="empty">No bench runs with both decode and prefill.</div>`; $("legend").innerHTML = ""; return; }
  const W = 900, H = 320, L = 48, R = 16, T = 12, B = 34;
  const xs = pts.map((p) => p.prefill), ys = pts.map((p) => p.decode_median);
  const pad = (lo, hi) => { const d = (hi - lo) || hi * 0.2 || 1; return [Math.max(0, lo - d * 0.15), hi + d * 0.15]; };
  const [x0, x1] = pad(Math.min(...xs), Math.max(...xs)), [y0, y1] = pad(Math.min(...ys), Math.max(...ys));
  const X = (v) => L + (v - x0) / (x1 - x0) * (W - L - R), Y = (v) => H - B - (v - y0) / (y1 - y0) * (H - T - B);
  const ticks = (lo, hi) => { const s = Math.pow(10, Math.floor(Math.log10((hi - lo) / 4))); const st = [1, 2, 5, 10].map((m) => m * s).find((v) => (hi - lo) / v <= 6); const out = []; for (let v = Math.ceil(lo / st) * st; v <= hi; v += st) out.push(+v.toFixed(6)); return out; };
  let svg = `<svg viewBox="0 0 ${W} ${H}" role="img" aria-label="Decode against prefill">`;
  ticks(y0, y1).forEach((v) => { svg += `<line class="grid" x1="${L}" x2="${W - R}" y1="${Y(v)}" y2="${Y(v)}"/><text x="${L - 6}" y="${Y(v) + 4}" text-anchor="end">${v}</text>`; });
  ticks(x0, x1).forEach((v) => { svg += `<text x="${X(v)}" y="${H - B + 16}" text-anchor="middle">${v}</text>`; });
  svg += `<text x="${(L + W - R) / 2}" y="${H - 4}" text-anchor="middle">Prefill tok/s</text>`;
  svg += `<text x="12" y="${(T + H - B) / 2}" text-anchor="middle" transform="rotate(-90 12 ${(T + H - B) / 2})">Decode tok/s (median)</text>`;
  pts.forEach((p, i) => {
    svg += `<circle cx="${X(p.prefill)}" cy="${Y(p.decode_median)}" r="6" fill="${colorOf(idOf(p))}" fill-opacity=".85"
      stroke="var(--surface)" stroke-width="1.5" data-i="${i}" class="pt"><title>${esc(idOf(p))}</title></circle>`;
  });
  $("scatter").innerHTML = svg + "</svg>";
  $("legend").innerHTML = [...new Set(pts.map(idOf))].map((m) => `<span><span class="swatch" style="background:${colorOf(m)}"></span>${esc(m)}</span>`).join("");
  wireTip(pts);
}

// Hovering a point shows what it was: the model, the engine and build, the
// context and flags it ran with, and every number.
function wireTip(pts) {
  const tip = $("tip");
  document.querySelectorAll("#scatter .pt").forEach((c) => {
    c.onmouseenter = (e) => {
      const p = pts[+c.dataset.i];
      tip.innerHTML =
        `<b>${esc(p.model)}</b>${quantTag(quantOf(p))} <span class="flags">${esc(p.model_file || "")}</span><br>` +
        `${esc(modelLine(p))}<br>` +
        `${esc(p.engine)} <span class="flags">${esc(p.engine_build)}</span>, ${esc(day(p.ts))}<br>` +
        `<b>${fmt(p.decode_median)}</b> t/s decode (best ${fmt(p.decode_best)})` +
        (p.draft_accept != null ? `, MTP ${Math.round(p.draft_accept * 100)}%` : "") + `<br>` +
        `${fmt(p.prefill, 0)} t/s prefill, VRAM ${p.vram_mib ? fmt(p.vram_mib / 1024) : "–"} GB` +
        (p.ram_used_gib != null ? `, RAM ${fmt(p.ram_used_gib)} GB` : "") +
        (p.threads != null ? `, ${p.threads} threads` : "") + `<br>` +
        `<span class="flags">${esc(flagStr(p.flags))}</span>` +
        (p.note ? `<br><i>${esc(p.note)}</i>` : "");
      tip.hidden = false;
      moveTip(e);
    };
    c.onmousemove = moveTip;
    c.onmouseleave = () => { tip.hidden = true; };
  });
}
function moveTip(e) {
  const tip = $("tip"), pad = 14, w = tip.offsetWidth, h = tip.offsetHeight;
  let x = e.clientX + pad, y = e.clientY + pad;
  if (x + w > window.innerWidth - 8) x = e.clientX - w - pad;
  if (y + h > window.innerHeight - 8) y = e.clientY - h - pad;
  tip.style.left = x + "px";
  tip.style.top = y + "px";
}

let sortKey = "ts", sortDir = -1;
const COLS = [
  ["ts", "Time", (r) => esc((r.ts || "").replace("T", " ").slice(0, 16))],
  ["kind", "Kind", (r) => esc(r.kind)],
  ["model", "Model", (r) => `<span class="swatch" style="background:${colorOf(idOf(r))}"></span>${esc(r.model)}${quantTag(quantOf(r))}`],
  ["engine_build", "Build", (r) => esc(`${r.engine} ${r.engine_build}`)],
  ["model_stats", "Params", (r) => paramsCell(r), false],
  ["ctx", "Ctx", (r) => (r.ctx ? (r.ctx / 1024).toFixed(0) + "K" : "–"), true],
  ["threads", "Threads", (r) => (r.threads != null ? r.threads : "–"), true],
  ["decode", "Decode", (r) => fmt(r.kind === "eval" ? r.eval?.decode_best : r.decode_median), true],
  ["prefill", "Prefill", (r) => fmt(r.kind === "eval" ? r.eval?.needle_prefill : r.prefill, 0), true],
  ["vram_mib", "VRAM GB", (r) => (r.vram_mib ? fmt(r.vram_mib / 1024) : "–"), true],
  ["ram_used_gib", "RAM GB", (r) => (r.ram_used_gib != null ? fmt(r.ram_used_gib) : "–"), true],
  ["result", "Eval", (r) => (r.kind === "eval" ? `<span class="badge ${verdict(r.eval?.needle)}">${r.eval?.needle === "PASS" ? "Pass" : r.eval?.needle === "FAIL" ? "Fail" : "–"}</span>` : "")
    + (agentOf(r) ? ` <span class="badge ${agentOf(r).pass ? "PASS" : "FAIL"}" title="${esc(agentOf(r).apis)}">Agent ${agentOf(r).solved}/${agentOf(r).total}</span>` : "")],
  ["flags", "Flags", (r) => `<span class="flags">${esc(flagStr(r.flags))}</span>`],
  ["note", "Note", (r) => `<span class="note" title="${esc(r.note)}">${esc(r.note)}</span>`],
];
const sortVal = (r, k) => k === "decode" ? (r.kind === "eval" ? r.eval?.decode_best : r.decode_median) ?? -1
  : k === "prefill" ? (r.kind === "eval" ? r.eval?.needle_prefill : r.prefill) ?? -1
  : k === "model" ? idOf(r) : k === "flags" ? flagStr(r.flags) : k === "model_stats" ? (r.params_b ?? -1) : k === "result" ? r.eval?.needle || "" : r[k] ?? "";

function renderRuns(rows) {
  const sorted = rows.slice().sort((a, b) => { const x = sortVal(a, sortKey), y = sortVal(b, sortKey); return (x > y ? 1 : x < y ? -1 : 0) * sortDir; });
  const order = (k) => (k === sortKey ? (sortDir > 0 ? "ascending" : "descending") : "none");
  $("runs").innerHTML = `<thead><tr>${COLS.map(([k, label, , num]) => `<th data-k="${k}" class="${num ? "num" : ""}" tabindex="0" aria-sort="${order(k)}">${label}${k === sortKey ? (sortDir > 0 ? " ▲" : " ▼") : ""}</th>`).join("")}</tr></thead><tbody>` +
    (sorted.length ? sorted.map((r) => `<tr class="${r.quiet === false ? "suspect" : ""}">${COLS.map(([, , f, num]) => `<td class="${num ? "num" : ""}">${f(r)}</td>`).join("")}</tr>`).join("")
      : `<tr><td class="empty" colspan="${COLS.length}">No runs match the filters.</td></tr>`) + "</tbody>";
  $("runs").querySelectorAll("th").forEach((th) => {
    const sort = () => { const k = th.dataset.k; sortDir = k === sortKey ? -sortDir : -1; sortKey = k; renderRuns(filtered()); $("runs").querySelector(`th[data-k="${k}"]`)?.focus(); };
    th.onclick = sort;
    th.onkeydown = (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); sort(); } };
  });
}

function render() {
  const rows = filtered();
  const last = ROWS.map((r) => r.ts).sort().pop();
  $("meta").textContent = `${ROWS.length} recorded, the last ${last ? last.replace("T", " ").slice(0, 16) + " UTC" : "never"}`;
  renderBoard(rows); renderScatter(rows); renderRuns(rows);
}
["f-host", "f-engine", "f-model", "f-class", "f-class-chart", "f-quiet"].forEach((id) => $(id).addEventListener("change", render));
render();

// ---- the console (served by golem service) ------------------------------
// results.jsonl is the single source of truth: poll it and the state. A
// snapshot page (file://) answers neither, keeps its data and shows Results.

// A model's best measured decode on this machine: the fastest bench row for
// it, with that row's prefill.
function bestRun(name, quant) {
  const runs = ROWS.filter((r) => r.kind === "bench" && r.model === name && r.decode_median && (!topHost || r.host === topHost)
    && (!quant || quantOf(r) === quant));
  return runs.reduce((a, b) => (!a || b.decode_median > a.decode_median ? b : a), null);
}

// What a model's numbers are worth on this machine, from its newest gate
// (an eval with a needle) and its benches: proven, failed, measured, or
// nothing at all, on this machine (`topHost`).
function evidence(name, quant) {
  const mine = ROWS.filter((r) => r.model === name && (!topHost || r.host === topHost) && (!quant || quantOf(r) === quant));
  const gate = mine.filter((r) => r.kind === "eval" && r.eval?.needle).sort((a, b) => (a.ts < b.ts ? 1 : -1))[0];
  const build = (r) => (r.engine_build || "").split("-")[0];
  const size = (r) => (r.eval?.needle_tokens ? `${Math.round(r.eval.needle_tokens / 1000)}K` : "long-context");
  if (gate?.eval.needle === "PASS")
    return { tier: "proven", text: `<b>Proven</b> on this machine: ${size(gate)} needle passed on ${esc(build(gate))}, ${esc(day(gate.ts))}` };
  if (gate) return { tier: "failed", text: `<b>Failed the gate</b>: ${size(gate)} needle on ${esc(build(gate))}, ${esc(day(gate.ts))}` };
  if (mine.some((r) => r.kind === "bench")) return { tier: "measured", text: `<b>Measured</b>, not yet through the 200K gate` };
  return { tier: "none", text: "Not measured on this machine" };
}
const sealHtml = (ev) => SEALS[ev.tier] + `<span>${ev.text}</span>`;

// The agent suite on a row - `eval --agent`'s own, or a gate that ran it -
// summed over its APIs, and whether each API cleared the gate's floor
// (TUNE_MIN_AGENT's default).
function agentOf(r) {
  const runs = Object.entries(r?.agent || {});
  if (!runs.length) return null;
  const solved = runs.reduce((s, [, a]) => s + (a.solved || 0), 0), total = runs.reduce((s, [, a]) => s + (a.total || 0), 0);
  const pass = runs.every(([, a]) => a.total && a.solved >= AGENT_FLOOR * a.total);
  return { solved, total, pass, apis: runs.map(([k, a]) => `${API_NAMES[k] || k} ${a.solved}/${a.total}`).join(", ") };
}
// A model's newest agent verdict on this machine, as a seal, or null.
function agentEvidence(name, quant) {
  const r = ROWS.filter((x) => x.model === name && (!topHost || x.host === topHost) && (!quant || quantOf(x) === quant) && agentOf(x))
    .sort((a, b) => (a.ts < b.ts ? 1 : -1))[0];
  const a = agentOf(r);
  if (!a) return null;
  return { tier: a.pass ? "proven" : "failed", text: `<b>Agent work</b>: ${a.solved} of ${a.total} tasks solved (${esc(a.apis)}), ${esc(day(r.ts))}` };
}

// The strip on top of Models: what serves (or would), what it is worth, how
// fast it ran here.
function renderServing(models, state) {
  const name = state?.alias || "";
  const m = models.find((x) => x.Name === name);
  const quant = m?.Quant || "";
  SERVED_QUANT = quant;
  const run = bestRun(name, quant);
  const busy = state?.job_running;
  $("sv-lead").textContent = busy ? `${state.job_action} running` : state?.up ? "Serving" : "Stopped";
  $("sv-lead").className = "lead" + (state?.up && !busy ? " up" : "");
  $("sv-name").innerHTML = name ? esc(name) + quantTag(quant) : "No model yet";
  const ev = evidence(name, quant);
  $("sv-seal").className = "seal " + ev.tier;
  $("sv-seal").innerHTML = name ? sealHtml(ev) : "";
  const ag = name ? agentEvidence(name, quant) : null;
  $("sv-agent").hidden = !ag;
  $("sv-agent").className = "seal " + (ag?.tier || "");
  $("sv-agent").innerHTML = ag ? sealHtml(ag) : "";
  const cell = (label, value, unit) => `<div><dt>${label}</dt><dd>${value}${unit ? `<small>${unit}</small>` : ""}</dd></div>`;
  $("sv-numbers").innerHTML = (run ? cell("Decode", fmt(run.decode_median), "t/s") + cell("Prefill", fmt(run.prefill, 0), "t/s") : "")
    + (m?.Ctx ? cell("Context", Math.round(m.Ctx / 1024), "K") : "");
}

// ---- connecting an agent ------------------------------------------------
// The exact setup for each client, from the host, port and key the service
// answers with: the Anthropic API at the root, the OpenAI APIs under /v1.
let CX = "claude", CX_SHOW = false, CX_STATE = null;
// The default model's quant, from the Models list: Connect names it, since a
// client's results belong to that file and not only to the preset's name.
let SERVED_QUANT = "";
try { CX = localStorage.getItem("golem.connect") || "claude"; } catch (e) { /* no storage */ }
const WINDOWS = /Windows/.test(navigator.userAgent);
function connectSetup(chat, state, key) {
  const wild = chat.host === "0.0.0.0" || chat.host === "::";
  const host = wild ? location.hostname : chat.host;
  const root = `http://${host}:${chat.port}`, v1 = `${root}/v1`;
  const alias = state?.alias || "model";
  const models = (state?.presets || [alias]);
  const env = (name, value) => (WINDOWS ? `$env:${name} = "${value}"` : `export ${name}="${value}"`);
  const setups = {
    claude: {
      text: [env("ANTHROPIC_BASE_URL", root), env("ANTHROPIC_AUTH_TOKEN", key), env("ANTHROPIC_MODEL", alias),
        env("ANTHROPIC_SMALL_FAST_MODEL", alias), "claude"].join("\n"),
      note: `${WINDOWS ? "PowerShell" : "A shell"}, then claude. On this machine, golem claude does the same.`,
    },
    opencode: {
      text: JSON.stringify({ $schema: "https://opencode.ai/config.json", provider: { golem: { npm: "@ai-sdk/openai-compatible", name: "Golem",
        options: { baseURL: v1, apiKey: key }, models: Object.fromEntries(models.map((m) => [m, {}])) } }, model: `golem/${alias}` }, null, 2),
      note: "Into opencode.json (in ~/.config/opencode). The base URL ends in /v1.",
    },
    codex: {
      text: `# ~/.codex/config.toml\nmodel = "${alias}"\nmodel_provider = "golem"\n\n[model_providers.golem]\nname = "Golem"\nbase_url = "${v1}"\n`
        + `env_key = "GOLEM_API_KEY"\nwire_api = "responses"\n\n# and in the shell that runs codex:\n${env("GOLEM_API_KEY", key)}`,
      note: "Codex speaks the Responses API, which golem serves under /v1.",
    },
    other: {
      text: `OpenAI-compatible\n  base URL  ${v1}\n  API key   ${key}\n  model     ${alias}\n\n`
        + `Anthropic-compatible\n  base URL  ${root}\n  API key   ${key}  (as a bearer token or x-api-key)\n\nModels: ${models.join(", ")}`,
      note: "Most clients add /chat/completions or /messages to the base URL themselves.",
    },
  };
  return { root, v1, local: !wild && (chat.host === "127.0.0.1" || chat.host === "localhost"), ...setups[CX] };
}
function renderConnect(state) {
  if (state) CX_STATE = state;
  if (!CHAT) return;
  const masked = "•".repeat(12);
  const shown = connectSetup(CHAT, CX_STATE, CX_SHOW ? CHAT.key : masked);
  $("cx-anthropic").textContent = shown.root;
  $("cx-openai").textContent = shown.v1;
  $("cx-key").textContent = CX_SHOW ? CHAT.key : masked;
  $("cx-snippet").textContent = shown.text;
  $("cx-model").innerHTML = CX_STATE?.alias ? esc(CX_STATE.alias) + quantTag(SERVED_QUANT) : "–";
  $("cx-note").textContent = shown.note + (shown.local && location.hostname !== CHAT.host && location.hostname !== "localhost"
    ? ` Golem's API listens on ${CHAT.host} only, so agents on other machines cannot reach it (LOCAL_AI_HOST in golem.toml).` : "");
  $("cx-show").textContent = CX_SHOW ? "Hide key" : "Show key";
  document.querySelectorAll("#cx-tabs button").forEach((b) => b.setAttribute("aria-selected", String(b.dataset.cx === CX)));
  const web = CX_STATE?.web;
  $("cx-web").innerHTML = web === false
    ? `Golem's web search and page fetch are off (WEB_SEARCH = "off" in golem.toml).`
    : `<span class="web-on">Web search and page fetch come with every request</span> through this port, beside the agent's own tools. They run from this machine: searches go to DuckDuckGo with no account, and every call is a line in logs/web.log.`;
}
document.querySelectorAll("#cx-tabs button").forEach((b) => b.onclick = () => {
  CX = b.dataset.cx;
  try { localStorage.setItem("golem.connect", CX); } catch (e) { /* no storage */ }
  renderConnect();
});
$("cx-show").onclick = () => { CX_SHOW = !CX_SHOW; renderConnect(); };
$("cx-copy").onclick = () => CHAT && copyText(connectSetup(CHAT, CX_STATE, CHAT.key).text, $("cx-copy"), "Copy setup");
$("cx-key-copy").onclick = () => CHAT && copyText(CHAT.key, $("cx-key-copy"), "Copy");
document.querySelectorAll("[data-copy]").forEach((b) => b.onclick = () => copyText($(b.dataset.copy).textContent, b, "Copy"));
async function loadChat() {
  if (CHAT) return CHAT;
  try { CHAT = await (await fetch("api/chat", { cache: "no-store" })).json(); } catch (e) { return null; }
  return CHAT;
}

const busyNote = "The console is not answering. Is golem service running?";

// What an action answered, said under the engine buttons: the service's own
// words for a refusal ("a job is already running"), a short one for success.
const ANSWERS = { applied: "Done. It takes effect at the next start.", added: "Preset added.",
  "bad-input": "Refused: the default model stays, and only what this machine has can be chosen.",
  deleted: "Model file deleted.", removed: "Preset removed." };
// The service's answer in words: a JSON `result` code through ANSWERS, or
// the plain text it sent.
function answerWords(text) {
  try { const r = JSON.parse(text).result; return ANSWERS[r] || r; } catch (e) { return text; }
}
async function say(response) {
  const words = answerWords((await response.text()).trim());
  const msg = $("c-msg");
  msg.textContent = words;
  msg.className = "msg" + (response.ok ? "" : " bad");
  msg.hidden = !words;
}

// The list is rebuilt only when what it shows has changed, so a poll does not
// take the focus from a button or a choice from a menu.
let modelsShown = "";
function renderModels(models, state) {
  const running = !!state?.job_running;
  const serving = state?.up ? state.alias : null;
  const shown = JSON.stringify([models, running, serving, ROWS.length]);
  if (shown === modelsShown) return;
  modelsShown = shown;
  $("m-list").innerHTML = models.length ? models.map((m) => {
    const run = bestRun(m.Name, m.Quant);
    const ev = evidence(m.Name, m.Quant), ag = agentEvidence(m.Name, m.Quant);
    const chips = [
      m.Role ? `<span class="chip role">${esc(m.Role)}</span>` : "",
      m.Default ? `<span class="chip">default</span>` : "",
      m.Moe ? `<span class="chip">MoE ${esc(m.Experts)}</span>` : "",
      m.Mtp ? `<span class="chip">MTP</span>` : "",
      m.Present ? "" : `<span class="chip away">not downloaded</span>`,
    ].join("");
    const facts = [
      m.ParamsB ? `<span>${fmt(m.ParamsB, 1)}B${m.Moe && m.ParamsActiveB ? `, ${fmt(m.ParamsActiveB, 1)}B active` : ""}</span>` : "",
      m.Gb ? `<span>${fmt(m.Gb)} GB</span>` : "",
      m.Ctx ? `<span>${Math.round(m.Ctx / 1024)}K context</span>` : "",
      `<span class="code">${esc(m.Repo || m.File)}</span>`,
    ].join("");
    const acts = [
      m.Present
        ? (m.Default ? "" : `<button class="btn" data-alias="${esc(m.Name)}">Make default</button>`)
        : `<button class="btn" data-pull="${esc(m.Name)}">Download</button>`,
      m.Present ? `<button class="btn" data-tune="${esc(m.Name)}" title="Find the split and context for this machine, gate it, and serve it">Tune</button>` : "",
      m.Present ? `<button class="btn quiet danger" data-delmodel="${esc(m.Name)}">Delete file</button>` : "",
      m.Default ? "" : `<button class="btn quiet danger" data-delpreset="${esc(m.Name)}">Remove</button>`,
    ].join("");
    return `<li class="model${m.Name === serving ? " serving" : ""}">
      <div class="name"><strong>${esc(m.Name)}</strong>${quantTag(m.Quant)}${chips}</div>
      <div class="facts">${facts}</div>
      ${m.Present ? `<div class="seals"><div class="seal ${ev.tier}">${sealHtml(ev)}</div>${
        ag ? `<div class="seal ${ag.tier}">${sealHtml(ag)}</div>` : ""}</div>` : ""}
      <div class="speed${run ? "" : " none"}">${run ? `<b>${fmt(run.decode_median)}<small>t/s</small></b><span>${fmt(run.prefill, 0)} prefill</span>` : `<b>–</b><span>not measured yet</span>`}</div>
      <div class="acts">${acts}</div></li>`;
  }).join("") : `<li class="empty">No models yet. Pull one above.</li>`;
  $("m-list").querySelectorAll("button").forEach((b) => { b.disabled = running; });

  // A refused or failed action gives its button back: the list repaints
  // only when what it shows changed, and a refusal changes nothing.
  const act = (sel, fn) => $("m-list").querySelectorAll(sel).forEach((b) => b.onclick = async () => {
    b.disabled = true;
    try {
      const r = await fn(b);
      if (r) await say(r);
      if (!r || !r.ok) b.disabled = false;
    } catch (e) { $("pull-msg").textContent = busyNote; b.disabled = false; }
    setTimeout(pollLive, 1200);
  });
  act("[data-alias]", (b) => post("api/run", `action=alias&arg=${encodeURIComponent(b.dataset.alias)}`));
  act("[data-pull]", (b) => post("api/run", `action=pull&arg=${encodeURIComponent(b.dataset.pull)}`));
  act("[data-tune]", (b) => post("api/run", `action=tune&arg=${encodeURIComponent(b.dataset.tune)}`));
  // Deleting asks first: the file can be downloaded again, the preset has a
  // .bak beside it.
  act("[data-delmodel]", (b) => confirm(`Delete the model file of ${b.dataset.delmodel}?\n\nIt can be downloaded again later.`)
    ? post("api/delmodel", `arg=${encodeURIComponent(b.dataset.delmodel)}`) : null);
  act("[data-delpreset]", (b) => confirm(`Remove ${b.dataset.delpreset} from the presets?\n\nThe model file stays on disk.`)
    ? post("api/delpreset", `arg=${encodeURIComponent(b.dataset.delpreset)}`) : null);
}

function renderState(s) {
  if (s.version) $("c-version").textContent = "v" + s.version;
  setThisHost(s.host);
  const busy = s.job_running;
  jobRunning = !!busy;
  $("c-state").className = "state " + (busy ? "busy" : s.up ? "up" : "");
  const p = busy ? s.job_progress : null;
  $("c-state-text").innerHTML = busy ? `<b>${esc(s.job_action)}</b> running${p ? `, ${p.percent}%` : ""}`
    : s.up && s.cpu_only ? `Serving <b>${esc(s.alias)}</b> <span class="cpu-only" title="${esc(s.cpu_only)}">on the CPU only - no GPU device</span>`
    : s.up ? `Serving <b>${esc(s.alias)}</b>${s.vram ? `<span class="extra">, ${(s.vram / 1024).toFixed(1)} GB VRAM</span>` : ""}${s.device ? `<span class="extra"> on ${esc(s.device.replace(/^\S+\s+/, ""))}</span>` : ""}`
    : `Stopped, default <b>${esc(s.alias || "none")}</b>`;
  drawMark(busy ? "busy" : s.up ? "up" : "down");
  // Rebuilt only when the list or the setting changed: a choice not yet
  // applied with "Use" survives the poll.
  const engines = JSON.stringify([s.engines, s.engine]);
  if ($("c-engine").dataset.shown !== engines) {
    $("c-engine").dataset.shown = engines;
    $("c-engine").innerHTML = (s.engines || []).map((v) => `<option${v === s.engine ? " selected" : ""}>${esc(v)}</option>`).join("");
  }
  const bar = $("c-progress");
  bar.hidden = !p;
  if (p) {
    $("c-progress-label").textContent = p.label;
    $("c-progress-amount").textContent = `${p.amount} · ${p.percent}%`;
    $("c-progress-fill").style.width = `${p.percent}%`;
    bar.setAttribute("aria-valuenow", p.percent);
    bar.setAttribute("aria-label", p.label);
  }
  const box = $("c-log-box");
  box.hidden = !s.job_tail;
  if (s.job_tail) {
    renderJobLog($("c-log"), s.job_tail);
    // A job that starts opens the log; one that ended stays as the reader
    // left it, and a page opened after it ended shows only its last line.
    if (busy && !jobWasRunning) box.open = true;
    const lines = s.job_tail.split("\n").map((l) => l.trim()).filter(Boolean);
    const last = (lines[lines.length - 1] || "").replace(/^(\d\d:\d\d):\d\d /, "$1 ").replace(/=== | ===$/g, "");
    $("c-log-sum").innerHTML = busy ? `<b>${esc(s.job_action)}</b> running` : `Last job: ${esc(last)}`;
  }
  jobWasRunning = !!busy;
  renderUpdate(s.update);
  document.querySelectorAll("#control .btn, #pull-form .btn, #update-go").forEach((b) => { b.disabled = !!busy; });
}

// The update banner: a release on offer, or one installed and waiting for a
// restart. Hidden the rest of the time - api/state says `update: null`.
let updateShown = "";
function renderUpdate(u) {
  const shown = JSON.stringify(u) || "";
  if (shown === updateShown) return;
  updateShown = shown;
  const box = $("update");
  box.hidden = !u;
  if (!u) return;
  $("update-text").innerHTML = u.installed
    ? `Golem <b>${esc(u.latest)}</b> is installed. Restart Golem to use it.`
    : `Golem <b>${esc(u.latest)}</b> is available (you have ${esc(u.current)}).`;
  $("update-link").hidden = !u.url;
  if (u.url) $("update-link").href = u.url;
  $("update-go").hidden = u.installed;
  $("update-skip").hidden = u.installed;
}

// The click is the consent: the job runs `golem update --yes`, its progress
// lands in the job pane, and the Models tab is where that pane lives. A
// refused click (another job is running) leaves the banner up: the poll
// repaints it, and the offer is not lost.
// A refusal is said in the banner itself, until the next click.
$("update-go").onclick = async () => {
  updateShown = "";
  const note = $("update-msg");
  note.hidden = true;
  try {
    const r = await post("api/run", "action=update");
    if (r.ok) { $("update").hidden = true; showTab("models"); }
    else { note.textContent = answerWords((await r.text()).trim()) || `Refused (HTTP ${r.status})`; note.hidden = false; }
  } catch (e) { note.textContent = "The service did not answer."; note.hidden = false; }
  setTimeout(pollLive, 1200);
};
$("update-skip").onclick = async () => {
  updateShown = "";
  try {
    const r = await post("api/run", "action=skip-update");
    if (r.ok) $("update").hidden = true;
  } catch (e) { /* nothing to skip */ }
};

// The job pane, one div per line: the clock stamp dimmed, the === strips
// bold, a verdict tag coloured. Rebuilt only when the text changed - the
// page polls several times a second - and the reader's scroll position
// stays unless they were already at the bottom, where new lines follow.
let jobLogShown = null;
function renderJobLog(pre, text) {
  if (text === jobLogShown) return;
  jobLogShown = text;
  const atEnd = pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 4;
  pre.innerHTML = text.split("\n").map((line) => {
    const stamp = line.match(/^\d\d:\d\d:\d\d /)?.[0] || "";
    const body = line.slice(stamp.length);
    const cls = /^=== .* ===$/.test(body) ? " jf"
      : body.startsWith("[ OK ]") ? " ok"
      : body.startsWith("[WARN]") ? " warn"
      : body.startsWith("[FAIL]") ? " bad" : "";
    const rest = cls === " jf" ? body.replace(/^=== | ===$/g, "") : body;
    return `<div class="jl${cls}">${stamp ? `<span class="jt">${esc(stamp)}</span>` : ""}${esc(rest)}</div>`;
  }).join("");
  if (atEnd) pre.scrollTop = pre.scrollHeight;
}

async function pollOnce() {
  let live = false;
  try {
    const r = await fetch("results.jsonl", { cache: "no-store" });
    if (r.ok) {
      const rows = parseRows(await r.text());
      if (rows.length !== ROWS.length || (rows.length && rows[rows.length - 1].ts !== ROWS[ROWS.length - 1]?.ts)) {
        ROWS = rows;
        fillFilters();
        render();
      }
      live = true;
    }
  } catch (e) { live = false; }
  if (live !== LIVE) {
    LIVE = live;
    document.querySelectorAll('.tabs a[data-tab="models"], .tabs a[data-tab="chat"], .tabs a[data-tab="log"], .tabs a[data-tab="settings"]').forEach((a) => { a.hidden = !live; });
    showTab(location.hash.slice(1) || (live ? "models" : "results"));
  }
  if (!live) return;
  let state = null;
  try { state = await (await fetch("api/state", { cache: "no-store" })).json(); renderState(state); }
  catch (e) { $("c-state-text").textContent = "No state"; }
  ENGINE_UP = !!state?.up;
  if (!$("tab-chat").hidden) showChat();
  try {
    const models = await (await fetch("api/models", { cache: "no-store" })).json();
    renderServing(models, state);
    renderModels(models, state);
  }
  catch (e) {
    if (!$("m-list").children.length) $("m-list").innerHTML = `<li class="empty">The model list is not available from this service.</li>`;
  }
  // After the strip, which reads the default model's quant for Connect.
  if (await loadChat()) renderConnect(state);
  try {
    const l = await (await fetch("api/logs", { cache: "no-store" })).json();
    $("log-file").textContent = l.file ? l.file : "No engine log yet. It starts with the engine.";
    renderEngineLog($("engine-log"), l);
  } catch (e) { /* no log */ }
}

// The engine pane: llama.cpp's lines as the service read them (a model tag,
// loads, request speeds, failures coloured like the job log), or its own
// text with Raw on. Rebuilt only when what it shows changed, and it follows
// new lines only when the reader was already at the bottom.
let ENGINE_RAW = false;
try { ENGINE_RAW = localStorage.getItem("golem.engineRaw") === "1"; } catch (e) { /* no storage */ }
let engineLogShown = null;
function renderEngineLog(pre, l) {
  const raw = ENGINE_RAW || !Array.isArray(l.lines);
  const key = (raw ? "r" : "l") + (l.text || "");
  if (key === engineLogShown) return;
  engineLogShown = key;
  const atEnd = pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 4;
  pre.classList.toggle("raw", raw);
  if (raw) {
    pre.textContent = l.text || "";
  } else {
    pre.innerHTML = l.lines.map((line) =>
      // The space after the tag is a real one, so a copied line reads
      // "router  listening on ...", not "routerlistening".
      `<div class="jl${line.k ? " " + line.k : ""}"><span class="em" title="${esc(line.m)}">${esc(line.m || "router")}</span> ${esc(line.t)}</div>`
    ).join("");
  }
  if (atEnd) pre.scrollTop = pre.scrollHeight;
}
$("log-raw").onclick = () => {
  ENGINE_RAW = !ENGINE_RAW;
  $("log-raw").setAttribute("aria-pressed", String(ENGINE_RAW));
  $("log-raw").classList.toggle("go", ENGINE_RAW);
  try { localStorage.setItem("golem.engineRaw", ENGINE_RAW ? "1" : "0"); } catch (e) { /* no storage */ }
  engineLogShown = null;
  pollLive();
};
$("log-raw").setAttribute("aria-pressed", String(ENGINE_RAW));
$("log-raw").classList.toggle("go", ENGINE_RAW);

// ---- settings -----------------------------------------------------------
// The golem.toml keys the service offers (api/settings), one row each,
// saved one at a time. Read when the tab opens, never by the poll: a poll
// would overwrite what someone is typing.
const APPLIES = {
  "now": "applies now",
  "next-run": "next bench, eval or tune",
  "engine-restart": "on the engine's next start",
  "service-restart": "when the service restarts",
};
async function loadSettings() {
  const box = $("set-groups");
  let list;
  try { list = await (await fetch("api/settings", { cache: "no-store" })).json(); }
  catch (e) { box.innerHTML = `<div class="surface empty">This service has no settings to offer: update Golem.</div>`; return; }
  const groups = [];
  list.forEach((s) => { let g = groups.find((x) => x.name === s.group); if (!g) groups.push(g = { name: s.group, items: [] }); g.items.push(s); });
  box.innerHTML = groups.map((g) => `<h2>${esc(g.name)}</h2><div class="surface settings">${g.items.map(settingRow).join("")}</div>`).join("");
  box.querySelectorAll("form.set-row").forEach((f) => f.onsubmit = (e) => { e.preventDefault(); saveSetting(f); });
}
function settingRow(s) {
  const id = "set-" + s.key;
  const placeholder = s.default ? `default ${s.default}` : "unset";
  // The service lists only the choices this machine can use; a value the
  // file already holds outside them is shown as what it is, and cannot be
  // picked again.
  const stale = s.shape === "choice" && s.value && !s.choices.includes(s.value)
    ? `<option selected disabled>${esc(s.value)} - not on this machine</option>` : "";
  const field = s.shape === "choice"
    ? `<select id="${id}" name="value">${stale}${s.choices.map((c) => `<option${!stale && c === (s.value || s.default) ? " selected" : ""}>${esc(c)}</option>`).join("")}</select>`
    : s.shape === "secret"
      ? `<input type="password" id="${id}" name="value" autocomplete="off" placeholder="${s.set ? "set - type to replace" : "not set"}">`
      : `<input type="${s.shape === "whole" || s.shape === "decimal" || s.shape === "port" ? "number" : "text"}" id="${id}" name="value"
          ${s.min != null ? `min="${s.min}" max="${s.max}"` : ""} ${s.shape === "decimal" ? `step="any"` : ""}
          value="${esc(s.value)}" placeholder="${esc(placeholder)}" spellcheck="false" autocomplete="off">`;
  return `<form class="set-row" data-key="${esc(s.key)}">
    <label for="${id}">${esc(s.label)} <span class="flags">${esc(s.key)}</span></label>
    <span class="help">${esc(s.help)} <span class="chip">${esc(APPLIES[s.applies] || s.applies)}</span></span>
    <div class="field">${field}</div>
    <div class="acts"><button class="btn" type="submit">Save</button></div>
    <small class="note" aria-live="polite"></small></form>`;
}
async function saveSetting(form) {
  const key = form.dataset.key, input = form.querySelector("[name=value]"), note = form.querySelector(".note"), btn = form.querySelector("button");
  // An empty secret field is one nobody typed in, not a request to clear it.
  if (input.type === "password" && !input.value) { note.className = "note bad"; note.textContent = "Type the token to save it."; return; }
  btn.disabled = true;
  note.className = "note"; note.textContent = "Saving...";
  try {
    const r = await post("api/settings", `key=${encodeURIComponent(key)}&value=${encodeURIComponent(input.value)}`);
    if (r.ok) {
      const a = await r.json();
      note.className = "note ok"; note.textContent = `Saved - ${APPLIES[a.applies] || a.applies}.`;
      if (input.type === "password") { input.value = ""; input.placeholder = "set - type to replace"; }
    } else { note.className = "note bad"; note.textContent = (await r.text()).trim() || `Refused (HTTP ${r.status})`; }
  } catch (e) { note.className = "note bad"; note.textContent = "The service did not answer."; }
  btn.disabled = false;
}

// ---- chat ---------------------------------------------------------------
// The Chat tab frames the engine's own chat page (llama-server's, on the
// engine's port). That page keeps its API key in its own origin's storage,
// which this page cannot write, so the title bar copies it to paste once.
let ENGINE_UP = false;
let CHAT = null;        // { host, port, key } from api/chat
let chatShown = "";     // the URL the frame was loaded with, while it is up
async function showChat() {
  if (!(await loadChat())) return;
  // A wildcard bind is reached the way this page was; anything else by name.
  const wild = CHAT.host === "0.0.0.0" || CHAT.host === "::";
  const url = `http://${wild ? location.hostname : CHAT.host}:${CHAT.port}/`;
  $("chat-open").href = url;
  const frame = $("chat-frame"), msg = $("chat-msg");
  frame.hidden = !ENGINE_UP;
  msg.hidden = ENGINE_UP;
  if (!ENGINE_UP) {
    msg.textContent = "The chat opens when a model is serving. Start one from Models.";
    chatShown = "";
    frame.removeAttribute("src");
  } else if (chatShown !== url) {
    // Loaded once per start: the poll must not reload a conversation.
    frame.src = url;
    chatShown = url;
  }
}
// Copy text, and say so on the button. The async clipboard exists only on a
// secure origin - a phone reaching the console over the LAN has plain http -
// so the fallback copies from a textarea made for it.
async function copyText(text, button, label) {
  let copied = false;
  try { await navigator.clipboard.writeText(text); copied = true; } catch (e) {
    const area = document.createElement("textarea");
    area.value = text;
    area.setAttribute("readonly", "");
    area.style.cssText = "position:fixed;top:0;left:0;opacity:0";
    document.body.appendChild(area);
    area.select();
    try { copied = document.execCommand("copy"); } catch (err) { copied = false; }
    area.remove();
  }
  button.textContent = copied ? "Copied" : "Copy failed";
  setTimeout(() => { button.textContent = label; }, 2000);
}
$("chat-copy").onclick = async () => { if (await loadChat()) copyText(CHAT.key || "", $("chat-copy"), "Copy API key"); };

document.querySelectorAll("#control .btn").forEach((b) => b.onclick = async () => {
  b.disabled = true;
  const apply = b.dataset.apply;
  const body = apply ? `action=engine&arg=${encodeURIComponent($(apply).value)}` : `action=${b.dataset.run}`;
  try { await say(await post("api/run", body)); } catch (e) { $("pull-msg").textContent = busyNote; }
  setTimeout(pollLive, 1200);
});
$("pull-form").onsubmit = async (e) => {
  e.preventDefault();
  const ref = $("pull-ref").value.trim();
  if (!ref) { $("pull-msg").textContent = "Type owner/repo, owner/repo:quant or a library name."; return; }
  $("pull-msg").textContent = `Pulling ${ref}. Its progress shows under the engine buttons.`;
  try {
    const r = await post("api/run", `action=pull&arg=${encodeURIComponent(ref)}`);
    if (!r.ok) $("pull-msg").textContent = await r.text();
    else $("pull-ref").value = "";
  } catch (err) { $("pull-msg").textContent = busyNote; }
  setTimeout(pollLive, 1200);
};

showTab(location.hash.slice(1) || "results");
// Every 5 s, and every 1.5 s while a job runs so its bar moves; never two
// polls at once. A poll asked for while one runs (a button's follow-up)
// runs once more right after it, so answers are applied in order.
let polling = false, pollAgain = false, jobRunning = false, jobWasRunning = false;
async function pollLive() {
  if (polling) { pollAgain = true; return; }
  polling = true;
  try {
    do { pollAgain = false; await pollOnce(); } while (pollAgain);
  } finally { polling = false; }
}
async function tick() {
  await pollLive();
  setTimeout(tick, jobRunning ? 1500 : 5000);
}
tick();
