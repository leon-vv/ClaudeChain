"use strict";
// Resources page: plan usage, token usage, running agents and system load.
// Uses the helpers and state from app.js (h, api, S, fmtTok, ...).

const R = { sys: null, timer: null, built: false };

function fmtBytes(n) {
  if (n == null) return "";
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n.toFixed(n < 10 && i > 0 ? 1 : 0)} ${u[i]}`;
}

function fmtSecs(s) {
  if (s < 60) return `${Math.round(s)}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m${String(Math.round(s % 60)).padStart(2, "0")}s`;
  if (s < 86400) return `${Math.floor(s / 3600)}h${String(Math.floor((s % 3600) / 60)).padStart(2, "0")}m`;
  return `${Math.floor(s / 86400)}d ${Math.floor((s % 86400) / 3600)}h`;
}

const runSecs = (r) => ((r.ended ? new Date(r.ended) : new Date()) - new Date(r.started)) / 1000;

function meter(pct, extraClass = "") {
  const p = Math.max(0, Math.min(100, pct || 0));
  return h("span", { class: `meter ${extraClass}` }, h("span", { class: `fill ${levelClass(p)}`, style: `width:${p}%` }));
}

/// Small line chart of `values` scaled to `max`.
function spark(values, max, cls = "") {
  const w = 240, ht = 44;
  if (values.length < 2) return h("div", { class: "spark-empty" }, "collecting…");
  const pts = values.map((v, i) => `${((i / (values.length - 1)) * w).toFixed(1)},${(ht - 2 - (Math.min(v, max) / max) * (ht - 4)).toFixed(1)}`);
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", `0 0 ${w} ${ht}`);
  svg.setAttribute("preserveAspectRatio", "none");
  svg.setAttribute("class", `spark ${cls}`);
  svg.innerHTML = `<polygon points="0,${ht} ${pts.join(" ")} ${w},${ht}" class="area"/><polyline points="${pts.join(" ")}" class="line"/>`;
  return svg;
}

function card(title, ...body) {
  return h("section", { class: "card" }, h("h2", {}, title), ...body);
}

function stat(label, value, sub) {
  return h("div", { class: "stat" }, h("div", { class: "stat-label" }, label), h("div", { class: "stat-value" }, value), sub ? h("div", { class: "stat-sub" }, sub) : "");
}

function table(headers, rows, numericFrom = 1) {
  return h("table", { class: "data" },
    h("thead", {}, h("tr", {}, headers.map((t, i) => h("th", { class: i >= numericFrom ? "num" : "" }, t)))),
    h("tbody", {}, rows.length ? rows.map((r) => h("tr", {}, r.map((c, i) => h("td", { class: i >= numericFrom ? "num" : "" }, c))))
      : h("tr", {}, h("td", { colspan: headers.length, class: "muted" }, "nothing yet"))));
}

// ---- Lifecycle -----------------------------------------------------------

function startResources() {
  buildResources();
  pollResources();
  clearInterval(R.timer);
  R.timer = setInterval(pollResources, 2000);
}

function stopResources() {
  clearInterval(R.timer);
  R.timer = null;
}

async function pollResources() {
  try {
    R.sys = await api("GET", "/api/resources");
  } catch {
    return;
  }
  if (S.page === "resources") {
    renderSystem();
    renderAgents();
  }
}

/// Builds the page skeleton once; sections re-render independently so the
/// pause-threshold input keeps focus while typing.
function buildResources() {
  if (R.built) return renderResources();
  R.built = true;
  const pause = h("input", { type: "number", min: 1, max: 100, step: 1, id: "pause-above", placeholder: "off" });
  pause.onchange = () => act(async () => {
    const v = parseFloat(pause.value);
    S.account = await api("PUT", "/api/limits", { pause_above: Number.isFinite(v) && v > 0 ? v : null });
    renderPlan();
    renderPlanCard();
    toast(Number.isFinite(v) && v > 0 ? `New runs pause at ${v}% of a plan limit` : "Auto-pause off");
  });
  $("#resources-page").replaceChildren(
    h("div", { class: "res-grid" },
      h("div", { id: "res-plan" }),
      h("section", { class: "card pause-card" },
        h("h2", {}, "Auto-pause"),
        h("p", { class: "help" }, "Hold back new agent runs while either plan limit is at or above this percentage. Running agents finish their work; queued items wait until the window resets."),
        h("div", { class: "inline" }, pause, h("span", {}, "% of a plan limit"),
          h("button", { class: "small", onclick: () => { pause.value = ""; pause.onchange(); } }, "Turn off"))),
      h("div", { id: "res-system", class: "span2" }),
      h("div", { id: "res-agents", class: "span2" }),
      h("div", { id: "res-tokens", class: "span2" }),
    ),
  );
  renderResources();
}

function renderResources() {
  if (!R.built) return;
  renderPlanCard();
  renderTokens();
  renderSystem();
  renderAgents();
}

// ---- Plan usage ------------------------------------------------------------

function renderPlanCard() {
  const box = $("#res-plan");
  if (!box) return;
  const pause = $("#pause-above");
  if (pause && document.activeElement !== pause) pause.value = S.account?.pause_above ?? "";
  const windows = planWindows();
  const at = S.account?.limits?.at;
  box.replaceChildren(card("Plan usage",
    S.account?.paused ? h("div", { class: "banner warn" }, `⏸ New runs are paused: ${S.account.paused}`) : "",
    windows.length
      ? h("div", { class: "windows" }, windows.map((w) => h("div", { class: "window" },
          h("div", { class: "window-head" }, h("b", {}, `${w.name[0].toUpperCase()}${w.name.slice(1)} limit`), h("span", { class: "big" }, `${w.pct}%`)),
          meter(w.pct, "wide"),
          h("div", { class: "muted" }, w.resets ? `resets in ${fmtUntil(w.resets)} (${w.resets.toLocaleString([], { weekday: "short", hour: "2-digit", minute: "2-digit" })})` : ""))))
      : h("p", { class: "muted" }, "No plan usage reported yet. Claude Code reports it with each agent run, so it appears after the next run."),
    at ? h("p", { class: "muted small" }, `As of ${new Date(at).toLocaleTimeString()}, from the latest agent run. Usage from other Claude sessions counts too.`) : "",
  ));
}

// ---- Token usage -------------------------------------------------------------

function sumTokens(runs) {
  const t = { input: 0, output: 0, cache_read: 0, cache_write: 0, cost: 0, runs: runs.length, secs: 0 };
  for (const r of runs) {
    if (r.tokens) {
      t.input += r.tokens.input;
      t.output += r.tokens.output;
      t.cache_read += r.tokens.cache_read;
      t.cache_write += r.tokens.cache_write;
    }
    t.cost += r.cost_usd || 0;
    t.secs += runSecs(r);
  }
  t.total = t.input + t.output + t.cache_read + t.cache_write;
  return t;
}

function groupBy(runs, key) {
  const m = new Map();
  for (const r of runs) {
    const k = key(r);
    if (!m.has(k)) m.set(k, []);
    m.get(k).push(r);
  }
  return [...m.entries()].map(([k, rs]) => [k, sumTokens(rs)]).sort((a, b) => b[1].total - a[1].total);
}

function renderTokens() {
  const box = $("#res-tokens");
  if (!box) return;
  const all = [];
  for (const it of S.items.values()) for (const r of it.runs) all.push({ ...r, item: it });
  const dayAgo = Date.now() - 864e5;
  const day = all.filter((r) => new Date(r.started) >= dayAgo);
  const tAll = sumTokens(all), tDay = sumTokens(day);

  // Tokens per 15 minutes over the last 24 hours (attributed to the quarter-hour a run started).
  // Epoch-aligned quarters match local quarter-hours: time-zone offsets are multiples of 15 minutes.
  const Q = 9e5;
  const base = Math.floor(Date.now() / Q) * Q;
  const buckets = Array.from({ length: 96 }, (_, i) => ({ start: new Date(base - (95 - i) * Q), tokens: 0, output: 0 }));
  for (const r of day) {
    const t = new Date(r.started);
    const b = buckets.findLast((x) => x.start <= t);
    if (b && r.tokens) {
      b.tokens += tokTotal(r.tokens);
      b.output += r.tokens.output;
    }
  }
  const hm = (d) => `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
  const maxBucket = Math.max(1, ...buckets.map((x) => x.tokens));
  const chart = h("div", { class: "bars" }, buckets.map((x) =>
    h("div", { class: "bar", title: `${hm(x.start)}–${hm(new Date(x.start.getTime() + Q))}: ${x.tokens.toLocaleString()} tokens (${x.output.toLocaleString()} output)` },
      h("span", { style: `height:${(x.tokens / maxBucket) * 100}%` }),
      h("label", {}, x.start.getMinutes() === 0 && x.start.getHours() % 3 === 0 ? String(x.start.getHours()).padStart(2, "0") : ""))));

  const row = (name, t) => {
    const inp = t.input + t.cache_read + t.cache_write;
    return [name, t.runs, h("span", { title: tokTitle(t) }, fmtTok(t.total)), fmtTok(inp),
      inp ? `${fmtTok(t.cache_read)} (${Math.round((t.cache_read / inp) * 100)}%)` : fmtTok(t.cache_read),
      fmtTok(t.output), fmtTok(Math.round(t.total / Math.max(1, t.runs))), fmtSecs(t.secs / Math.max(1, t.runs)), `$${t.cost.toFixed(2)}`];
  };
  const head = ["", "Runs", "Total tokens", "Input", "Cached", "Output", "Avg per run", "Avg time", "≈ API $"];
  const steps = groupBy(all, (r) => r.step).sort((a, b) => stepIndex(a[0]) - stepIndex(b[0]));
  const models = groupBy(all, (r) => r.model);
  const items = groupBy(all, (r) => r.item.id).slice(0, 10);

  box.replaceChildren(card("Token usage",
    h("div", { class: "stats" },
      stat("Last 24 hours", fmtTok(tDay.total), `${tDay.runs} runs · ${fmtTok(tDay.output)} output`),
      stat("All time (this repo)", fmtTok(tAll.total), `${tAll.runs} runs · ${fmtTok(tAll.output)} output`),
      stat("Cache reads", tAll.total ? `${Math.round((tAll.cache_read / tAll.total) * 100)}%` : "–", "of all tokens; cheapest kind"),
      stat("≈ API-equivalent", `$${tAll.cost.toFixed(2)}`, "covered by your subscription"),
    ),
    h("h3", {}, "Tokens per 15 minutes, last 24 hours"), chart,
    h("div", { class: "tables" },
      h("div", { class: "table-wrap" }, h("h3", {}, "By step"), table(["Step", ...head.slice(1)], steps.map(([k, t]) => row(stepName(k), t)))),
      h("div", { class: "table-wrap" }, h("h3", {}, "By model"), table(["Model", ...head.slice(1)], models.map(([k, t]) => row(shortModel(k), t)))),
      h("div", { class: "table-wrap span-all" }, h("h3", {}, "Top items"), table(["Item", ...head.slice(1)], items.map(([k, t]) => {
        const it = S.items.get(k);
        return row(h("a", { href: "#", onclick: (e) => { e.preventDefault(); showPage("board"); openInspector(k, it?.runs.at(-1)?.n); } }, it?.title || k), t);
      }))),
    ),
    h("p", { class: "muted small" }, "Total tokens = input + output. Input includes cache reads and cache writes; Cached = cache reads, shown as a share of input. As reported by Claude Code. Cache reads are much cheaper than other input and count less toward plan limits."),
  ));
}

// ---- Running agents ------------------------------------------------------------

function renderAgents() {
  const box = $("#res-agents");
  if (!box) return;
  const agents = R.sys?.agents || [];
  const rows = agents.map(({ agent: a, usage: u }) => {
    const item = S.items.get(a.item);
    const run = item?.runs.find((r) => r.n === a.run);
    const t = run?.tokens;
    const ctx = t?.context && t?.context_window ? Math.round((t.context / t.context_window) * 100) : null;
    return [
      h("a", { href: "#", onclick: (e) => { e.preventDefault(); showPage("board"); openInspector(a.item, a.run); } }, item?.title || a.item),
      stepName(a.step),
      run ? shortModel(run.model) : "",
      run ? fmtSecs(runSecs(run)) : "",
      h("span", { title: tokTitle(t) }, t ? fmtTok(tokTotal(t)) : "–"),
      t?.context ? `${fmtTok(t.context)}${ctx != null ? ` (${ctx}%)` : ""}` : "–",
      u ? h("span", { class: "cell-meter" }, meter(u.cpu / Math.max(1, R.sys.system.cores.length)), `${Math.round(u.cpu)}%`) : "–",
      u ? fmtBytes(u.rss) : "–",
      u ? h("span", { title: u.top.length ? `busiest: ${u.top.join(", ")}` : "" }, `${u.processes}${u.top.length ? ` · ${u.top.join(", ")}` : ""}`) : "–",
    ];
  });
  const server = R.sys?.server;
  box.replaceChildren(card(`Running agents (${agents.length})`,
    table(["Item", "Step", "Model", "Time", "Tokens", "Context", "CPU", "Memory", "Processes"], rows, 3),
    h("p", { class: "muted small" },
      "CPU and memory include everything an agent started (builds, tests). 100% CPU is one full core.",
      server ? ` ClaudeChain itself: ${fmtBytes(server.rss)}, ${server.processes} process${server.processes === 1 ? "" : "es"} including agents.` : ""),
  ));
}

// ---- System ------------------------------------------------------------------

function renderSystem() {
  const box = $("#res-system");
  if (!box) return;
  const s = R.sys?.system;
  if (!s) return box.replaceChildren(card("System", h("p", { class: "muted" }, "Loading…")));
  if (!s.supported) return box.replaceChildren(card("System", h("p", { class: "muted" }, "System monitoring is only available on Linux.")));
  const hist = R.sys.history || [];
  const mem = s.memory;
  const used = mem.total - mem.available;
  const memPct = mem.total ? (used / mem.total) * 100 : 0;
  const swapUsed = mem.swap_total - mem.swap_free;
  const cores = s.cores.length;
  const temps = hist.map((p) => p.cpu_temp).filter((t) => t != null);
  const diskPct = s.disk ? ((s.disk.total - s.disk.free) / s.disk.total) * 100 : 0;

  box.replaceChildren(card("System",
    h("div", { class: "sys-grid" },
      h("div", { class: "sys-tile" },
        stat("CPU", `${Math.round(s.cpu)}%`, `${cores} cores · load ${s.load.map((l) => l.toFixed(2)).join(" ")}`),
        spark(hist.map((p) => p.cpu), 100, "cpu"),
        h("div", { class: "cores", title: "Per core" }, s.cores.map((c, i) =>
          h("span", { class: "core", title: `core ${i}: ${Math.round(c)}%` }, h("span", { class: `fill ${levelClass(c)}`, style: `height:${Math.max(2, c)}%` }))))),
      h("div", { class: "sys-tile" },
        stat("Memory", `${fmtBytes(used)}`, `of ${fmtBytes(mem.total)} (${Math.round(memPct)}%)${mem.swap_total ? ` · swap ${fmtBytes(swapUsed)} / ${fmtBytes(mem.swap_total)}` : " · no swap"}`),
        spark(hist.map((p) => p.mem_used), mem.total, "mem"),
        meter(memPct, "wide")),
      h("div", { class: "sys-tile" },
        stat("CPU temperature", s.cpu_temp != null ? `${Math.round(s.cpu_temp)}°C` : "–",
          s.sensors.length ? `${s.sensors.length} sensors` : "no temperature sensors found (common in virtual machines)"),
        temps.length > 1 ? spark(temps, 105, "temp") : "",
        s.sensors.length ? h("details", { class: "fold" }, h("summary", {}, "All sensors"),
          table(["Sensor", "°C"], s.sensors.map((x) => [x.name, x.celsius.toFixed(1)]))) : ""),
      h("div", { class: "sys-tile" },
        stat("Disk (data directory)", s.disk ? `${fmtBytes(s.disk.free)} free` : "–", s.disk ? `of ${fmtBytes(s.disk.total)} · worktrees and builds live here` : ""),
        s.disk ? meter(diskPct, "wide") : "",
        h("div", { class: "muted small", style: "margin-top:8px" }, `Uptime ${fmtSecs(s.uptime_secs)}`)),
    ),
  ));
}
