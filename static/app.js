"use strict";

// ---- State ---------------------------------------------------------------

const S = {
  repo: null,
  data: null,
  defaultData: null,
  recent: [],
  home: "",
  pipeline: null,
  items: new Map(),
  insp: null, // { id, run, tab, data, file }
  account: null, // { limits, pause_above, paused }
  page: "board",
  renaming: null, // step id whose header name is being edited
};

const $ = (sel) => document.querySelector(sel);

function h(tag, attrs = {}, ...children) {
  const el = document.createElement(tag);
  if (tag === "button" && !("type" in attrs)) el.type = "button";
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null || v === false) continue;
    if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else if (k === "class") el.className = v;
    else if (k === "html") el.innerHTML = v;
    else el.setAttribute(k, v === true ? "" : v);
  }
  for (const c of children.flat()) {
    if (c == null || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

async function api(method, path, body) {
  const res = await fetch(path, {
    method,
    // The server rejects changes without this header (see web::guard).
    headers: { "x-claudechain": "1", ...(body ? { "content-type": "application/json" } : {}) },
    body: body ? JSON.stringify(body) : undefined,
  });
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(data.error || res.statusText);
  return data;
}

function toast(msg) {
  const t = $("#toast");
  t.textContent = msg;
  t.hidden = false;
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => (t.hidden = true), 4000);
}

async function act(fn) {
  try {
    return await fn();
  } catch (e) {
    toast(e.message);
  }
}

const stepById = (id) => S.pipeline.steps.find((s) => s.id === id);
const stepName = (id) => (id === "done" ? "Done" : stepById(id)?.name ?? id);
const stepIndex = (id) => S.pipeline.steps.findIndex((s) => s.id === id);
const shortModel = (m) => m.replace(/^claude-/, "");
const fmtCost = (c) => (c == null ? "" : `$${c.toFixed(2)}`);
/// CPU in percent of one core (an agent tree can use several cores, so >100%).
const fmtCpu = (c) => (c == null ? "" : `${Math.round(c)}%`);

function fmtTok(n) {
  if (n == null) return "";
  if (n < 1000) return String(n);
  if (n < 1e6) return `${(n / 1000).toFixed(n < 1e4 ? 1 : 0)}k`;
  return `${(n / 1e6).toFixed(n < 1e7 ? 2 : 1)}M`;
}
const tokTotal = (t) => (t ? t.input + t.output + t.cache_read + t.cache_write : 0);
/// Tokens split into read (input: uncached + cached) and write (output + cache write).
const tokParts = (t) => ({
  read: t.input + t.cache_read, uncached: t.input, cached: t.cache_read,
  write: t.output + t.cache_write, output: t.output, cacheWrite: t.cache_write,
});
const tokTitle = (t) => {
  if (!t) return "";
  const p = tokParts(t), n = (x) => x.toLocaleString();
  return `read ${n(p.read)}: uncached ${n(p.uncached)} · cached ${n(p.cached)}\nwrite ${n(p.write)}: output ${n(p.output)} · cache write ${n(p.cacheWrite)}`;
};

/// Plan windows from the latest rate_limit_event, ignoring ones that already reset.
function planWindows() {
  const w = S.account?.limits?.info?.unifiedWindows || {};
  const now = Date.now() / 1000;
  return Object.entries(w).map(([key, v]) => ({
    key,
    name: key === "five_hour" ? "5-hour" : key === "seven_day" ? "weekly" : key.replace(/_/g, " "),
    short: key === "five_hour" ? "5h" : key === "seven_day" ? "wk" : key.slice(0, 3),
    pct: v.resetsAt > now ? Math.round((v.utilization || 0) * 100) : 0,
    resets: v.resetsAt ? new Date(v.resetsAt * 1000) : null,
    status: v.status,
  }));
}

function fmtUntil(date) {
  if (!date) return "";
  const m = Math.max(0, Math.round((date - Date.now()) / 60000));
  if (m < 60) return `${m}m`;
  if (m < 48 * 60) return `${Math.floor(m / 60)}h${String(m % 60).padStart(2, "0")}`;
  return `${Math.round(m / 1440)}d`;
}

const levelClass = (pct) => (pct >= 90 ? "bad" : pct >= 70 ? "warn" : "ok");

function renderPlan() {
  const el = $("#plan");
  const windows = planWindows();
  el.hidden = !windows.length && !S.account?.paused;
  el.classList.toggle("paused", !!S.account?.paused);
  el.replaceChildren(...[
    S.account?.paused && h("span", { class: "plan-paused" }, "⏸ paused"),
    ...windows.map((w) => h("span", { class: "plan-win" },
      h("span", { class: "plan-label" }, w.short),
      h("span", { class: "meter" }, h("span", { class: `fill ${levelClass(w.pct)}`, style: `width:${w.pct}%` })),
      `${w.pct}%`)),
  ].filter(Boolean));
  el.title = [
    ...windows.map((w) => `${w.name} limit: ${w.pct}% used, resets in ${fmtUntil(w.resets)}`),
    S.account?.paused ? `New runs paused: ${S.account.paused}` : null,
    "Click for details",
  ].filter(Boolean).join("\n");
}

function fmtDuration(run) {
  if (!run) return "";
  const end = run.ended ? new Date(run.ended) : new Date();
  const s = Math.max(0, Math.round((end - new Date(run.started)) / 1000));
  return s < 60 ? `${s}s` : `${Math.floor(s / 60)}m${String(s % 60).padStart(2, "0")}s`;
}

const STATUS_LABEL = {
  queued: "⏳ queued",
  awaiting_approval: "⏸ awaiting approval",
  running: "running",
  needs_human: "⚠ needs human",
  done: "✓ done",
};

// ---- Loading & live updates -------------------------------------------

async function load() {
  const st = await api("GET", "/api/state");
  S.repo = st.repo;
  S.data = st.data ?? null;
  S.defaultData = st.default_data ?? null;
  S.recent = st.recent || [];
  S.home = st.home;
  S.pipeline = st.pipeline ?? null;
  S.account = st.account ?? null;
  renderPlan();
  S.items = new Map((st.items || []).map((i) => [i.id, i]));
  // The folder name keeps the header on one line; the tooltip has the full path.
  $("#repo-path").textContent = st.repo ? st.repo.replace(/\/+$/, "").split("/").pop() : "no repository";
  $("#repo").title = st.repo ? `Repository ${st.repo}\nData ${st.data}\n\nClick to open another repository` : "Open a repository";
  $("#btn-add").disabled = !st.repo;
  $("#no-repo").hidden = !!st.repo;
  // Without a repository the page is a landing page: no top bar, just a card for opening one.
  document.body.classList.toggle("no-repo", !st.repo);
  document.body.classList.remove("loading");
  if (!st.repo) {
    $("#board").replaceChildren();
    $("#empty").hidden = true;
    closeInspector();
    renderLanding();
    return;
  }
  renderBoard();
  if (S.insp) renderInspector();
}

/// Landing page: one-click buttons for recently opened repositories.
function renderLanding() {
  $("#landing-recent").hidden = !S.recent.length;
  $("#landing-recent-list").replaceChildren(...S.recent.map((r) => {
    const data = r.data === `${r.repo.replace(/\/+$/, "")}/.claudechain` ? null : r.data;
    return h("li", {},
      h("button", {
        title: `Open ${r.repo}\nData ${r.data}`,
        onclick: async (e) => {
          const btn = e.currentTarget;
          btn.disabled = true;
          try {
            await api("POST", "/api/workspace", { repo: r.repo, data });
            await load();
          } catch (err) {
            toast(err.message);
          } finally {
            btn.disabled = false;
          }
        },
      },
        h("span", { class: "name" }, `📁 ${r.repo.replace(/\/+$/, "").split("/").pop() || r.repo}`),
        h("small", {}, data ? `${r.repo} · data: ${data}` : r.repo)));
  }));
}

function connect() {
  const es = new EventSource("/api/events");
  es.onopen = () => {
    $("#conn").classList.add("on");
    load();
  };
  es.onerror = () => $("#conn").classList.remove("on");
  es.onmessage = (m) => {
    const ev = JSON.parse(m.data);
    switch (ev.kind) {
      case "item":
        S.items.set(ev.item.id, ev.item);
        scheduleBoard();
        if (S.insp?.id === ev.item.id) refreshInspectorFromItem(ev.item);
        break;
      case "item_removed":
        S.items.delete(ev.id);
        scheduleBoard();
        if (S.insp?.id === ev.id) closeInspector();
        break;
      case "limits":
        S.account = ev.account;
        renderPlan();
        if (S.page === "resources") renderResources();
        break;
      case "workspace":
        closeInspector();
        load();
        break;
      case "pipeline":
      case "resync":
        load();
        break;
      case "run_event":
        if (S.insp?.id === ev.item && S.insp.run === ev.run && S.insp.data) {
          S.insp.data.events.push(ev.event);
          if (S.insp.tab === "transcript") appendTranscriptEvent(ev.event);
        }
        break;
    }
  };
}

let boardPending = false;
function scheduleBoard() {
  if (boardPending) return;
  boardPending = true;
  requestAnimationFrame(() => {
    boardPending = false;
    renderBoard();
    if (S.page === "resources") renderResources();
  });
}

// ---- Board ---------------------------------------------------------------

function renderBoard() {
  const p = S.pipeline;
  if (!p) return;
  // A header rename is in progress; re-rendering would destroy its input.
  // renameStep() calls renderBoard() again when it finishes.
  if (S.renaming) return;
  const board = $("#board");
  const items = [...S.items.values()].sort((a, b) => a.created.localeCompare(b.created));
  $("#empty").hidden = items.length > 0;

  const busy = {};
  for (const it of items) if (it.status === "running") busy[it.step] = (busy[it.step] || 0) + 1;

  const head = h("tr", {},
    h("th", { class: "corner" }, `${items.length} item${items.length === 1 ? "" : "s"}`,
      h("div", {}, `max ${p.max_iterations} bounces per item`),
      h("button", { class: "small", style: "margin-top:6px", onclick: pipelineDialog }, "Edit pipeline details…")),
    p.steps.map((s) => stepHeader(s, busy[s.id] || 0)),
    h("th", { class: "row-actions" }),
  );
  const rows = items.map((it) => h("tr", {},
    itemCell(it),
    p.steps.map((s) => stepCell(it, s)),
    rowActions(it),
  ));
  board.replaceChildren(h("thead", {}, head), h("tbody", {}, rows));
}

// Thinking levels for `claude --effort`; "" means Claude's default (no flag).
const EFFORT_LEVELS = ["low", "medium", "high", "xhigh", "max"];

function effortOptions(current, defaultLabel) {
  return [
    h("option", { value: "", selected: !current }, defaultLabel),
    ...EFFORT_LEVELS.map((e) => h("option", { value: e, selected: e === current }, e)),
  ];
}

function stepHeader(s, busy) {
  const models = [...new Set([...S.pipeline.models, s.model])];
  const put = (body) => act(() => api("PUT", `/api/steps/${s.id}`, body));
  return h("th", { class: "step-head" },
    h("div", { class: "step-head-inner" },
      h("div", { class: "name" },
        h("span", { class: "label", title: "Double-click to rename", ondblclick: (e) => renameStep(s, e.currentTarget) }, s.name),
        h("span", { class: "busy" }, `${busy}/${s.max_workers} busy`)),
      h("div", { class: "row" },
        h("select", { title: "Model", onchange: (e) => put({ model: e.target.value }) },
          models.map((m) => h("option", { value: m, selected: m === s.model }, shortModel(m)))),
        h("select", { title: "Thinking level (claude --effort)", onchange: (e) => put({ effort: e.target.value }) },
          effortOptions(s.effort, "effort: default")),
      ),
      h("div", { class: "row" },
        h("button", { class: "small", title: `${s.name} prompt`, onclick: () => editPrompt(s.id, `${s.name} prompt`) }, "Edit prompt"),
      ),
    ),
  );
}

// Swaps a step header's name for a text input. Enter or blur saves, Escape cancels.
function renameStep(s, label) {
  if (S.renaming) return;
  S.renaming = s.id;
  let finished = false;
  const input = h("input", { type: "text", class: "rename", value: s.name, maxlength: 80 });
  const finish = async (save) => {
    if (finished) return;
    finished = true;
    const name = input.value.trim();
    S.renaming = null;
    if (save && name && name !== s.name) {
      // On success the server emits a "pipeline" event, which reloads and re-renders.
      await act(() => api("PUT", `/api/steps/${s.id}`, { name }));
    }
    renderBoard();
  };
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") { e.preventDefault(); finish(true); }
    else if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); finish(false); }
  });
  input.addEventListener("blur", () => finish(true));
  label.replaceWith(input);
  input.focus();
  input.select();
}

function itemCell(it) {
  const cost = it.runs.reduce((sum, r) => sum + (r.cost_usd || 0), 0);
  const tokens = it.runs.reduce((sum, r) => sum + tokTotal(r.tokens), 0);
  const last = it.runs[it.runs.length - 1];
  return h("td", { class: "item-cell" },
    h("div", { class: "title", onclick: () => openInspector(it.id, last?.n, true) }, it.number ? `#${it.number} ` : "", it.title || it.id),
    h("div", { class: "sub" },
      h("span", {}, it.id),
      it.url && h("a", { href: it.url, target: "_blank", rel: "noopener" }, "issue ↗"),
      it.pr_url && h("a", { href: it.pr_url, target: "_blank", rel: "noopener" }, "PR ↗"),
      it.iteration > 0 && h("span", { title: "Backward moves" }, `↩ ${it.iteration}`),
      tokens > 0 && h("span", { title: `${tokens.toLocaleString()} tokens over ${it.runs.length} runs; ≈ ${fmtCost(cost)} API-equivalent (covered by your subscription)` }, `${fmtTok(tokens)} tok`),
    ),
    it.status === "done" && h("div", { class: `state done` }, STATUS_LABEL.done),
    it.pending_notes.length > 0 && h("div", { class: "detail" }, `📝 ${it.pending_notes.length} note(s) for the next run`),
  );
}
// Last column: a small remove cross, shown once the item is done.
function rowActions(it) {
  return h("td", { class: "row-actions" },
    it.status === "done" && h("button", {
      class: "remove-x",
      title: "Remove this finished item from the board",
      "aria-label": "Remove",
      onclick: () => removeItem(it.id),
    }, "✕"));
}

function runChipClass(run) {
  if (!run.ended) return "running";
  if (run.failed) return "fail";
  const to = run.transition?.to;
  if (to && to !== "next" && to !== "human" && stepIndex(to) <= stepIndex(run.step)) return "back";
  return "ok";
}

function runChipLabel(run) {
  if (!run.ended) return `#${run.n} …`;
  const to = run.transition?.to;
  if (run.failed) return `#${run.n} ✗`;
  if (to && to !== "next") return `#${run.n} ↩ ${stepName(to)}`;
  return `#${run.n} ✓`;
}

function stepCell(it, s) {
  const runs = it.runs.filter((r) => r.step === s.id);
  const current = it.step === s.id && it.status !== "done";
  const td = h("td", {
    class: `cell${current ? " current" : ""}`,
    onclick: () => openInspector(it.id, runs.length ? runs[runs.length - 1].n : null),
  });
  if (runs.length) {
    td.append(h("div", { class: "chips" }, runs.map((r) =>
      h("span", {
        class: `chip ${runChipClass(r)}`,
        title: `${shortModel(r.model)} · ${fmtDuration(r)}${tokTotal(r.tokens) > 0 ? `\n${tokTotal(r.tokens).toLocaleString()} tokens\n${tokTitle(r.tokens)}` : ""}${r.outcome ? "\n" + r.outcome : ""}`,
        onclick: (e) => { e.stopPropagation(); openInspector(it.id, r.n); },
      }, runChipLabel(r), tokTotal(r.tokens) > 0 && h("span", { class: "tok" }, ` · ${fmtTok(tokTotal(r.tokens))}`)))));
  }
  if (current) {
    const cls = it.status;
    const liveRun = cls === "running" ? it.runs[it.runs.length - 1] : null;
    const live = liveRun?.tokens;
    td.append(h("div", { class: `state ${cls}${cls === "running" ? " pulse" : ""}`, title: tokTitle(live) },
      STATUS_LABEL[cls], live ? ` · ${fmtTok(tokTotal(live))}` : ""));
    const u = liveRun && !liveRun.ended ? liveRun.usage : null;
    if (u) {
      td.append(h("div", {
        class: "usage",
        title: `Agent and all its sub-processes (${u.processes} process${u.processes === 1 ? "" : "es"})\nCPU in % of one core · peak ${fmtCpu(u.cpu_max)}\nMemory peak ${fmtBytes(u.rss_max)}`,
      },
        h("span", { class: "info" }, `CPU ${fmtCpu(u.cpu)}`),
        h("span", { class: "info" }, fmtBytes(u.rss))));
    }
    if (it.status === "awaiting_approval") {
      td.append(h("div", { class: "gate" },
        h("button", { class: "small primary", onclick: (e) => { e.stopPropagation(); act(() => api("POST", `/api/items/${it.id}/approve`)); } }, "Approve"),
        h("button", { class: "small", onclick: (e) => { e.stopPropagation(); moveDialog(it, true); } }, "Send back…"),
      ));
    }
    if (it.status_detail && it.status !== "running") {
      td.append(h("div", { class: "last", title: it.status_detail }, it.status_detail));
    }
  } else if (runs.length) {
    const last = runs[runs.length - 1];
    if (last.transition?.summary) td.append(h("div", { class: "last", title: last.transition.summary }, last.transition.summary));
  }
  return td;
}

// ---- Inspector -----------------------------------------------------------

/// True when run `n` is the item's newest run and still going (or there is no run yet).
function isLiveLatest(item, n) {
  const last = item?.runs[item.runs.length - 1];
  return n == null || (last != null && last.n === n && !last.ended);
}

/// `follow`: move to each new run of the item as it starts. Defaults to true only when
/// the chosen run is the live latest one; a pinned (ended or older) run stays put.
async function openInspector(id, run, follow) {
  run = run ?? null;
  follow = follow ?? isLiveLatest(S.items.get(id), run);
  S.insp = { id, run, follow, tab: S.insp?.tab || "transcript", data: null, file: null };
  $("#inspector").hidden = false;
  await loadRun();
}

function closeInspector() {
  S.insp = null;
  $("#inspector").hidden = true;
}

async function loadRun() {
  const insp = S.insp;
  if (!insp) return;
  insp.data = null;
  if (insp.run != null) {
    try {
      insp.data = await api("GET", `/api/items/${insp.id}/runs/${insp.run}`);
    } catch (e) {
      toast(e.message);
    }
  }
  if (S.insp === insp) renderInspector();
}

/// Called when the open item changes. Switches to a new run only when the inspector is
/// following (`insp.follow`); otherwise it stays on the run the user picked.
function refreshInspectorFromItem(item) {
  const insp = S.insp;
  const last = item.runs[item.runs.length - 1];
  if (insp.follow && last && insp.run !== last.n) {
    insp.run = last.n; // follow the new run live
    loadRun();
    return;
  }
  if (insp.data && insp.run != null) {
    const run = item.runs.find((r) => r.n === insp.run);
    const wasRunning = !insp.data.run.ended;
    if (run) insp.data.run = run;
    if (wasRunning && run?.ended) return void loadRun(); // pick up the message and transition
  }
  renderInspectorChrome();
}

function renderInspector() {
  renderInspectorChrome();
  renderTab();
}

function renderInspectorChrome() {
  const insp = S.insp;
  const item = S.items.get(insp.id);
  if (!item) return closeInspector();
  const run = insp.data?.run;

  $("#insp-item").textContent = `${item.number ? "#" + item.number + " " : ""}${item.title}`;
  const meta = [
    `${stepName(item.step)} · ${STATUS_LABEL[item.status] || item.status}`,
    run && `run #${run.n} ${stepName(run.step)} · ${shortModel(run.model)} · ${fmtDuration(run)}`,
    run?.tokens?.context && `context ${fmtTok(run.tokens.context)}${run.tokens.context_window ? ` / ${fmtTok(run.tokens.context_window)}` : ""}`,
    run?.cost_usd != null && `≈ ${fmtCost(run.cost_usd)} API-equivalent`,
    run?.resumed_from && `continues #${run.resumed_from}`,
  ].filter(Boolean);
  $("#insp-meta").textContent = meta.join("  ·  ");
  if (run?.tokens) {
    const p = tokParts(run.tokens);
    const total = tokTotal(run.tokens);
    $("#insp-meta").append(h("div", { class: "insp-tokens", title: `${total.toLocaleString()} tokens\n${tokTitle(run.tokens)}` },
      `Tokens ${fmtTok(total)} · read ${fmtTok(p.read)} (cached ${fmtTok(p.cached)} · uncached ${fmtTok(p.uncached)})`
      + ` · write ${fmtTok(p.write)} (output ${fmtTok(p.output)} · cache write ${fmtTok(p.cacheWrite)})`));
  }
  if (item.status_detail && item.status !== "running") {
    $("#insp-meta").append(h("div", {}, item.status_detail));
  }

  const sel = $("#insp-run");
  sel.replaceChildren(
    ...(item.runs.length ? [] : [h("option", { value: "" }, "no runs yet")]),
    ...[...item.runs].reverse().map((r) =>
      h("option", { value: r.n, selected: r.n === insp.run }, `#${r.n} ${stepName(r.step)} ${r.ended ? (r.failed ? "✗" : "") : "…"}`)),
  );
  $("#insp-stop").disabled = item.status !== "running";
  $("#insp-retry").disabled = item.status === "running";
  $("#insp-move").disabled = item.status === "running";

  for (const b of document.querySelectorAll("#insp-tabs button")) {
    b.classList.toggle("active", b.dataset.tab === insp.tab);
  }
  renderComposer(item, run);
}

function renderComposer(item, run) {
  const actions = $("#composer-actions");
  const text = () => $("#composer-text").value.trim();
  const send = (mode) => act(async () => {
    if (!text()) return toast("Type a message first");
    await api("POST", `/api/items/${item.id}/message`, { text: text(), mode, run: run?.n });
    $("#composer-text").value = "";
    toast(mode === "live" ? "Sent to the agent" : mode === "note" ? "Saved for the next run" : "Run continues with your message");
  });
  const live = item.status === "running" && run && !run.ended;
  const canContinue = item.status !== "running" && run?.session_id;
  actions.replaceChildren(...[
    h("span", { class: "hint" },
      live ? "The agent sees your message on its next turn."
        : canContinue ? `Continue resumes run #${run.n}'s session with full context; a note is added to the next run's prompt.`
        : "Notes are added to the next run's prompt."),
    live && h("button", { type: "button", class: "primary", onclick: () => send("live") }, "Send to agent"),
    !live && canContinue && h("button", { type: "button", class: "primary", onclick: () => send("continue") }, "Continue this run"),
    h("button", { type: "button", onclick: () => send("note") }, "Note for next run"),
  ].filter(Boolean));
}

function renderTab() {
  const insp = S.insp;
  const body = $("#insp-body");
  body.replaceChildren();
  const d = insp.data;
  if (!d && insp.tab !== "files" && insp.tab !== "diff") {
    body.append(h("p", { class: "empty" }, "This item has no runs yet. It is waiting for a free worker or approval."));
    return;
  }
  ({ transcript: renderTranscript, input: renderInput, output: renderOutput, files: renderFiles, diff: renderDiff })[insp.tab](body, d);
}

// Transcript ---------------------------------------------------------------

const SKIP_SYSTEM = new Set(["thinking_tokens", "commands_changed", "task_summary", "task_started", "task_notification", "post_turn_summary", "status"]);

function renderTranscript(body, d) {
  body.append(h("div", { id: "transcript" }));
  if (d.input?.prompt) {
    $("#transcript").append(h("details", { class: "fold ev" },
      h("summary", {}, d.input.resume_session ? "Resume message sent to the agent" : "Initial prompt sent to the agent"),
      h("pre", {}, d.input.prompt)));
  }
  for (const ev of d.events) appendTranscriptEvent(ev, true);
  if (!d.run.ended) body.scrollTop = body.scrollHeight;
}

function toolSummary(name, input) {
  if (!input) return "";
  const v = input.command ?? input.file_path ?? input.path ?? input.pattern ?? input.url ?? input.description ?? input.prompt;
  return v ? String(v).split("\n")[0] : "";
}

function toolResultText(content) {
  if (typeof content === "string") return content;
  if (Array.isArray(content)) return content.map((c) => (c.type === "text" ? c.text : `[${c.type}]`)).join("\n");
  return JSON.stringify(content, null, 2);
}

function clip(text, n = 20000) {
  return text.length > n ? text.slice(0, n) + `\n… (${text.length - n} more characters)` : text;
}

function appendTranscriptEvent(ev, initial = false) {
  const box = $("#transcript");
  if (!box) return;
  const scroller = $("#insp-body");
  const atBottom = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 60;
  const add = (el) => el && box.append(el);

  switch (ev.type) {
    case "system":
      if (ev.subtype === "init") {
        add(h("div", { class: "ev sys" }, `Session ${ev.session_id?.slice(0, 8) ?? ""} started · ${ev.model ?? ""} · ${ev.permissionMode ?? ""} · tools: ${(ev.tools || []).join(", ")}`));
      } else if (!SKIP_SYSTEM.has(ev.subtype)) {
        add(h("div", { class: "ev sys" }, `system: ${ev.subtype}`));
      }
      break;
    case "assistant":
      for (const block of ev.message?.content || []) {
        if (block.type === "text" && block.text?.trim()) {
          add(h("div", { class: "ev assistant" }, h("div", { class: "who" }, "Claude"), h("div", { class: "body" }, block.text)));
        } else if (block.type === "thinking" && block.thinking?.trim()) {
          add(h("details", { class: "fold ev" }, h("summary", {}, "thinking"), h("pre", {}, block.thinking)));
        } else if (block.type === "tool_use") {
          add(h("details", { class: "tool ev", "data-tool": block.id },
            h("summary", {}, h("span", { class: "tname" }, block.name), " ", toolSummary(block.name, block.input)),
            h("div", { class: "inner" }, h("pre", {}, JSON.stringify(block.input, null, 2)), h("div", { class: "out" }))));
        }
      }
      break;
    case "user": {
      const content = ev.message?.content;
      if (!Array.isArray(content)) break;
      for (const block of content) {
        if (block.type !== "tool_result") continue;
        const tool = box.querySelector(`[data-tool="${CSS.escape(block.tool_use_id)}"]`);
        const pre = h("pre", {}, clip(toolResultText(block.content)));
        if (tool) {
          tool.querySelector(".out").replaceChildren(h("div", { class: "who" }, block.is_error ? "error" : "result"), pre);
          if (block.is_error) tool.querySelector("summary").append(h("span", { class: "err" }, "  ✗"));
        } else {
          add(h("div", { class: "ev" }, pre));
        }
      }
      break;
    }
    case "result": {
      // The result text usually repeats the last assistant message.
      const lastText = [...box.querySelectorAll(".ev.assistant .body")].pop()?.textContent;
      const text = ev.result && ev.result.trim() !== lastText?.trim() ? ev.result : null;
      add(h("div", { class: `result-box ${ev.is_error ? "bad" : "ok"}` },
        h("div", { class: "who" }, `Result: ${ev.subtype} · ${ev.num_turns ?? "?"} turns · ${Math.round((ev.duration_ms || 0) / 1000)}s · ≈ ${fmtCost(ev.total_cost_usd)}`),
        text && h("div", { style: "white-space:pre-wrap" }, text)));
      break;
    }
    case "claudechain":
      if (ev.subtype === "human_message") {
        add(h("div", { class: "ev human" }, h("div", { class: "who" }, "You"), h("div", { style: "white-space:pre-wrap" }, ev.text)));
      } else if (ev.subtype === "raw_output") {
        add(h("pre", { class: "ev" }, ev.text));
      } else {
        add(h("div", { class: "ev warn" }, ev.text));
      }
      break;
  }
  if (!initial && atBottom) scroller.scrollTop = scroller.scrollHeight;
}

// Input / output ---------------------------------------------------------

function kv(pairs) {
  return h("dl", { class: "kv" }, pairs.filter(([, v]) => v != null && v !== "").flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, v)]));
}

function shellQuote(a) {
  return /^[\w@%+=:,./-]+$/.test(a) ? a : `'${a.replace(/'/g, `'\\''`)}'`;
}

function renderInput(body, d) {
  const i = d.input;
  if (!i) return body.append(h("p", { class: "empty" }, "No input recorded (the run did not start)."));
  body.append(
    kv([
      ["Model", i.model],
      ["Effort", i.effort],
      ["Step", stepName(i.step)],
      ["Tools", (i.tools || []).join(", ")],
      ["Permission mode", i.permission_mode],
      ["Working dir", i.cwd],
      ["Resumed session", i.resume_session],
    ]),
    i.notes?.length ? h("div", {}, h("h3", {}, "Notes included"), h("pre", {}, i.notes.map((n) => `• ${n.text}`).join("\n"))) : "",
    h("h3", {}, "System prompt (appended to Claude Code's)"), h("pre", {}, i.system_prompt),
    h("h3", {}, i.resume_session ? "Resume message" : "Prompt"), h("pre", {}, i.prompt),
    h("details", { class: "fold" }, h("summary", {}, "Full command line"),
      h("pre", {}, (i.command || []).map(shellQuote).join(" "))),
  );
}

function renderOutput(body, d) {
  const r = d.run;
  body.append(
    kv([
      ["Outcome", r.outcome ?? (r.ended ? "" : "running…")],
      ["Session", r.session_id],
      ["Started", new Date(r.started).toLocaleString()],
      ["Duration", fmtDuration(r)],
      ["Cost", r.cost_usd != null ? `≈ ${fmtCost(r.cost_usd)} API-equivalent (subscription)` : null],
      ["CPU" + (r.ended ? " (last)" : ""), r.usage ? `${fmtCpu(r.usage.cpu)} of one core · peak ${fmtCpu(r.usage.cpu_max)}` : null],
      ["Memory" + (r.ended ? " (last)" : ""), r.usage ? `${fmtBytes(r.usage.rss)} · peak ${fmtBytes(r.usage.rss_max)}` : null],
      ["Processes" + (r.ended ? " (last)" : ""), r.usage ? `${r.usage.processes} (agent and sub-processes)` : null],
    ]),
    h("h3", {}, "Transition"), h("pre", {}, d.transition ?? "(not written)"),
    h("h3", {}, "Message file"), h("pre", { style: "max-height:none" }, d.message ?? "(not written)"),
    d.stderr ? h("div", {}, h("h3", {}, "stderr"), h("pre", {}, d.stderr)) : "",
  );
}

async function renderFiles(body) {
  const insp = S.insp;
  const files = await act(() => api("GET", `/api/items/${insp.id}/files`));
  if (!files || S.insp !== insp || insp.tab !== "files") return;
  const view = h("div", {}, h("p", { class: "empty" }, "Select a file"));
  const list = h("ul", {});
  const show = async (f, li) => {
    insp.file = f;
    list.querySelectorAll("li").forEach((x) => x.classList.toggle("active", x === li));
    const res = await act(() => api("GET", `/api/items/${insp.id}/file?path=${encodeURIComponent(f)}`));
    if (res) view.replaceChildren(h("h3", {}, f), h("pre", { style: "max-height:none" }, clip(res.text, 200000)));
  };
  for (const f of files) {
    const li = h("li", { onclick: () => show(f, li) }, f);
    list.append(li);
    if (f === insp.file) show(f, li);
  }
  const note = h("p", { class: "files-note" },
    "Everything in this work item's folder ", h("code", {}, `.claudechain/items/${insp.id}/`), ". ",
    h("b", {}, "messages/"), " holds the hand-off notes each agent writes; the next agent gets the list of these and is told to read them. ",
    h("b", {}, "runs/"), " holds per-run logs (input, event stream, stderr, transition). ",
    "This is not a list of files the agent read, and not the code changes. For those, see the Diff tab.");
  body.replaceChildren(note, h("div", { class: "files" }, list, view));
}

async function renderDiff(body) {
  const insp = S.insp;
  body.append(h("p", { class: "empty" }, "Loading diff…"));
  const res = await act(() => api("GET", `/api/items/${insp.id}/diff`));
  if (!res || S.insp !== insp || insp.tab !== "diff") return;
  const pre = h("pre", { class: "diff", style: "max-height:none" });
  for (const line of res.text.split("\n")) {
    const cls = line.startsWith("+") && !line.startsWith("+++") ? "add"
      : line.startsWith("-") && !line.startsWith("---") ? "del"
      : line.startsWith("@@") ? "hunk" : null;
    pre.append(cls ? h("span", { class: cls }, line) : line, "\n");
  }
  body.replaceChildren(h("button", { class: "small", onclick: () => renderTab() }, "↻ Refresh"), pre);
}

// ---- Dialogs -------------------------------------------------------------

/// Opens the shared dialog; `onOk` returns false/throws to keep it open.
function dialog(title, content, onOk, okLabel = "Save", wide = false) {
  const dlg = $("#dialog");
  dlg.classList.toggle("wide", wide);
  $("#dialog-title").textContent = title;
  $("#dialog-body").replaceChildren(...content);
  $("#dialog-error").textContent = "";
  $("#dialog-ok").textContent = okLabel;
  $("#dialog-ok").disabled = false;
  dlg.onclose = null;
  $("#dialog-form").onsubmit = async (e) => {
    if (e.submitter?.value !== "ok") return;
    e.preventDefault();
    $("#dialog-ok").disabled = true;
    try {
      await onOk();
      dlg.close();
    } catch (err) {
      $("#dialog-error").textContent = err.message;
    } finally {
      $("#dialog-ok").disabled = false;
    }
  };
  dlg.showModal();
}

async function editPrompt(id, title) {
  const { text } = await api("GET", `/api/prompts/${id}`);
  const ta = h("textarea", { rows: 24 });
  ta.value = text;
  const help = id === "general"
    ? "Shared preamble for every step, followed by the step prompt. Each run also gets the item's paths, history and allowed transitions."
    : "Added after the general prompt for this step. Changes apply to runs that start from now on.";
  dialog(title, [h("p", { class: "help" }, help), ta], () => api("PUT", `/api/prompts/${id}`, { text: ta.value }));
}

function addDialog() {
  const ta = h("textarea", { rows: 10, placeholder: "https://github.com/owner/repo/issues/123\n#123\n\n…or paste the issue text (first line becomes the title)" });
  dialog("Add work item", [
    h("p", { class: "help" }, "A GitHub issue URL or number is fetched with gh. Anything else is used as the issue text."),
    ta,
  ], () => api("POST", "/api/items", { text: ta.value }), "Add");
  setTimeout(() => ta.focus(), 0);
}

// Workspace ----------------------------------------------------------------

/// Folder picker backed by the server's /api/fs listing.
/// A single click selects a folder (calls `onSelect`), a double click enters it.
function folderBrowser(onSelect) {
  const head = h("div", { class: "browser-head" });
  const list = h("ul", {});
  const el = h("div", { class: "browser" }, head, list);
  let hidden = false;
  let current = "";
  let selected = null;
  const select = (path, li) => {
    selected = path;
    list.querySelectorAll("li.sel").forEach((x) => x.classList.remove("sel"));
    li.classList.add("sel");
    onSelect(path);
  };
  const go = async (path) => {
    let res;
    try {
      res = await api("GET", `/api/fs?path=${encodeURIComponent(path || "")}&hidden=${hidden}`);
    } catch (e) {
      $("#dialog-error").textContent = e.message;
      return;
    }
    $("#dialog-error").textContent = "";
    current = res.path;
    head.replaceChildren(
      h("button", { class: "small", disabled: !res.parent, onclick: () => go(res.parent), title: "Parent folder" }, "↑"),
      h("button", { class: "small", onclick: () => go(""), title: "Home folder" }, "⌂"),
      h("span", { class: "cur", title: res.path }, res.path, res.git ? " " : "", res.git ? h("span", { class: "badge" }, "git") : ""),
      h("label", { style: "margin:0;display:inline-flex;gap:4px;align-items:center" },
        h("input", { type: "checkbox", checked: hidden, onchange: (e) => { hidden = e.target.checked; go(current); } }), "hidden"),
    );
    list.replaceChildren(...(res.entries.length ? res.entries : [{ name: "(no subfolders)", path: null }]).map((e) =>
      h("li", {
        class: e.path && e.path === selected ? "sel" : null,
        onclick: (ev) => e.path && select(e.path, ev.currentTarget),
        ondblclick: () => e.path && go(e.path),
      },
        h("span", {}, e.path ? "📁 " : "", e.name), e.git ? h("span", { class: "badge" }, "git") : "")));
  };
  return { el, go };
}

function workspaceDialog() {
  const repo = h("input", { type: "text", value: S.repo ?? "", placeholder: "/path/to/your/repository" });
  const data = h("input", { type: "text", value: S.data && S.data !== S.defaultData ? S.data : "" });
  const updatePlaceholder = () => (data.placeholder = repo.value.trim() ? `default: ${repo.value.trim().replace(/\/$/, "")}/.claudechain` : "default: <repository>/.claudechain");
  repo.oninput = updatePlaceholder;
  updatePlaceholder();
  // Clicking a folder fills the most recently focused input (Repository by default).
  let target = repo;
  const setTarget = (input) => {
    target = input;
    repo.classList.toggle("target", input === repo);
    data.classList.toggle("target", input === data);
  };
  repo.addEventListener("focus", () => setTarget(repo));
  data.addEventListener("focus", () => setTarget(data));
  setTarget(repo);
  const browser = folderBrowser((path) => {
    target.value = path;
    updatePlaceholder();
  });
  const recent = S.recent.length
    ? h("div", {}, h("label", {}, "Recent"), h("div", { class: "recent" }, S.recent.map((r) =>
        h("button", { onclick: () => { repo.value = r.repo; data.value = r.data === `${r.repo}/.claudechain` ? "" : r.data; updatePlaceholder(); } },
          r.repo, h("small", {}, `data: ${r.data}`)))))
    : "";
  dialog("Open repository", [
    h("p", { class: "help" }, "ClaudeChain works on one git repository at a time. Its pipeline, prompts, work items and worktrees are kept in the data directory, so each repository keeps its own setup. Switching is only possible while no agents are running."),
    recent,
    browser.el,
    h("label", {}, "Repository"), h("div", { class: "field" }, repo),
    h("label", {}, "Data directory (optional)"), h("div", { class: "field" }, data),
  ], () => api("POST", "/api/workspace", { repo: repo.value, data: data.value || null }), "Open", true);
  browser.go(S.repo ? S.repo.replace(/\/[^/]+\/?$/, "") || "/" : "");
}

// Pipeline editor ----------------------------------------------------------

const KNOWN_TOOLS = ["Read", "Grep", "Glob", "Edit", "Write", "Bash", "WebFetch", "WebSearch", "NotebookEdit", "TodoWrite", "Agent"];
const PERMISSION_MODES = ["dontAsk", "acceptEdits", "bypassPermissions", "auto", "plan"];

function slug(name) {
  return name.toLowerCase().trim().replace(/[^a-z0-9]+/g, "_").replace(/^_+|_+$/g, "") || "step";
}

// Preset stages offered by "Choose default stage" in the pipeline editor. Picking one
// overwrites every setting of the step. Targets naming steps that are not in the
// pipeline are dropped. The ids match the default prompts written for a new data dir.
const READ_TOOLS = ["Read", "Grep", "Glob", "Bash", "Write"];
const EDIT_TOOLS = [...READ_TOOLS, "Edit"];
const STEP_PRESETS = [
  { id: "triage", name: "Triage & Context", model: "claude-haiku-5-5", max_workers: 2, require_approval: false,
    allowed_targets: ["next", "human"], tools: READ_TOOLS, permission_mode: "dontAsk",
    desc: "Checks the issue is actionable and collects files, snippets and test status for the implementer." },
  { id: "breakdown", name: "Work breakdown", model: "claude-opus-5-5", max_workers: 2, require_approval: false,
    allowed_targets: ["next", "triage", "human"], tools: READ_TOOLS, permission_mode: "dontAsk",
    desc: "Turns the issue into a concrete step-by-step implementation plan." },
  { id: "implementation", name: "Implementation", model: "claude-opus-5-5", max_workers: 3, require_approval: false,
    allowed_targets: ["next", "triage", "human"], tools: EDIT_TOOLS, permission_mode: "dontAsk",
    desc: "Writes the code change and its tests." },
  { id: "build_test", name: "Build & test", model: "claude-haiku-5-5", max_workers: 1, require_approval: false,
    allowed_targets: ["next", "implementation", "human"], tools: READ_TOOLS, permission_mode: "dontAsk",
    desc: "Builds, lints and runs the tests; sends failures back to Implementation." },
  { id: "review", name: "Code review", model: "claude-opus-5-5", max_workers: 2, require_approval: false,
    allowed_targets: ["next", "implementation", "human"], tools: READ_TOOLS, permission_mode: "dontAsk",
    desc: "Reviews the diff like a senior engineer before it is published." },
  { id: "security_audit", name: "Security audit", model: "claude-opus-5-5", max_workers: 1, require_approval: false,
    allowed_targets: ["next", "implementation", "human"], tools: READ_TOOLS, permission_mode: "dontAsk",
    desc: "Looks for injection, secrets, unsafe input handling and risky dependencies." },
  { id: "docs", name: "Documentation", model: "claude-sonnet-5-5", max_workers: 1, require_approval: false,
    allowed_targets: ["next", "human"], tools: EDIT_TOOLS, permission_mode: "dontAsk",
    desc: "Updates README, docs and code comments to match the change." },
  { id: "changelog", name: "Changelog", model: "claude-haiku-5-5", max_workers: 1, require_approval: false,
    allowed_targets: ["next", "human"], tools: EDIT_TOOLS, permission_mode: "dontAsk",
    desc: "Adds a user-facing changelog entry for the change." },
  { id: "pull_request", name: "Pull request", model: "claude-haiku-5-5", max_workers: 1, require_approval: true,
    allowed_targets: ["next", "implementation", "human"], tools: READ_TOOLS, permission_mode: "dontAsk",
    desc: "Pushes the branch and opens a pull request (after your approval)." },
];

// Small modal on top of the pipeline dialog listing STEP_PRESETS; calls onPick(preset).
function presetPicker(onPick) {
  const dlg = h("dialog", { class: "preset-picker" },
    h("h2", {}, "Choose default stage"),
    h("p", { class: "help" }, "Replaces the name, model, workers, approval, permissions, transitions and tools of this step. Transitions to steps that are not in the pipeline are left out."),
    h("div", { class: "preset-list" }, STEP_PRESETS.map((p) =>
      h("button", { onclick: () => { dlg.close(); onPick(p); } },
        h("strong", {}, p.name),
        h("small", {}, `${p.model} · ${p.max_workers} worker${p.max_workers === 1 ? "" : "s"}${p.require_approval ? " · approval" : ""} · ${p.tools.join(", ")}`),
        h("span", {}, p.desc)))),
    h("div", { class: "preset-actions" }, h("button", { onclick: () => dlg.close() }, "Cancel")),
  );
  dlg.addEventListener("close", () => dlg.remove());
  document.body.append(dlg);
  dlg.showModal();
}

function pipelineDialog() {
  const draft = structuredClone({
    max_iterations: S.pipeline.max_iterations,
    models: S.pipeline.models,
    steps: S.pipeline.steps,
  });
  const body = h("div", {});

  const uniqueId = (base, self) => {
    let id = base, n = 2;
    while (draft.steps.some((s) => s !== self && s.id === id)) id = `${base}_${n++}`;
    return id;
  };
  const renameId = (step, next) => {
    for (const s of draft.steps) s.allowed_targets = s.allowed_targets.map((t) => (t === step.id ? next : t));
    step.id = next;
  };
  const check = (label, checked, onchange, title) =>
    h("label", { title }, h("input", { type: "checkbox", checked, onchange: (e) => onchange(e.target.checked) }), label);
  const toggle = (list, value, on) => {
    const i = list.indexOf(value);
    if (on && i < 0) list.push(value);
    if (!on && i >= 0) list.splice(i, 1);
  };
  // Overwrites a step with a preset. Existing steps keep their id (runs and messages
  // use it); new steps take the preset's id so they pick up its default prompt.
  const applyPreset = (step, p) => {
    step.name = p.name;
    step.model = p.model;
    step.effort = p.effort ?? null;
    step.max_workers = p.max_workers;
    step.require_approval = p.require_approval;
    step.permission_mode = p.permission_mode;
    step.tools = [...p.tools];
    if (step._new) {
      step._idEdited = false;
      renameId(step, uniqueId(p.id, step));
    }
    step.allowed_targets = p.allowed_targets.filter((t) =>
      t === "next" || t === "human" || (t !== step.id && draft.steps.some((s) => s.id === t)));
    render();
  };

  const card = (step, i) => {
    const last = i === draft.steps.length - 1;
    const models = [...new Set([...draft.models, step.model])];
    const extraTools = step.tools.filter((t) => !KNOWN_TOOLS.includes(t));
    const nameInput = h("input", { type: "text", class: "name", value: step.name, placeholder: "Step name" });
    const idInput = step._new ? h("input", { type: "text", class: "id", value: step.id, title: "Step id: used in file names and transitions" }) : null;
    nameInput.oninput = () => {
      step.name = nameInput.value;
      if (step._new && !step._idEdited) {
        renameId(step, uniqueId(slug(step.name), step));
        idInput.value = step.id;
      }
    };
    nameInput.onchange = render; // refresh transition labels in other cards
    if (idInput) idInput.onchange = () => { step._idEdited = true; renameId(step, idInput.value.trim()); render(); };

    return h("div", { class: "step-card" },
      h("div", { class: "top" },
        h("span", { class: "num" }, i + 1),
        nameInput,
        idInput ?? h("code", { title: "Step id (fixed once created: runs and messages use it)" }, step.id),
        h("span", { class: "spacer" }),
        h("button", { class: "small", title: "Fill in this step from a list of preset stages", onclick: () => presetPicker((p) => applyPreset(step, p)) }, "Choose default stage"),
        h("button", { class: "small", disabled: i === 0, title: "Move up", onclick: () => { draft.steps.splice(i - 1, 0, ...draft.steps.splice(i, 1)); render(); } }, "↑"),
        h("button", { class: "small", disabled: last, title: "Move down", onclick: () => { draft.steps.splice(i + 1, 0, ...draft.steps.splice(i, 1)); render(); } }, "↓"),
        h("button", { class: "small danger", title: "Remove step", onclick: () => {
          draft.steps.splice(i, 1);
          for (const s of draft.steps) s.allowed_targets = s.allowed_targets.filter((t) => t !== step.id);
          render();
        } }, "✕"),
      ),
      h("div", { class: "grid" },
        h("span", { class: "lbl" }, "Model"),
        h("div", { class: "inline" },
          h("select", { onchange: (e) => (step.model = e.target.value) }, models.map((m) => h("option", { value: m, selected: m === step.model }, m))),
          h("label", { title: "Thinking level (claude --effort)" }, "effort",
            h("select", { onchange: (e) => (step.effort = e.target.value || null) }, effortOptions(step.effort, "default"))),
          h("label", {}, "workers", h("input", { type: "number", min: 0, max: 32, value: step.max_workers, onchange: (e) => (step.max_workers = Math.max(0, parseInt(e.target.value, 10) || 0)) })),
          check("requires approval", step.require_approval, (v) => (step.require_approval = v)),
          h("label", { title: "claude --permission-mode" }, "permissions",
            h("select", { onchange: (e) => (step.permission_mode = e.target.value) },
              [...new Set([...PERMISSION_MODES, step.permission_mode])].map((m) => h("option", { value: m, selected: m === step.permission_mode }, m)))),
        ),
        h("span", { class: "lbl" }, "Transitions"),
        h("div", { class: "checks" },
          check(last ? "Done" : `→ ${draft.steps[i + 1].name}`, step.allowed_targets.includes("next"), (v) => toggle(step.allowed_targets, "next", v),
            last ? "Allow finishing the item" : `Allow passing the item on to ${draft.steps[i + 1].name}`),
          check("Human", step.allowed_targets.includes("human"), (v) => toggle(step.allowed_targets, "human", v), "Allow stopping and asking the human for help"),
          // The following step is already covered by the box above; list it separately only if it is explicitly allowed by id.
          draft.steps.filter((s, j) => s !== step && (j !== i + 1 || step.allowed_targets.includes(s.id))).map((s) =>
            check(`${s.name}`, step.allowed_targets.includes(s.id), (v) => toggle(step.allowed_targets, s.id, v), `Allow sending the item to ${s.name} (${s.id})`)),
        ),
        h("span", { class: "lbl" }, "Tools"),
        h("div", { class: "checks" },
          KNOWN_TOOLS.map((t) => check(t, step.tools.includes(t), (v) => toggle(step.tools, t, v))),
          h("input", {
            type: "text", value: extraTools.join(", "), placeholder: "other tools, comma-separated", style: "width:220px",
            onchange: (e) => {
              const extra = e.target.value.split(",").map((x) => x.trim()).filter(Boolean);
              step.tools = [...step.tools.filter((t) => KNOWN_TOOLS.includes(t)), ...extra];
            },
          }),
        ),
      ),
    );
  };

  function render() {
    const scroll = $("#dialog-body").scrollTop;
    body.replaceChildren(
      h("div", { class: "pl-general" },
        h("button", {
          type: "button",
          title: "Edit the general prompt shared by every step (closes this dialog; unsaved pipeline changes are lost)",
          onclick: () => editPrompt("general", "General prompt"),
        }, "General prompt…"),
      ),
      h("div", { class: "pl-settings" },
        h("div", {}, h("label", {}, "Max backward moves per item"),
          h("input", { type: "number", min: 0, max: 100, value: draft.max_iterations, onchange: (e) => (draft.max_iterations = parseInt(e.target.value, 10) || 0) })),
        h("div", {}, h("label", {}, "Models offered in dropdowns (comma-separated)"),
          h("input", { type: "text", value: draft.models.join(", "), onchange: (e) => { draft.models = e.target.value.split(",").map((x) => x.trim()).filter(Boolean); render(); } })),
      ),
      h("p", { class: "help" }, "Steps run left to right. Under Transitions, “→ step” passes an item on to the following step (“Done” on the last step finishes it), “Human” stops and asks you, and the other boxes are the steps an agent may send work to, e.g. back to Implementation when a build fails. New steps get a starter prompt you can edit from the column header. A step can only be removed when no unfinished item is in it."),
      ...draft.steps.map(card),
      h("button", { onclick: () => {
        const prev = draft.steps[draft.steps.length - 1];
        const step = {
          id: uniqueId("new_step"), name: "New step", model: prev?.model || draft.models[0] || "claude-sonnet-5-5",
          max_workers: 1, require_approval: false, allowed_targets: ["next", "human"],
          tools: ["Read", "Grep", "Glob", "Bash", "Write"], permission_mode: "dontAsk", _new: true,
        };
        draft.steps.push(step);
        render();
        $("#dialog-body").scrollTop = $("#dialog-body").scrollHeight;
      } }, "+ Add step"),
    );
    $("#dialog-body").scrollTop = scroll;
  }

  dialog("Pipeline", [body], () => {
    const steps = draft.steps.map(({ _new, _idEdited, ...s }) => s);
    return api("PUT", "/api/pipeline", { ...draft, steps });
  }, "Save pipeline", true);
  render();
}

function moveDialog(item, back = false) {
  // "Send back" defaults to the step before the current one.
  const cur = stepIndex(item.step);
  const def = back && cur > 0 ? S.pipeline.steps[cur - 1].id : item.step;
  const sel = h("select", {},
    S.pipeline.steps.map((s) => h("option", { value: s.id, selected: s.id === def }, s.name)),
    h("option", { value: "done" }, "Done (finish the item)"));
  const note = h("textarea", { rows: 4, placeholder: "Optional note for the agent of that step" });
  dialog(`Move ${item.id}`, [h("label", {}, "Move to step"), sel, h("label", {}, "Note"), note],
    () => api("POST", `/api/items/${item.id}/move`, { step: sel.value, note: note.value || null }), "Move");
}

// ---- Wiring --------------------------------------------------------------

function showPage(page) {
  S.page = page;
  $("#board-page").hidden = page !== "board";
  $("#resources-page").hidden = page !== "resources";
  for (const b of document.querySelectorAll("#pages button")) b.classList.toggle("active", b.dataset.page === page);
  if (page !== "board") closeInspector();
  if (location.hash !== (page === "board" ? "" : `#${page}`)) history.replaceState(null, "", page === "board" ? location.pathname : `#${page}`);
  if (page === "resources") startResources();
  else stopResources();
}
$("#pages").onclick = (e) => e.target.dataset?.page && showPage(e.target.dataset.page);
$("#plan").onclick = () => showPage("resources");
window.addEventListener("hashchange", () => showPage(location.hash === "#resources" ? "resources" : "board"));

$("#btn-add").onclick = addDialog;
$("#repo").onclick = workspaceDialog;
$("#btn-open-repo").onclick = workspaceDialog;
$("#dialog-cancel").onclick = () => $("#dialog").close();
$("#insp-close").onclick = closeInspector;

// Inspector width: drag the left edge to resize; the width is kept in localStorage.
const INSP_WIDTH_KEY = "claudechain-inspector-width";
const INSP_MIN_WIDTH = 360;
function clampInspWidth(w) {
  return Math.round(Math.max(Math.min(INSP_MIN_WIDTH, window.innerWidth), Math.min(w, window.innerWidth)));
}
function applyInspWidth(w) {
  $("#inspector").style.width = w == null ? "" : clampInspWidth(w) + "px";
}
function savedInspWidth() {
  let v = null;
  try { v = parseInt(localStorage.getItem(INSP_WIDTH_KEY), 10); } catch {}
  return Number.isFinite(v) && v > 0 ? v : null;
}
applyInspWidth(savedInspWidth());
{
  const handle = $("#insp-resizer");
  handle.addEventListener("pointerdown", (e) => {
    if (e.button !== 0) return;
    e.preventDefault();
    handle.setPointerCapture(e.pointerId);
    handle.classList.add("dragging");
    document.body.classList.add("resizing");
  });
  handle.addEventListener("pointermove", (e) => {
    if (!handle.hasPointerCapture(e.pointerId)) return;
    applyInspWidth(window.innerWidth - e.clientX);
  });
  const end = (e) => {
    if (!handle.hasPointerCapture(e.pointerId)) return;
    handle.releasePointerCapture(e.pointerId);
    handle.classList.remove("dragging");
    document.body.classList.remove("resizing");
    const w = parseInt($("#inspector").style.width, 10);
    if (Number.isFinite(w)) try { localStorage.setItem(INSP_WIDTH_KEY, String(w)); } catch {}
  };
  handle.addEventListener("pointerup", end);
  handle.addEventListener("pointercancel", end);
  handle.addEventListener("dblclick", () => {
    try { localStorage.removeItem(INSP_WIDTH_KEY); } catch {}
    applyInspWidth(null); // back to the CSS default min(920px, 100vw)
  });
}
window.addEventListener("resize", () => {
  const w = savedInspWidth();
  if (w != null) applyInspWidth(w); // re-clamp to the new window size
});

// Theme: follows the system until the user picks one with the toggle; the choice is kept in localStorage.
const THEME_KEY = "claudechain-theme";
const darkQuery = matchMedia("(prefers-color-scheme: dark)");
function savedTheme() {
  try { return localStorage.getItem(THEME_KEY); } catch { return null; }
}
function applyTheme(t) {
  document.documentElement.dataset.theme = t;
  const btn = $("#btn-theme");
  btn.textContent = t === "dark" ? "☀ Light mode" : "☾ Dark mode";
  btn.title = t === "dark" ? "Switch to light mode" : "Switch to dark mode";
  btn.setAttribute("aria-label", btn.title);
}
$("#btn-theme").onclick = () => {
  const t = document.documentElement.dataset.theme === "dark" ? "light" : "dark";
  try { localStorage.setItem(THEME_KEY, t); } catch {}
  applyTheme(t);
};
darkQuery.addEventListener("change", (e) => savedTheme() || applyTheme(e.matches ? "dark" : "light"));
applyTheme(document.documentElement.dataset.theme || (darkQuery.matches ? "dark" : "light"));

$("#insp-run").onchange = (e) => {
  S.insp.run = e.target.value ? parseInt(e.target.value, 10) : null;
  S.insp.follow = isLiveLatest(S.items.get(S.insp.id), S.insp.run);
  loadRun();
};
$("#insp-tabs").onclick = (e) => {
  const tab = e.target.dataset?.tab;
  if (!tab || !S.insp) return;
  S.insp.tab = tab;
  renderInspectorChrome();
  renderTab();
};
$("#insp-stop").onclick = () => act(() => api("POST", `/api/items/${S.insp.id}/stop`));
$("#insp-retry").onclick = () => act(() => api("POST", `/api/items/${S.insp.id}/retry`));
$("#insp-move").onclick = () => moveDialog(S.items.get(S.insp.id));
function removeItem(id) {
  if (confirm(`Remove ${id} from the board? Its folder is moved to archive/ and its worktree is removed (the branch is kept).`)) {
    act(() => api("DELETE", `/api/items/${id}`));
  }
}
$("#insp-remove").onclick = () => removeItem(S.insp.id);
$("#composer").onsubmit = (e) => e.preventDefault();
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && S.insp && !$("#dialog").open) closeInspector();
});
// Keep durations of running items ticking.
setInterval(() => {
  if ([...S.items.values()].some((i) => i.status === "running")) {
    scheduleBoard();
    if (S.insp) renderInspectorChrome();
  }
}, 5000);

// Load the state right away instead of waiting for the live connection, so the
// landing page or board shows up even when the event stream is slow to open.
load().catch((e) => {
  document.body.classList.remove("loading");
  toast(`Could not load state: ${e.message}`);
});
connect();
// resources.js loads after this script; switch pages once it has.
window.addEventListener("DOMContentLoaded", () => location.hash === "#resources" && showPage("resources"));
