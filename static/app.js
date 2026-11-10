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
  S.items = new Map((st.items || []).map((i) => [i.id, i]));
  $("#repo-path").textContent = st.repo ?? "no repository";
  $("#repo").title = st.repo ? `Repository ${st.repo}\nData ${st.data}\n\nClick to open another repository` : "Open a repository";
  for (const id of ["#btn-general", "#btn-pipeline", "#btn-add"]) $(id).disabled = !st.repo;
  $("#no-repo").hidden = !!st.repo;
  if (!st.repo) {
    $("#board").replaceChildren();
    $("#empty").hidden = true;
    closeInspector();
    if (!$("#dialog").open && !load.prompted) {
      load.prompted = true;
      workspaceDialog();
    }
    return;
  }
  renderBoard();
  if (S.insp) renderInspector();
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
  });
}

// ---- Board ---------------------------------------------------------------

function renderBoard() {
  const p = S.pipeline;
  if (!p) return;
  const board = $("#board");
  const items = [...S.items.values()].sort((a, b) => a.created.localeCompare(b.created));
  $("#empty").hidden = items.length > 0;

  const busy = {};
  for (const it of items) if (it.status === "running") busy[it.step] = (busy[it.step] || 0) + 1;

  const head = h("tr", {},
    h("th", { class: "corner" }, `${items.length} item${items.length === 1 ? "" : "s"}`,
      h("div", {}, `max ${p.max_iterations} bounces per item`),
      h("button", { class: "small", style: "margin-top:6px", onclick: pipelineDialog }, "Edit pipeline…")),
    p.steps.map((s) => stepHeader(s, busy[s.id] || 0)),
  );
  const rows = items.map((it) => h("tr", {},
    itemCell(it),
    p.steps.map((s) => stepCell(it, s)),
  ));
  board.replaceChildren(h("thead", {}, head), h("tbody", {}, rows));
}

function stepHeader(s, busy) {
  const models = [...new Set([...S.pipeline.models, s.model])];
  const put = (body) => act(() => api("PUT", `/api/steps/${s.id}`, body));
  return h("th", { class: "step-head" },
    h("div", { class: "name" }, s.name, h("span", { class: "busy" }, `${busy}/${s.max_workers} busy`)),
    h("div", { class: "row" },
      h("select", { title: "Model", onchange: (e) => put({ model: e.target.value }) },
        models.map((m) => h("option", { value: m, selected: m === s.model }, shortModel(m)))),
    ),
    h("div", { class: "row" },
      h("label", {}, "workers"),
      h("input", {
        type: "number", min: 0, max: 32, value: s.max_workers, title: "Maximum agents running this step at once (0 pauses the step)",
        onchange: (e) => put({ max_workers: Math.max(0, parseInt(e.target.value, 10) || 0) }),
      }),
      h("label", { title: "Items wait for your approval before this step runs" },
        h("input", { type: "checkbox", checked: s.require_approval, onchange: (e) => put({ require_approval: e.target.checked }) }),
        " approval"),
    ),
    h("div", { class: "row" },
      h("button", { class: "small", onclick: () => editPrompt(s.id, `${s.name} prompt`) }, "✎ prompt"),
      h("span", { title: "Allowed transitions" }, "→ " + s.allowed_targets.map((t) => (t === "next" || t === "human" ? t : stepName(t))).join(", ")),
    ),
  );
}

function itemCell(it) {
  const cost = it.runs.reduce((sum, r) => sum + (r.cost_usd || 0), 0);
  const last = it.runs[it.runs.length - 1];
  return h("td", { class: "item-cell" },
    h("div", { class: "title", onclick: () => openInspector(it.id, last?.n) }, it.number ? `#${it.number} ` : "", it.title || it.id),
    h("div", { class: "sub" },
      h("span", {}, it.id),
      it.url && h("a", { href: it.url, target: "_blank", rel: "noopener" }, "issue ↗"),
      it.pr_url && h("a", { href: it.pr_url, target: "_blank", rel: "noopener" }, "PR ↗"),
      it.iteration > 0 && h("span", { title: "Backward moves" }, `↩ ${it.iteration}`),
      cost > 0 && h("span", { title: "API-equivalent cost (covered by your subscription)" }, `≈ ${fmtCost(cost)}`),
    ),
    it.status === "done" && h("div", { class: `state done` }, STATUS_LABEL.done),
    it.pending_notes.length > 0 && h("div", { class: "detail" }, `📝 ${it.pending_notes.length} note(s) for the next run`),
  );
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
        title: `${shortModel(r.model)} · ${fmtDuration(r)}${r.outcome ? "\n" + r.outcome : ""}`,
        onclick: (e) => { e.stopPropagation(); openInspector(it.id, r.n); },
      }, runChipLabel(r)))));
  }
  if (current) {
    const cls = it.status;
    td.append(h("div", { class: `state ${cls}${cls === "running" ? " pulse" : ""}` }, STATUS_LABEL[cls]));
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

async function openInspector(id, run) {
  S.insp = { id, run: run ?? null, tab: S.insp?.tab || "transcript", data: null, file: null };
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

/// Called when the open item changes; reloads when a new run started.
function refreshInspectorFromItem(item) {
  const insp = S.insp;
  const last = item.runs[item.runs.length - 1];
  const viewingLatest = insp.run == null || insp.run === last?.n - 1 || insp.run === last?.n;
  if (last && insp.run !== last.n && viewingLatest && !last.ended) {
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
    run?.cost_usd != null && `≈ ${fmtCost(run.cost_usd)} API-equivalent`,
    run?.resumed_from && `continues #${run.resumed_from}`,
  ].filter(Boolean);
  $("#insp-meta").textContent = meta.join("  ·  ");
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
  body.replaceChildren(h("div", { class: "files" }, list, view));
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
function folderBrowser(onPick) {
  const head = h("div", { class: "browser-head" });
  const list = h("ul", {});
  const el = h("div", { class: "browser" }, head, list);
  let hidden = false;
  let current = "";
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
      h("button", { class: "small primary", onclick: () => onPick(res.path) }, "Use this folder"),
    );
    list.replaceChildren(...(res.entries.length ? res.entries : [{ name: "(no subfolders)", path: null }]).map((e) =>
      h("li", { onclick: () => e.path && go(e.path), ondblclick: () => e.path && onPick(e.path) },
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
  let target = repo;
  const browser = folderBrowser((path) => {
    target.value = path;
    updatePlaceholder();
  });
  const browse = (input) => h("button", { onclick: () => { target = input; browser.go(input.value.trim() || (input === data ? repo.value.trim() : "")); } }, "Browse…");
  const recent = S.recent.length
    ? h("div", {}, h("label", {}, "Recent"), h("div", { class: "recent" }, S.recent.map((r) =>
        h("button", { onclick: () => { repo.value = r.repo; data.value = r.data === `${r.repo}/.claudechain` ? "" : r.data; updatePlaceholder(); } },
          r.repo, h("small", {}, `data: ${r.data}`)))))
    : "";
  dialog("Open repository", [
    h("p", { class: "help" }, "ClaudeChain works on one git repository at a time. Its pipeline, prompts, work items and worktrees are kept in the data directory, so each repository keeps its own setup. Switching is only possible while no agents are running."),
    recent,
    h("label", {}, "Repository"), h("div", { class: "field" }, repo, browse(repo)),
    h("label", {}, "Data directory (optional)"), h("div", { class: "field" }, data, browse(data)),
    browser.el,
  ], () => api("POST", "/api/workspace", { repo: repo.value, data: data.value || null }), "Open", true);
  browser.go(S.repo ? S.repo.replace(/\/[^/]+\/?$/, "") || "/" : "");
}

// Pipeline editor ----------------------------------------------------------

const KNOWN_TOOLS = ["Read", "Grep", "Glob", "Edit", "Write", "Bash", "WebFetch", "WebSearch", "NotebookEdit", "TodoWrite", "Agent"];
const PERMISSION_MODES = ["dontAsk", "acceptEdits", "bypassPermissions", "auto", "plan"];

function slug(name) {
  return name.toLowerCase().trim().replace(/[^a-z0-9]+/g, "_").replace(/^_+|_+$/g, "") || "step";
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
          h("label", {}, "workers", h("input", { type: "number", min: 0, max: 32, value: step.max_workers, onchange: (e) => (step.max_workers = Math.max(0, parseInt(e.target.value, 10) || 0)) })),
          check("requires approval", step.require_approval, (v) => (step.require_approval = v)),
          h("label", { title: "claude --permission-mode" }, "permissions",
            h("select", { onchange: (e) => (step.permission_mode = e.target.value) },
              [...new Set([...PERMISSION_MODES, step.permission_mode])].map((m) => h("option", { value: m, selected: m === step.permission_mode }, m)))),
        ),
        h("span", { class: "lbl" }, "Transitions"),
        h("div", { class: "checks" },
          check(last ? "next (= done)" : `next (→ ${draft.steps[i + 1].name})`, step.allowed_targets.includes("next"), (v) => toggle(step.allowed_targets, "next", v)),
          check("human", step.allowed_targets.includes("human"), (v) => toggle(step.allowed_targets, "human", v)),
          draft.steps.filter((s) => s !== step).map((s) =>
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
      h("div", { class: "pl-settings" },
        h("div", {}, h("label", {}, "Max backward moves per item"),
          h("input", { type: "number", min: 0, max: 100, value: draft.max_iterations, onchange: (e) => (draft.max_iterations = parseInt(e.target.value, 10) || 0) })),
        h("div", {}, h("label", {}, "Models offered in dropdowns (comma-separated)"),
          h("input", { type: "text", value: draft.models.join(", "), onchange: (e) => { draft.models = e.target.value.split(",").map((x) => x.trim()).filter(Boolean); render(); } })),
      ),
      h("p", { class: "help" }, "Steps run left to right. “next” moves an item to the following step (or finishes it after the last step); the other boxes are the steps an agent may send work to, e.g. back to Implementation when a build fails. New steps get a starter prompt you can edit from the column header. A step can only be removed when no unfinished item is in it."),
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

$("#btn-add").onclick = addDialog;
$("#btn-general").onclick = () => editPrompt("general", "General prompt");
$("#btn-pipeline").onclick = pipelineDialog;
$("#repo").onclick = workspaceDialog;
$("#btn-open-repo").onclick = workspaceDialog;
$("#dialog-cancel").onclick = () => $("#dialog").close();
$("#insp-close").onclick = closeInspector;
$("#insp-run").onchange = (e) => {
  S.insp.run = e.target.value ? parseInt(e.target.value, 10) : null;
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
$("#insp-remove").onclick = () => {
  const id = S.insp.id;
  if (confirm(`Remove ${id} from the board? Its folder is moved to archive/ and its worktree is removed (the branch is kept).`)) {
    act(() => api("DELETE", `/api/items/${id}`));
  }
};
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

connect();
