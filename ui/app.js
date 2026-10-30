// Rose web shell: chat-first. Buttons that need M2/T1.4 stay disabled.
const DOT = "inline-block w-3 h-3 rounded-full ";
const threadEl = () => document.getElementById("live-tail");

function bubble(html, mine) {
  const d = document.createElement("div");
  if (mine) {
    d.className = "bubble-mine self-end max-w-[85%] px-[2%] py-2 rounded-2xl rounded-br-sm";
    d.textContent = html;
  } else {
    d.className = "self-start max-w-[90%] flex gap-2";
    d.innerHTML = `<span class="flex-none">🌌</span><div class="bubble-rose px-[2%] py-2 rounded-2xl rounded-tl-sm">${html}</div>`;
  }
  threadEl().appendChild(d);
  d.scrollIntoView({ block: "nearest" });
}

function approvalCard(say, goal, chatId) {
  const d = document.createElement("div");
  d.className = "self-start w-full max-w-[90%] flex gap-2";
  d.innerHTML = `<span class="flex-none text-amber-500 font-bold text-lg">●</span>
    <div class="flex-1 p-[4%] rounded-2xl rounded-tl-sm border-2 border-amber-500 bg-amber-50 dark:bg-amber-950">
      <div class="font-semibold text-sm">Needs approval — ${say}</div>
      <div class="flex flex-wrap gap-2 mt-2">
        <button class="px-4 py-1.5 rounded bg-green-700 text-white font-semibold">Approve once</button>
        <button class="px-4 py-1.5 rounded bg-green-900 text-white font-semibold">Approve all — don't ask again</button>
        <button class="px-4 py-1.5 rounded bg-red-700 text-white font-semibold">Deny</button>
      </div>
    </div>`;
  const [approve, approveAll, deny] = d.querySelectorAll("button");
  approve.onclick = async () => {
    approve.disabled = true;
    approveAll.disabled = true;
    await runGoal(goal, true, false, d, chatId);
  };
  approveAll.onclick = async () => {
    approve.disabled = true;
    approveAll.disabled = true;
    await runGoal(goal, true, true, d, chatId);
  };
  deny.onclick = () => {
    d.querySelector("div div").innerHTML += `<div class="text-sm text-neutral-500 mt-1">Denied — nothing acted on.</div>`;
    approve.disabled = true;
    deny.disabled = true;
  };
  threadEl().appendChild(d);
  d.scrollIntoView({ block: "nearest" });
}

// ---- live run trace -------------------------------------------------------
// /api/step streams NDJSON events as the loop runs (observed -> planned ->
// acted -> result -> ... -> final). They are rendered into one card per
// run, updated in place, so the chat shows what Rose sees, what it plans,
// what each action did, and what is fed back to the brain — as it happens,
// including exactly where a run fails. All model text goes through
// textContent: it is untrusted output, never HTML.

function line(parent, text, cls) {
  const d = document.createElement("div");
  d.className = cls || "text-sm";
  d.textContent = text;
  parent.appendChild(d);
  d.scrollIntoView({ block: "nearest" });
  return d;
}

/// Short assistant reply bubble (the concise answer; the run card below
/// it is only the expandable detail). Text-only: model output is untrusted.
function sayBubble(text) {
  const d = document.createElement("div");
  d.className = "self-start max-w-[90%] flex gap-2";
  const icon = document.createElement("span");
  icon.className = "flex-none";
  icon.textContent = "🌌";
  const body = document.createElement("div");
  body.className =
    "bubble-rose px-[2%] py-2 rounded-2xl rounded-tl-sm";
  body.textContent = text;
  d.append(icon, body);
  threadEl().appendChild(d);
  d.scrollIntoView({ block: "nearest" });
}

function shorten(text, max) {
  const t = String(text || "").trim();
  return t.length <= max ? t : `${t.slice(0, max - 1)}…`;
}

function createRunCard(goal) {
  const wrap = document.createElement("div");
  wrap.className = "self-start w-full max-w-[90%] flex gap-2";
  const card = document.createElement("div");
  card.className =
    "bubble-rose flex-1 px-[2%] py-2 rounded-2xl rounded-tl-sm";
  const head = document.createElement("div");
  head.className = "font-semibold text-sm flex items-center gap-2";
  const dot = document.createElement("span");
  dot.className = "inline-block w-2.5 h-2.5 rounded-full bg-blue-600 animate-pulse flex-none";
  const title = document.createElement("span");
  title.textContent = "Rose is working…";
  const elapsed = document.createElement("span");
  elapsed.className = "text-xs font-normal opacity-60";
  const spacer = document.createElement("span");
  spacer.className = "flex-1";
  // Minimize/expand the whole feedback trace. It stays live while the
  // run is going; when the run ends the concise reply bubble is the
  // answer and the trace auto-minimizes (unless the user already
  // chose a state themselves) so the chat stays readable.
  const toggle = document.createElement("button");
  toggle.className =
    "px-2 py-0.5 rounded-lg btn-ghost text-xs font-medium flex-none";
  toggle.type = "button";
  head.append(dot, title, elapsed, spacer, toggle);
  const steps = document.createElement("div");
  steps.className = "flex flex-col gap-2 mt-2";
  card.append(head, steps);
  wrap.appendChild(card);
  threadEl().appendChild(wrap);
  wrap.scrollIntoView({ block: "nearest" });

  const startedAt = Date.now();
  const timer = setInterval(() => {
    elapsed.textContent = `${Math.round((Date.now() - startedAt) / 1000)}s`;
  }, 500);
  const rows = new Map();
  let latestStep = -1;
  const setRowCollapsed = (row, collapsed) => {
    // First child is the step header; it always stays visible.
    row.childNodes.forEach((child, i) => {
      if (i > 0) child.classList.toggle("hidden", collapsed);
    });
  };
  const cardApi = {
    stepsEl: steps,
    touched: false,
    setMinimized(min) {
      steps.classList.toggle("hidden", min);
      toggle.textContent = min ? "▸ Details" : "▾ Details";
      toggle.title = min ? "Show step-by-step feedback" : "Hide step-by-step feedback";
      toggle.setAttribute("aria-expanded", String(!min));
    },
    row(step) {
      if (!rows.has(step)) {
        const row = document.createElement("div");
        row.className =
          "border-l-2 border-neutral-300 dark:border-neutral-600 pl-2 flex flex-col gap-0.5";
        const headRow = document.createElement("div");
        headRow.className = "text-xs font-semibold uppercase tracking-wide opacity-60 cursor-pointer select-none";
        headRow.textContent = `Step ${step + 1}`;
        headRow.title = "Show/hide this step";
        headRow.onclick = () => {
          const bodyHidden = row.childNodes[1]?.classList.contains("hidden") ?? false;
          setRowCollapsed(row, !bodyHidden);
        };
        row.appendChild(headRow);
        steps.appendChild(row);
        rows.set(step, row);
      }
      if (step > latestStep) {
        // New latest step: it stays open, older rows collapse to headers.
        latestStep = step;
        rows.forEach((r, s) => setRowCollapsed(r, s !== step));
      }
      return rows.get(step);
    },
    finish(text, ok) {
      clearInterval(timer);
      elapsed.textContent = `${Math.round((Date.now() - startedAt) / 1000)}s total`;
      dot.className =
        "inline-block w-2.5 h-2.5 rounded-full flex-none " + (ok ? "bg-green-600" : "bg-red-600");
      title.textContent = text;
    },
  };
  toggle.onclick = () => {
    cardApi.touched = true;
    cardApi.setMinimized(!steps.classList.contains("hidden"));
  };
  cardApi.setMinimized(false);
  return cardApi;
}

function renderEvent(card, ev) {
  const row = ev.step != null ? card.row(ev.step) : card.stepsEl;
  switch (ev.type) {
    case "run_started":
      line(row, `▶ Run started — goal: ${ev.goal} (budget ${ev.max_steps} steps)`, "text-sm font-medium");
      break;
    case "observing":
      line(row, "👀 Taking a screenshot…", "text-sm opacity-70");
      break;
    case "observed": {
      const win = ev.active_window ? ` · active window: “${ev.active_window}”` : "";
      line(row, `👀 Saw ${ev.width}×${ev.height} screenshot (${ev.frame_bytes} bytes)${win} — the gridded copy below is what the brain gets`, "text-sm");
      const hist = ev.history && ev.history.length ? ev.history.join(" → ") : "(empty — first step)";
      line(row, `↩ Fed back to brain: gridded screenshot + history: ${hist}`, "text-xs opacity-70");
      break;
    }
    case "thinking":
      line(
        row,
        ev.mode === "text"
          ? "🧠 Thinking (text/shell step — no screenshot taken or sent)…"
          : "🧠 Thinking — screenshot + goal + history sent to the brain…",
        "text-sm opacity-70"
      );
      break;
    case "vision_requested":
      line(row, `👀 Brain needs the desktop for this: ${ev.say} — taking a screenshot…`, "text-sm");
      break;
    case "grounded": {
      const n = ev.elements || 0;
      const ms = ev.ms != null ? ` in ${ev.ms}ms` : "";
      const src = ev.source ? ` via ${ev.source}` : "";
      if (n > 0) {
        line(row, `🎯 Numbered 64×36 grid applied${ms}${src}, plus ${n} listed element${n === 1 ? "" : "s"} — brain aims by grid cell, element number, or pixels off the axes`, "text-sm");
        if (ev.labels && ev.labels.length) {
          line(row, ev.labels.join(" · "), "text-xs font-mono opacity-80");
        }
      } else {
        line(row, `🎯 Numbered 64×36 grid applied${ms}${src} — brain aims by grid cell g<col>,<row> or pixels off the axes (no raw screenshot is ever sent)`, "text-sm");
      }
      if (ev.frame && ev.run_id) {
        const a = document.createElement("a");
        a.href = `/api/runs/${encodeURIComponent(ev.run_id)}/frames/${ev.step}/grounded`;
        a.target = "_blank";
        a.rel = "noopener";
        a.className = "text-xs underline opacity-80";
        a.textContent = "🎯 View the gridded screenshot the brain saw";
        row.appendChild(a);
      }
      break;
    }
    case "planned":
      line(row, `🧠 Plan: ${ev.say}`, "text-sm font-medium");
      line(row, `→ Will do: ${ev.action} (brain took ${ev.brain_ms}ms)`, "text-sm font-mono");
      break;
    case "acting":
      line(row, `▶ Doing: ${ev.action}…`, "text-sm");
      break;
    case "acted":
      line(row, `✓ Done: ${ev.action} dispatched in ${ev.ms}ms — ${ev.note}`, "text-sm text-green-700 dark:text-green-400");
      break;
    case "result": {
      const win =
        ev.active_window_before || ev.active_window_after
          ? ` · window: “${ev.active_window_before || "none"}” → “${ev.active_window_after || "none"}”`
          : "";
      const settled = ev.settle_ms != null ? ` · settled in ${ev.settle_ms}ms` : "";
      line(
        row,
        `${ev.screen_changed ? "🖥 Result: screen changed" : "🖥 Result: screen did NOT change"}${win}${settled}`,
        `text-sm font-medium ${ev.screen_changed ? "" : "text-amber-600 dark:text-amber-400"}`
      );
      line(row, `↩ Feedback loop: ${ev.feedback}`, "text-xs opacity-70");
      break;
    }
    case "shell_result": {
      const status = ev.timed_out
        ? "⏱ timed out and was killed"
        : `exit ${ev.exit_code == null ? "?" : ev.exit_code}`;
      line(row, `$ ${ev.command} → ${status} (${ev.ms}ms)`, "text-sm font-mono font-medium");
      if (ev.stdout) line(row, ev.stdout, "text-xs font-mono whitespace-pre-wrap opacity-80");
      if (ev.stderr) line(row, `stderr: ${ev.stderr}`, "text-xs font-mono whitespace-pre-wrap text-amber-600 dark:text-amber-400");
      line(row, "↩ Feedback loop: this output is in the history fed back to the brain", "text-xs opacity-70");
      break;
    }
    case "stopped":
      line(row, `⏹ ${ev.say}`, "text-sm font-semibold text-red-600 dark:text-red-400");
      break;
    case "rejected":
      line(row, `🚫 Rejected repeat: ${ev.action} — ${ev.say}`, "text-sm font-semibold text-amber-600 dark:text-amber-400");
      break;
    case "decision":
      line(row, `🧠 Brain decided “${ev.status}”: ${ev.say} (brain took ${ev.brain_ms}ms)`, "text-sm font-medium");
      break;
    case "approval":
      line(row, `⚠️ Paused for approval: ${ev.say} — ${ev.action}`, "text-sm font-semibold text-amber-600 dark:text-amber-400");
      break;
    case "resumed":
      line(row, `▶ ${ev.say}`, "text-sm font-medium text-green-700 dark:text-green-400");
      break;
    case "auto_approved":
      line(row, `✅ Auto-approved (don't ask again): ${ev.say} — ${ev.action}`, "text-sm text-green-700 dark:text-green-400");
      break;
    case "lease":
      line(row, `⏸ ${ev.say}`, "text-sm font-medium text-amber-600 dark:text-amber-400");
      break;
    case "verify":
      line(row, `🔍 Verify: ${ev.say}`, "text-sm font-medium text-amber-600 dark:text-amber-400");
      break;
    case "stuck":
      line(row, `⚠️ Stuck: ${ev.say}`, "text-sm font-semibold text-amber-600 dark:text-amber-400");
      break;
    case "failed":
      line(
        row,
        `✗ Failed at ${ev.stage}${ev.action ? ` (${ev.action})` : ""}: ${ev.error}`,
        "text-sm font-semibold text-red-600 dark:text-red-400"
      );
      break;
    default:
      line(row, JSON.stringify(ev), "text-xs opacity-60");
  }
}

let running = false;
let activeCard = null;

/// Stop the in-flight run. The server sets a flag the loop polls at
/// every checkpoint (and while waiting on the brain/shell), so the
/// current atomic action may finish but nothing new starts.
async function stopRun() {
  const btn = document.getElementById("stop");
  btn.disabled = true;
  btn.textContent = "■ Stopping…";
  try {
    const r = await fetch("/api/stop", { method: "POST" });
    const j = await r.json().catch(() => ({}));
    if (r.ok && j.stopping) {
      if (activeCard) {
        line(activeCard.stepsEl, "⏹ Stop requested — finishing the current action, then stopping…", "text-sm font-semibold text-red-600 dark:text-red-400");
      }
    } else {
      bubble("No run is in progress to stop.");
      btn.textContent = "■ Stop";
    }
  } catch (e) {
    bubble(`⚠️ Stop failed: ${e.message}`);
    btn.textContent = "■ Stop";
    btn.disabled = !running;
  }
}

function setStopButton(active) {
  const btn = document.getElementById("stop");
  btn.disabled = !active;
  btn.textContent = "■ Stop";
}

async function runGoal(goal, force, autoApprove, approvalEl, chatId = selectedChat) {
  if (running) {
    bubble("⏳ A run is already in progress — watch it above, or stop it first.");
    return;
  }
  running = true;
  setStopButton(true);
  const card = createRunCard(goal);
  activeCard = card;
  let view = null;
  let liveChatId = chatId;
  try {
    const r = await fetch("/api/step", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ goal, force, auto_approve: autoApprove, chat_id: chatId }),
    });
    if (!r.ok) {
      const j = await r.json().catch(() => ({}));
      throw new Error(j.error || `HTTP ${r.status}`);
    }
    if (!r.body) throw new Error("streaming not supported by this browser");
    const reader = r.body.getReader();
    const decoder = new TextDecoder();
    let buf = "";
    const handle = (text) => {
      if (!text.trim()) return;
      const ev = JSON.parse(text);
      if (ev.chat_id) liveChatId = ev.chat_id;
      if (ev.type === "final") view = ev;
      else renderEvent(card, ev);
    };
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      buf += decoder.decode(value, { stream: true });
      let nl;
      while ((nl = buf.indexOf("\n")) >= 0) {
        handle(buf.slice(0, nl));
        buf = buf.slice(nl + 1);
      }
    }
    handle(buf);
  } catch (e) {
    // Failures keep the trace expanded: the detail is the point there.
    card.finish("Details — run failed", false);
    line(card.stepsEl, `⚠️ Step failed: ${e.message}`, "text-sm font-semibold text-red-600 dark:text-red-400");
    sayBubble(`⚠️ Couldn't finish: ${shorten(e.message, 200)}`);
    running = false;
    activeCard = null;
    setStopButton(false);
    return;
  }
  running = false;
  activeCard = null;
  setStopButton(false);
  presentFinal(card, view, goal, true);
  if (approvalEl) approvalEl.querySelectorAll("button").forEach((b) => (b.disabled = true));
  // The turn belongs to this chat (new or continued): keep it
  // selected in the sidebar and refresh its turn count/title.
  if (view && view.chat_id) liveChatId = view.chat_id;
  if (liveChatId) selectedChat = liveChatId;
  refreshChats();
}

/// Present a run's terminal view: concise chat reply + card header.
/// Live runs may raise an approval card; replayed (persisted) runs
/// never act again, so `interactive=false` only reports the pause.
function presentFinal(card, view, goal, interactive) {
  if (!view) {
    card.finish("Details — run ended without a result", false);
    sayBubble("⚠️ The run ended without a result — expand Details above for the trace.");
    return;
  }
  // The concise answer is a normal chat reply; the card is now just the
  // expandable detail behind it, so its header stops repeating the reply.
  const stepsNote = view.steps ? ` · ${view.steps} step${view.steps === 1 ? "" : "s"}` : "";
  if (view.stopped) {
    // Stopped runs keep the trace expanded: where it stopped matters.
    card.finish("Details — stopped by you", false);
    sayBubble(`⏹ Stopped.${stepsNote}`);
  } else if (view.error) {
    card.finish("Details — run failed", false);
    line(card.stepsEl, `⚠️ ${view.error}`, "text-sm font-semibold text-red-600 dark:text-red-400");
    sayBubble(`⚠️ Couldn't finish: ${shorten(view.error, 200)}`);
  } else if (view.approval_needed) {
    card.finish("Details — waiting for approval", true);
    if (!card.touched) card.setMinimized(true);
    if (interactive) approvalCard(view.say, goal, view.chat_id);
    else sayBubble(`⏸ Paused for approval: ${shorten(view.say, 200)}`);
  } else if (view.done) {
    card.finish("Details — done", true);
    if (!card.touched) card.setMinimized(true);
    sayBubble(`✅ ${shorten(view.say, 240) || "Done."}${stepsNote}`);
  } else if (view.acted) {
    card.finish("Details — finished", true);
    if (!card.touched) card.setMinimized(true);
    sayBubble(`${shorten(view.say, 240)}${stepsNote}`);
  } else {
    card.finish("Details — finished", true);
    if (!card.touched) card.setMinimized(true);
    sayBubble(shorten(view.say, 240));
  }
}

// ---- chats (one per persisted run) -----------------------------------------

let selectedChat = null;

const STATE_ICON = {
  done: "✅",
  finished: "✅",
  failed: "⚠️",
  stopped: "⏹",
  waiting_approval: "⏸",
  running: "▶️",
};

/// Fill the left sidebar from persisted chats, most recently
/// updated first (ChatGPT/Open WebUI style session list).
async function refreshChats() {
  const list = document.getElementById("chat-list");
  if (!list) return [];
  try {
    const r = await fetch("/api/chats?limit=50");
    const j = await r.json();
    const query = (document.getElementById("chat-search")?.value || "").toLowerCase();
    const chats = (j.chats || []).filter(
      (c) => !query || c.title.toLowerCase().includes(query)
    );
    list.replaceChildren();
    if (!chats.length) {
      line(
        list,
        query ? "No chats match your search." : "No chats yet — start one with ＋ New chat.",
        "text-xs text-neutral-500"
      );
      return chats;
    }
    for (const chat of chats) {
      const active = chat.chat_id === selectedChat;
      const item = document.createElement("div");
      item.className =
        "group w-full rounded-lg border flex items-start gap-1 px-[4%] py-2 " +
        (active
          ? "text-white"
          : "hover:bg-neutral-100 dark:hover:bg-neutral-800");
      if (active) item.style.background = "var(--accent)";
      else item.style.borderColor = "var(--line)";
      const main = document.createElement("button");
      main.type = "button";
      main.className = "flex-1 text-left flex flex-col gap-0.5 min-w-0";
      const title = document.createElement("span");
      title.className = "font-medium text-sm leading-snug";
      title.textContent = `${STATE_ICON[chat.state] || "•"} ${shorten(chat.title, 70)}`;
      const meta = document.createElement("span");
      meta.className = "text-xs " + (active ? "text-white/75" : "text-neutral-500");
      const when = chat.updated_at ? new Date(chat.updated_at).toLocaleString() : "";
      meta.textContent = `${chat.turns} turn${chat.turns === 1 ? "" : "s"} · ${chat.steps} steps · ${when}`;
      main.append(title, meta);
      main.onclick = () => openChat(chat.chat_id);
      const rename = document.createElement("button");
      rename.type = "button";
      rename.textContent = "✎";
      rename.title = "Rename chat";
      rename.className =
        "px-1 py-0.5 rounded text-xs flex-none " +
        (active ? "text-white" : "text-neutral-500 hover:text-neutral-800 dark:hover:text-neutral-200");
      rename.onclick = async (e) => {
        e.stopPropagation();
        const titleText = prompt("Rename chat:", chat.title);
        if (!titleText || !titleText.trim()) return;
        await fetch(`/api/chats/${encodeURIComponent(chat.chat_id)}`, {
          method: "PATCH",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ title: titleText.trim() }),
        });
        refreshChats();
      };
      const del = document.createElement("button");
      del.type = "button";
      del.textContent = "🗑";
      del.title = "Delete chat";
      del.className =
        "px-1 py-0.5 rounded text-xs flex-none " +
        (active ? "text-white" : "text-neutral-500 hover:text-red-600");
      del.onclick = async (e) => {
        e.stopPropagation();
        if (!confirm(`Delete chat “${chat.title}” and all its traces?`)) return;
        await fetch(`/api/chats/${encodeURIComponent(chat.chat_id)}`, { method: "DELETE" });
        if (selectedChat === chat.chat_id) {
          selectedChat = null;
          threadEl().replaceChildren();
        }
        refreshChats();
      };
      item.append(main, rename, del);
      list.appendChild(item);
    }
    return chats;
  } catch (e) {
    list.replaceChildren();
    line(list, `Could not load chats: ${e.message}`, "text-xs text-red-500");
    return [];
  }
}

/// Replay one persisted run (a single turn) into the pane.
async function replayRun(runId) {
  const r = await fetch(`/api/runs/${encodeURIComponent(runId)}`);
  if (!r.ok) throw new Error(`HTTP ${r.status}`);
  const data = await r.json();
  const card = createRunCard(data.summary.goal);
  let view = null;
  for (const ev of data.events || []) {
    if (ev.type === "run_meta") continue;
    if (ev.type === "final") view = ev;
    else renderEvent(card, ev);
  }
  card.touched = true;
  presentFinal(card, view, data.summary.goal, false);
  card.setMinimized(true);
}

/// Open one chat in the main pane: every turn in order — your
/// message, the (minimized) trace card, and Rose's reply.
async function openChat(chatId) {
  if (running) {
    bubble("⏳ A run is already in progress — wait for it to finish before switching chats.");
    return;
  }
  let runs;
  try {
    const r = await fetch(`/api/chats/${encodeURIComponent(chatId)}/runs`);
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    runs = (await r.json()).runs || [];
  } catch (e) {
    bubble(`⚠️ Could not load that chat: ${e.message}`);
    return;
  }
  selectedChat = chatId;
  threadEl().replaceChildren();
  for (const run of runs) {
    bubble(run.goal, true);
    await replayRun(run.run_id);
  }
  refreshChats();
}

/// Start a fresh chat: clear the pane, nothing selected.
function newChat() {
  if (running) {
    bubble("⏳ A run is already in progress — wait for it to finish before starting a new chat.");
    return;
  }
  selectedChat = null;
  threadEl().replaceChildren();
  refreshChats();
  document.getElementById("composer").focus();
}

function send() {
  const box = document.getElementById("composer");
  const v = box.value.trim();
  if (!v) return;
  // Append to the open chat (or start a new one if none is open):
  // the server continues the same chat_id and the brain receives
  // the earlier turns as context.
  bubble(v, true);
  box.value = "";
  runGoal(v, false, false, null, selectedChat);
}

async function refreshStatus() {
  const dot = document.getElementById("status-dot");
  try {
    const r = await fetch("/api/status");
    const s = await r.json();
    document.getElementById("status-text").textContent =
      `Idle · ${s.display || "?"} · brain=${s.brain}`;
    document.getElementById("backend-text").textContent =
      `backend=${s.backend} · ${s.version}`;
    document.getElementById("display-name").textContent = s.display ? `· ${s.display}` : "";
    dot.className = DOT + "bg-neutral-400";
    setControlUI(s.lease === "human");
  } catch (e) {
    document.getElementById("status-text").textContent = "rosed unreachable";
    dot.className = DOT + "bg-amber-500";
  }
  try {
    const r = await fetch("/api/keys");
    const k = await r.json();
    const st = document.getElementById("key-state");
    st.textContent = k.set ? `set (${k.source})` : "missing";
    st.className = k.set ? "text-green-600 font-semibold" : "text-red-500 font-semibold";
  } catch (e) { /* status already shows reachability */ }
}

async function refreshFrame() {
  try {
    const r = await fetch("/api/frame", { cache: "no-store" });
    if (!r.ok) return;
    const blob = await r.blob();
    const img = document.getElementById("frame");
    img.src = URL.createObjectURL(blob);
    img.classList.remove("hidden");
    document.getElementById("frame-note").style.display = "none";
  } catch (e) { /* backend not up yet */ }
}

// ---- takeover: you drive, Rose pauses (never killed) -----------------------
// The button toggles the control lease; while held, clicks / wheel / keys
// on the live view POST to /desktop/input and ride the same audited act()
// path as the agent, with Actor::Human. Coordinates go as fractions of the
// live image so X11 pixels, CDP viewports and the auto router all work
// without the UI knowing the backend or its size.
let controlling = false;

function setControlUI(held) {
  controlling = held;
  const btn = document.getElementById("take-control");
  const badge = document.getElementById("control-badge");
  const img = document.getElementById("frame");
  const hint = document.getElementById("control-hint");
  btn.textContent = held ? "Hand Back" : "Take Control";
  btn.title = held
    ? "Return the desktop to Rose (it re-observes and resumes)"
    : "Drive the desktop yourself — the agent pauses, nothing is killed";
  badge.textContent = held ? "YOU CONTROL" : "VIEW-ONLY";
  badge.className = "rounded px-2 py-0.5 text-xs font-semibold text-white " +
    (held ? "bg-green-600" : "bg-red-600");
  img.style.cursor = held ? "crosshair" : "";
  hint.textContent = held
    ? "You hold the desktop: click to click, double-click to double-click, wheel to scroll, type to type. Hand Back to resume Rose."
    : "Exactly what Rose sees. Take Control to drive it yourself through the same audited path — the agent pauses, never killed.";
}

async function toggleControl() {
  const to = controlling ? "/desktop/handback" : "/desktop/takeover";
  try {
    const r = await fetch(to, { method: "POST" });
    const j = await checked(r);
    setControlUI(j.holder === "human");
  } catch (e) {
    bubble(`⚠️ Takeover failed: ${e.message}`);
  }
}

async function checked(r) {
  const j = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(j.error || `HTTP ${r.status}`);
  return j;
}

async function postInput(body) {
  const r = await fetch("/desktop/input", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  await checked(r);
}

async function sendClick(kind, e) {
  if (!controlling) return;
  const { x, y } = frameFrac(e);
  try {
    await postInput({ kind, x, y });
  } catch (err) {
    bubble(`⚠️ ${kind === "click" ? "Click" : "Double-click"} not delivered: ${err.message}`);
  }
}

function frameFrac(e) {
  const rect = document.getElementById("frame").getBoundingClientRect();
  const x = Math.min(1, Math.max(0, (e.clientX - rect.left) / rect.width));
  const y = Math.min(1, Math.max(0, (e.clientY - rect.top) / rect.height));
  return { rect, x, y };
}

document.getElementById("frame").addEventListener("click", async (e) => {
  sendClick("click", e);
});

document.getElementById("frame").addEventListener("dblclick", async (e) => {
  e.preventDefault();
  sendClick("double_click", e);
});

document.getElementById("frame").addEventListener("wheel", async (e) => {
  if (!controlling) return;
  e.preventDefault();
  const { rect, x, y } = frameFrac(e);
  try {
    await postInput({ kind: "scroll", x, y, dy: e.deltaY / rect.height });
  } catch (err) {
    bubble(`⚠️ Scroll not delivered: ${err.message}`);
  }
}, { passive: false });

const TAKEOVER_KEYMAP = {
  Enter: "Return", Escape: "Escape", Tab: "Tab", Backspace: "Backspace",
  Delete: "Delete", ArrowLeft: "Left", ArrowRight: "Right",
  ArrowUp: "Up", ArrowDown: "Down", Home: "Home", End: "End",
  PageUp: "Page_Up", PageDown: "Page_Down", " ": "space",
};

document.addEventListener("keydown", async (e) => {
  if (!controlling) return;
  // Never hijack typing in the composer, search, key field, or dialogs.
  const t = e.target;
  if (t && (t.tagName === "TEXTAREA" || t.tagName === "INPUT" || t.tagName === "SELECT")) return;
  if (!document.getElementById("setup-modal").classList.contains("hidden")) return;
  if (e.metaKey) return;
  try {
    if (!e.ctrlKey && !e.altKey && e.key.length === 1) {
      e.preventDefault();
      await postInput({ kind: "type", text: e.key });
    } else {
      const keys = [];
      if (e.ctrlKey) keys.push("ctrl");
      if (e.altKey) keys.push("alt");
      if (e.shiftKey) keys.push("shift");
      const base = TAKEOVER_KEYMAP[e.key] || (e.key.length === 1 ? e.key : null);
      if (!base) return;
      keys.push(base);
      e.preventDefault();
      await postInput({ kind: "key", keys });
    }
  } catch (err) {
    bubble(`⚠️ Key not delivered: ${err.message}`);
  }
});

function saveKey() {
  const v = document.getElementById("api-key").value.trim();
  if (!v) return;
  fetch("/api/keys", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ muse_api_key: v }),
  }).then(async (r) => {
    document.getElementById("api-key").value = "";
    toggleSetup(false);
    if (r.ok) {
      bubble("API key saved for this session — server memory only, nothing written to disk.");
      refreshStatus();
    } else {
      const j = await r.json().catch(() => ({}));
      bubble(`⚠️ Key not saved: ${j.error || r.status}`);
    }
  });
}

function clearKey() {
  fetch("/api/keys", { method: "DELETE" }).then(() => {
    document.getElementById("api-key").value = "";
    refreshStatus();
  });
}

document.getElementById("composer").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); send(); }
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") toggleSetup(false);
});
document.getElementById("setup-modal").addEventListener("click", (e) => {
  if (e.target.id === "setup-modal") toggleSetup(false);
});

// ---- setup tabs -------------------------------------------------------------
// Keys pane talks to the real backend (POST /api/keys). Config pane tries
// GET/PUT /api/config when rosed ships it; until then it degrades to a
// localStorage draft so the UI is already wired.
window.lastTab = window.lastTab || "keys";
function setupTab(which) {
  window.lastTab = which;
  const keys = which === "keys";
  document.getElementById("pane-keys").classList.toggle("hidden", !keys);
  document.getElementById("pane-config").classList.toggle("hidden", keys);
  document.getElementById("tab-keys").className =
    "t-caption rounded-lg px-3 py-1.5 font-semibold " + (keys ? "btn-accent" : "btn-ghost");
  document.getElementById("tab-config").className =
    "t-caption rounded-lg px-3 py-1.5 font-semibold " + (!keys ? "btn-accent" : "btn-ghost");
  if (!keys) loadConfig();
}

// ---- config management ------------------------------------------------------
const CFG_FIELDS = ["cfg-bind","cfg-backend","cfg-display","cfg-width","cfg-height","cfg-cdp","cfg-launch","cfg-brain","cfg-model","cfg-endpoint","cfg-steps","cfg-minutes","cfg-calls","cfg-approvals","cfg-datadir"];
const CFG_DEFAULTS = {};
function snapshotConfig() {
  const o = {};
  CFG_FIELDS.forEach((id) => {
    const el = document.getElementById(id);
    if (!el) return;
    o[id] = el.type === "checkbox" ? el.checked : el.value;
  });
  return o;
}
function applyConfig(o) {
  CFG_FIELDS.forEach((id) => {
    const el = document.getElementById(id);
    if (!el || o[id] === undefined) return;
    if (el.type === "checkbox") el.checked = !!o[id];
    else el.value = o[id];
  });
  syncBrainNote();
}
function markDirty() {
  document.getElementById("config-dirty").classList.remove("hidden");
  document.getElementById("config-msg").textContent =
    "Unsaved changes — Apply validates locally, then PUT /api/config (or local draft until the backend lands).";
  syncBrainNote();
}
function markClean(msg) {
  document.getElementById("config-dirty").classList.add("hidden");
  document.getElementById("config-msg").textContent = msg;
}
function syncBrainNote() {
  const b = document.getElementById("cfg-brain").value;
  document.getElementById("brain-note").innerHTML =
    b === "fake"
      ? "fake brain needs no key — good for offline drafts."
      : 'muse brain reads the key from Setup → Keys or <span class="font-mono">ROSE_MUSE_API_KEY</span>.';
}
function collectConfig() {
  const v = (id) => document.getElementById(id).value.trim();
  const c = (id) => document.getElementById(id).checked;
  return {
    server: { bind: v("cfg-bind"), allow_remote: c("cfg-allow-remote") },
    desktop: { backend: v("cfg-backend"), display: v("cfg-display"), width: +v("cfg-width"), height: +v("cfg-height") },
    cdp: { endpoint: v("cfg-cdp"), launch_if_missing: c("cfg-launch") },
    models: { brain: v("cfg-brain"), endpoint: v("cfg-endpoint"), model: v("cfg-model") },
    policy: { max_steps: +v("cfg-steps"), max_minutes: +v("cfg-minutes"), max_model_calls: +v("cfg-calls"), require_approval_for: v("cfg-approvals").split(",").map((s) => s.trim()).filter(Boolean) },
    data: { dir: v("cfg-datadir") },
  };
}
function configFromBackend(j) {
  // Accept either flat {cfg-bind: ...} or nested {server:{...},...} shapes.
  if (!j || typeof j !== "object") return null;
  if (j["cfg-bind"] !== undefined) return j;
  const o = {};
  if (j.server) { if (j.server.bind) o["cfg-bind"] = j.server.bind; if (j.server.allow_remote !== undefined) o["cfg-allow-remote"] = j.server.allow_remote; }
  if (j.desktop) { if (j.desktop.backend) o["cfg-backend"] = j.desktop.backend; if (j.desktop.display) o["cfg-display"] = j.desktop.display; if (j.desktop.width) o["cfg-width"] = String(j.desktop.width); if (j.desktop.height) o["cfg-height"] = String(j.desktop.height); }
  if (j.cdp) { if (j.cdp.endpoint) o["cfg-cdp"] = j.cdp.endpoint; if (j.cdp.launch_if_missing !== undefined) o["cfg-launch"] = j.cdp.launch_if_missing; }
  if (j.models) { if (j.models.brain) o["cfg-brain"] = j.models.brain; if (j.models.model) o["cfg-model"] = j.models.model; if (j.models.endpoint) o["cfg-endpoint"] = j.models.endpoint; }
  if (j.policy) { if (j.policy.max_steps) o["cfg-steps"] = String(j.policy.max_steps); if (j.policy.max_minutes) o["cfg-minutes"] = String(j.policy.max_minutes); if (j.policy.max_model_calls) o["cfg-calls"] = String(j.policy.max_model_calls); if (j.policy.require_approval_for) o["cfg-approvals"] = (Array.isArray(j.policy.require_approval_for) ? j.policy.require_approval_for : [j.policy.require_approval_for]).join(", "); }
  if (j.data && j.data.dir) o["cfg-datadir"] = j.data.dir;
  return Object.keys(o).length ? o : null;
}
async function loadConfig() {
  const src = document.getElementById("config-source");
  try {
    const r = await fetch("/api/config", { cache: "no-store" });
    if (r.ok) {
      const j = await r.json();
      const flat = configFromBackend(j.config || j);
      if (flat) {
        applyConfig(flat);
        src.textContent = "rosed (/api/config)";
        markClean("Loaded from rosed. Real wiring: GET fills this form, PUT applies.");
        try { localStorage.setItem("rose-config-draft", JSON.stringify(snapshotConfig())); } catch (e) {}
        return;
      }
    }
    throw new Error(r.status === 404 ? "no /api/config yet" : `HTTP ${r.status}`);
  } catch (e) {
    try {
      const saved = JSON.parse(localStorage.getItem("rose-config-draft") || "null");
      if (saved) {
        applyConfig(saved);
        src.textContent = "local draft (backend has no /api/config yet)";
        markClean("Backend has no /api/config yet — showing your local draft. Apply saves locally.");
        return;
      }
    } catch (err) {}
    src.textContent = "defaults (rose.example.toml)";
    markClean("Backend has no /api/config yet — defaults shown. Apply saves a local draft.");
  }
}
async function saveConfig() {
  const cfg = collectConfig();
  const errs = [];
  if (!/^\S+:\d+$/.test(cfg.server.bind)) errs.push("bind must be host:port");
  if (!(cfg.desktop.width >= 640 && cfg.desktop.height >= 360)) errs.push("desktop size looks too small");
  if (!(cfg.policy.max_steps >= 1 && cfg.policy.max_steps <= 200)) errs.push("max_steps 1..200");
  if (errs.length) {
    document.getElementById("config-msg").textContent = "⚠️ " + errs.join(" · ");
    return;
  }
  try {
    const r = await fetch("/api/config", {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(cfg),
    });
    const j = await r.json().catch(() => ({}));
    if (r.ok) {
      document.getElementById("config-source").textContent = j.source || "rosed (/api/config)";
      if (j.config) {
        const flat = configFromBackend(j.config);
        if (flat) applyConfig(flat);
      }
      if (j.restart_needed) {
        markClean("✓ Applied — policy + brain hot-reloaded; backend/display/bind/data need a `rosed` restart.");
        bubble(`Config applied: backend=<strong>${cfg.desktop.backend}</strong>, brain=<strong>${cfg.models.brain}</strong>, budget=<strong>${cfg.policy.max_steps} steps / ${cfg.policy.max_minutes} min</strong>. Backend/display/bind changes need a <span class="font-mono">rosed</span> restart.`);
      } else {
        markClean("✓ Applied — policy + brain hot-reloaded into the next run.");
        bubble(`Config applied: backend=<strong>${cfg.desktop.backend}</strong>, brain=<strong>${cfg.models.brain}</strong>, budget=<strong>${cfg.policy.max_steps} steps / ${cfg.policy.max_minutes} min</strong>.`);
      }
      refreshStatus();
      return;
    }
    const msg = (j && j.error) || `HTTP ${r.status}`;
    if (r.status === 404) throw new Error("no /api/config yet");
    document.getElementById("config-msg").textContent = "⚠️ " + msg;
    return;
  } catch (e) {
    if (String(e.message || e).includes("no /api/config")) {
      try { localStorage.setItem("rose-config-draft", JSON.stringify(snapshotConfig())); } catch (err) {}
      document.getElementById("config-source").textContent = "local draft (backend has no /api/config yet)";
      markClean("Saved locally — rosed has no /api/config yet, so nothing was hot-reloaded.");
      bubble(`Config saved locally (rosed has no <span class="font-mono">/api/config</span> yet): backend=<strong>${cfg.desktop.backend}</strong>, brain=<strong>${cfg.models.brain}</strong>.`);
    } else {
      document.getElementById("config-msg").textContent = "⚠️ " + (e.message || e);
    }
  }
}
function resetConfig() {
  applyConfig(CFG_DEFAULTS);
  try { localStorage.removeItem("rose-config-draft"); } catch (e) {}
  document.getElementById("config-source").textContent = "defaults (rose.example.toml)";
  markClean("Reset to rose.example.toml defaults.");
}
function exportConfig() {
  const cfg = collectConfig();
  const toml =
    `[server]\nbind = "${cfg.server.bind}"\nallow_remote = ${cfg.server.allow_remote}\n\n` +
    `[desktop]\nbackend = "${cfg.desktop.backend}"\ndisplay = "${cfg.desktop.display}"\nwidth = ${cfg.desktop.width}\nheight = ${cfg.desktop.height}\n\n` +
    `[cdp]\nendpoint = "${cfg.cdp.endpoint}"\nlaunch_if_missing = ${cfg.cdp.launch_if_missing}\n\n` +
    `[models]\nbrain = "${cfg.models.brain}"\nendpoint = "${cfg.models.endpoint}"\nmodel = "${cfg.models.model}"\n\n` +
    `[policy]\nmax_steps = ${cfg.policy.max_steps}\nmax_minutes = ${cfg.policy.max_minutes}\nmax_model_calls = ${cfg.policy.max_model_calls}\n` +
    `require_approval_for = [${cfg.policy.require_approval_for.map((s) => `"${s}"`).join(", ")}]\n\n` +
    `[data]\ndir = "${cfg.data.dir}"\n`;
  if (navigator.clipboard) navigator.clipboard.writeText(toml).then(() => markClean("TOML copied to clipboard.")).catch(() => markClean("Copy failed."));
}

// ---- video → instructions ---------------------------------------------------
// Backend: POST /api/videos?filename= (raw bytes) → <data>/videos/<id>/,
// POST /api/videos/:id/instructions → Spark reads the video natively
// (frames + narration in one call: no SAM, no separate transcription)
// and persists the draft to steps.json, PUT :id/draft saves edits.
// Local object URLs + a clearly-labeled offline mock remain only for when
// rosed is unreachable — the server never returns mock steps.
let videos = [];
let selectedId = null;
let pipeTimer = null;
// Offline-mock stages only (no backend): the real path is one Spark call.
const STAGES = [
  { key: "read", label: "Reading video file (offline mock)", pct: 30 },
  { key: "listen", label: "Sampling narration (offline mock)", pct: 60 },
  { key: "draft", label: "Drafting placeholder steps (offline mock)", pct: 100 },
];
function fmtSize(b) {
  if (b < 1024) return b + " B";
  if (b < 1048576) return (b / 1024).toFixed(1) + " KB";
  if (b < 1073741824) return (b / 1048576).toFixed(1) + " MB";
  return (b / 1073741824).toFixed(2) + " GB";
}
function pickVideos(input) { addFiles([...input.files]); input.value = ""; }
function dropVideos(e) {
  e.preventDefault();
  if (e.dataTransfer?.files?.length) addFiles([...e.dataTransfer.files]);
}
async function probeVideoBackend() {
  try {
    const r = await fetch("/api/videos", { method: "GET", cache: "no-store" });
    if (r.ok) {
      document.getElementById("video-backend").textContent = "rosed (/api/videos)";
      return true;
    }
  } catch (e) {}
  document.getElementById("video-backend").textContent = "local only";
  return false;
}
async function uploadOne(file) {
  // Raw bytes (no multipart dep on the server): POST /api/videos?filename=.
  const r = await fetch(`/api/videos?filename=${encodeURIComponent(file.name)}`, {
    method: "POST",
    headers: { "Content-Type": file.type || "video/mp4" },
    body: file,
  });
  const j = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error((j && j.error) || `HTTP ${r.status}`);
  return j.video;
}
/// Generate / Save / Send are always visible under the player; Save and
/// Send stay disabled until the selected draft actually has steps.
function updateVideoActions() {
  const v = selectedVideo();
  const hasSteps = !!(v && v.steps.length);
  const gen = document.getElementById("vd-generate");
  const save = document.getElementById("vd-save");
  const send = document.getElementById("vd-send");
  if (gen) gen.disabled = !v || v.status === "working" || v.status === "uploading…";
  if (save) save.disabled = !hasSteps;
  if (send) send.disabled = !hasSteps;
}
async function addFiles(files) {
  const vids = files.filter((f) => f.type.startsWith("video/") || /\.(mp4|webm|mov|mkv)$/i.test(f.name));
  if (!vids.length) { bubble("No video files in that drop — try mp4 / webm / mov."); return; }
  // Render instantly (local playback URLs), then upload in the background
  // and patch each entry — a big file over a slow tunnel must not leave
  // the list empty with no way to act.
  for (const f of vids) {
    const entry = {
      id: "v" + Math.random().toString(36).slice(2, 8),
      name: f.name, size: f.size || 0, url: URL.createObjectURL(f),
      serverId: null, status: "uploading…", progress: 0, stage: "Uploading to rosed…", log: [], steps: [],
      _file: f,
    };
    videos.unshift(entry);
    if (!selectedId) selectedId = entry.id;
  }
  renderVideos();
  selectVideo(selectedId);
  updateVideoActions();
  bubble(`${vids.length} video${vids.length === 1 ? "" : "s"} queued — press <strong>Generate instructions</strong> under the desktop to draft agent steps.`);
  let backendOk = videos.some((v) => v.serverId);
  for (const entry of videos) {
    const f = entry._file;
    if (!f) continue;
    try {
      const meta = await uploadOne(f);
      delete entry._file;
      backendOk = true;
      entry.serverId = meta.id;
      entry.name = meta.name || entry.name;
      entry.size = meta.size || entry.size;
      entry.status = meta.status || "queued";
      entry.stage = "Waiting…";
      // Server bytes win for playback (survives restarts, shared with rosed).
      entry.url = `/api/videos/${encodeURIComponent(meta.id)}/file`;
    } catch (e) {
      // Keep the local playback + the file itself: Generate retries the
      // upload when rosed is reachable, otherwise it says why, loudly.
      const reason = String((e && e.message) || e).slice(0, 160);
      entry.status = "queued";
      entry.stage = `Upload failed: ${reason} — press Generate to retry`;
      bubble(`⚠️ Upload of <strong>${entry.name.replace(/</g, "&lt;")}</strong> failed: ${reason.replace(/</g, "&lt;")}. Press Generate to retry.`);
    }
    renderVideos();
    if (selectedId === entry.id) selectVideo(entry.id);
    updateVideoActions();
  }
  if (backendOk) document.getElementById("video-backend").textContent = "rosed (/api/videos)";
  probeVideoBackend();
}
async function refreshServerVideos() {
  try {
    const r = await fetch("/api/videos", { cache: "no-store" });
    if (!r.ok) return;
    const j = await r.json();
    const known = new Set(videos.map((v) => v.serverId));
    for (const m of j.videos || []) {
      if (known.has(m.id)) continue;
      videos.push({
        id: m.id, serverId: m.id, name: m.name, size: m.size || 0,
        url: `/api/videos/${encodeURIComponent(m.id)}/file`,
        status: m.status || "queued", progress: m.status === "ready" ? 100 : 0,
        stage: m.status === "ready" ? "Draft ready — review below" : "Waiting…",
        log: [], steps: [],
      });
    }
    if (videos.length && !selectedId) selectedId = videos[0].id;
    renderVideos();
    if (selectedId) selectVideo(selectedId);
  } catch (e) {}
}
function selectedVideo() { return videos.find((v) => v.id === selectedId) || null; }
function renderVideos() {
  const list = document.getElementById("video-list");
  if (!list) return;
  document.getElementById("video-count").textContent = videos.length;
  document.getElementById("video-empty").style.display = videos.length ? "none" : "";
  list.replaceChildren();
  videos.forEach((v) => {
    const row = document.createElement("div");
    row.className = "flex cursor-pointer items-center gap-2 rounded-lg border px-2 py-1.5";
    row.style.borderColor = v.id === selectedId ? "var(--accent)" : "var(--line)";
    row.onclick = () => selectVideo(v.id);
    const dot = document.createElement("span");
    dot.className = "size-2 flex-none rounded-full " + (v.status === "ready" ? "bg-green-600" : v.status === "working" ? "animate-pulse bg-blue-600" : "bg-neutral-400");
    const meta = document.createElement("div");
    meta.className = "min-w-0 flex-1";
    const n = document.createElement("div");
    n.className = "t-caption truncate font-semibold";
    n.textContent = v.name;
    const s = document.createElement("div");
    s.className = "muted t-caption";
    s.textContent = `${fmtSize(v.size)} · ${v.status}${v.steps.length ? ` · ${v.steps.length} steps` : ""}`;
    meta.append(n, s);
    const del = document.createElement("button");
    del.className = "t-caption rounded px-1.5 py-0.5 text-neutral-400 hover:text-red-600";
    del.textContent = "✕";
    del.title = "Remove video";
    del.onclick = (e) => { e.stopPropagation(); removeVideo(v.id); };
    row.append(dot, meta, del);
    list.appendChild(row);
  });
}
async function removeVideo(id) {
  const v = videos.find((x) => x.id === id);
  if (v && v.serverId) {
    try { await fetch(`/api/videos/${encodeURIComponent(v.serverId)}`, { method: "DELETE" }); } catch (e) {}
  }
  videos = videos.filter((x) => x.id !== id);
  if (selectedId === id) selectedId = videos[0]?.id || null;
  renderVideos();
  if (selectedId) selectVideo(selectedId);
  else { document.getElementById("video-detail").classList.add("hidden"); updateVideoActions(); }
}
async function selectVideo(id) {
  selectedId = id;
  const v = selectedVideo();
  renderVideos();
  if (!v) return;
  document.getElementById("video-detail").classList.remove("hidden");
  document.getElementById("vd-name").textContent = v.name;
  if (v.url) document.getElementById("vd-player").src = v.url;
  // Server draft: pull persisted steps on first open (list has counts only).
  if (v.serverId && !v.steps.length && v.status === "ready") {
    try {
      const r = await fetch(`/api/videos/${encodeURIComponent(v.serverId)}`, { cache: "no-store" });
      if (r.ok) {
        const j = await r.json();
        if (j.steps && j.steps.length) {
          v.steps = j.steps;
          v.progress = 100;
          v.stage = "Draft ready — review below";
        }
      }
    } catch (e) {}
  }
  setPipeline(v.stage, v.progress, v.log.join("\n"));
  setStatusBadge(v.status);
  renderSteps();
  updateVideoActions();
}
function setStatusBadge(st) {
  document.getElementById("vd-status").textContent = st;
}
function setPipeline(stage, pct, log) {
  document.getElementById("vd-stage").textContent = stage;
  document.getElementById("vd-pct").textContent = Math.round(pct) + "%";
  document.getElementById("vd-bar").style.width = pct + "%";
  document.getElementById("vd-log").textContent = log || "";
}
async function runPipeline(v) {
  if (!v) { bubble("Upload a video first — then generate instructions from it."); return; }
  if (v.status === "working") return;
  if (v.status === "uploading…") {
    bubble("Still uploading — Generate unlocks when the upload finishes.");
    return;
  }
  // No backend copy yet: if rosed is reachable and we still hold the file,
  // upload now instead of mocking. Only when rosed is unreachable does the
  // labeled offline mock below run.
  if (!v.serverId) {
    const backend = await probeVideoBackend();
    if (backend && !v._file) {
      v.stage = "No backend copy and no local file — upload the video again";
      setPipeline(v.stage, 0, v.log.join("\n"));
      renderVideos(); updateVideoActions();
      bubble("⚠️ rosed has no copy of that video and the local file is gone — upload it again, then Generate.");
      return;
    }
    if (backend && v._file) {
      try {
        v.status = "uploading…"; v.stage = "Uploading to rosed…";
        setPipeline(v.stage, 2, "POST /api/videos?filename=");
        setStatusBadge("uploading…"); renderVideos(); updateVideoActions();
        const meta = await uploadOne(v._file);
        delete v._file;
        v.serverId = meta.id;
        v.name = meta.name || v.name;
        v.size = meta.size || v.size;
        v.url = `/api/videos/${encodeURIComponent(meta.id)}/file`;
        document.getElementById("video-backend").textContent = "rosed (/api/videos)";
      } catch (e) {
        const reason = String((e && e.message) || e).slice(0, 200);
        v.status = "queued"; v.stage = `Upload failed: ${reason} — fix and retry`;
        setPipeline(v.stage, 0, v.log.concat([reason]).slice(-4).join("\n"));
        setStatusBadge("queued");
        renderVideos(); updateVideoActions();
        bubble(`⚠️ Upload failed: ${reason.replace(/</g, "&lt;")}. Nothing drafted — fix and retry.`);
        return;
      }
    }
  }
  clearInterval(pipeTimer);
  v.status = "working"; v.progress = 0; v.log = []; v.steps = [];
  document.getElementById("vd-steps-wrap").classList.add("hidden");
  document.getElementById("vd-saved").classList.add("hidden");
  setStatusBadge("working");
  selectVideo(v.id);
  updateVideoActions();
  // Backend: Spark reads the video natively (frames + narration, no SAM).
  // Any failure surfaces as an error — the server never returns mock steps.
  if (v.serverId) {
    try {
      v.stage = "Spark is watching the video…";
      setPipeline(v.stage, 5, "POST /api/videos/:id/instructions");
      const r = await fetch(`/api/videos/${encodeURIComponent(v.serverId)}/instructions`, { method: "POST" });
      const j = await r.json().catch(() => ({}));
      if (!r.ok) throw new Error((j && j.error) || `HTTP ${r.status}`);
      v.steps = j.steps || [];
      if (!v.steps.length) throw new Error("Spark found no task steps in this video");
      v.status = "ready"; v.progress = 100; v.stage = "Draft ready — review below";
      v.log.push("spark watched the video (frames + narration)");
      setPipeline(v.stage, 100, v.log.slice(-4).join("\n") + "\nsteps.json saved ✓");
      setStatusBadge("ready");
      renderVideos(); renderSteps(); updateVideoActions();
      bubble(`Video <strong>${v.name.replace(/</g, "&lt;")}</strong> → draft ready: <strong>${v.steps.length} steps</strong> from Spark. Review under the desktop, then send to Rose.`);
      return;
    } catch (e) {
      const msg = String((e && e.message) || e).slice(0, 300).replace(/</g, "&lt;");
      v.status = "queued"; v.progress = 0; v.stage = "Spark failed — see log";
      v.log.push(msg);
      setPipeline(v.stage, 0, v.log.slice(-4).join("\n"));
      setStatusBadge("queued");
      renderVideos(); updateVideoActions();
      bubble(`⚠️ Spark could not read that video: ${msg}. Nothing drafted — fix and retry.`);
      return;
    }
  }
  let i = 0;
  const tick = () => {
    if (selectedId !== v.id) { clearInterval(pipeTimer); return; }
    const st = STAGES[i];
    v.stage = st.label; v.progress = st.pct;
    v.log.push(`{stage:"${st.key}", pct:${st.pct}} — ${st.label}`);
    setPipeline(v.stage, v.progress, v.log.slice(-4).join("\n"));
    renderVideos();
    i++;
    if (i >= STAGES.length) {
      clearInterval(pipeTimer);
      v.status = "ready"; v.progress = 100; v.stage = "Offline mock draft — NOT from the video";
      v.steps = mockSteps(v.name);
      setPipeline(v.stage, 100, v.log.slice(-4).join("\n") + "\nConnect rosed for a real Spark video analysis");
      setStatusBadge("ready");
      renderVideos(); renderSteps(); updateVideoActions();
      bubble(`Video <strong>${v.name.replace(/</g, "&lt;")}</strong> → offline <strong>mock</strong> draft (placeholder steps, not from the video). Upload to rosed for a real Spark analysis.`);
    }
  };
  tick();
  pipeTimer = setInterval(tick, 900);
}
function cancelPipeline() {
  clearInterval(pipeTimer);
  const v = selectedVideo();
  if (v && v.status === "working") {
    v.status = "queued"; v.stage = "Cancelled — press Generate to retry";
    setStatusBadge("queued"); setPipeline(v.stage, v.progress, v.log.join("\n"));
    renderVideos(); updateVideoActions();
  }
}
// Offline-only placeholder steps (clearly labeled in the UI). Used only when
// rosed is unreachable; the server path never mocks.
function mockSteps(name) {
  const base = name.replace(/\.\w+$/, "").replace(/[_-]+/g, " ").slice(0, 40) || "recorded task";
  return [
    { ts: 12, say: "Open the billing portal", intent: `open billing portal (${base})`, params: "portal_url = {{portal_url}}", approval: false, conf: 0.93, quote: "I'm opening the billing portal" },
    { ts: 31, say: "Go to Invoices", intent: "navigate to Invoices list", params: "month = {{month}}", approval: false, conf: 0.88, quote: "go to Invoices" },
    { ts: 65, say: "Filter by month — changes every time", intent: "set month filter", params: "month = {{month}}", approval: false, conf: 0.61, quote: "filter by month - changes every time" },
    { ts: 100, say: "Export, then CSV", intent: "click Export → download CSV", params: "format = csv", approval: true, conf: 0.79, quote: "Export, then CSV" },
  ];
}
function renderSteps() {
  const v = selectedVideo();
  const wrap = document.getElementById("vd-steps-wrap");
  const box = document.getElementById("vd-steps");
  if (!v || !v.steps.length) { wrap.classList.add("hidden"); return; }
  wrap.classList.remove("hidden");
  const avg = v.steps.reduce((a, s) => a + s.conf, 0) / v.steps.length;
  document.getElementById("vd-conf").textContent = `avg confidence ${(avg * 100).toFixed(0)}%`;
  box.replaceChildren();
  v.steps.forEach((s, idx) => {
    const card = document.createElement("div");
    card.className = "rounded-lg border p-2";
    card.style.borderColor = s.conf < 0.65 ? "#ef4444" : s.conf < 0.8 ? "#f59e0b" : "var(--line)";
    const head = document.createElement("button");
    head.className = "flex w-full items-center gap-2 text-left";
    const mm = String(Math.floor(s.ts / 60)).padStart(2, "0"), ss = String(s.ts % 60).padStart(2, "0");
    const confCls = s.conf >= 0.8 ? "text-green-700 dark:text-green-400" : s.conf >= 0.65 ? "text-amber-600 dark:text-amber-400" : "text-red-600";
    head.innerHTML = `<span class="t-caption rounded px-1.5 py-0.5 font-mono" style="background:var(--line)">▶ ${mm}:${ss}</span><span class="t-caption flex-1 font-semibold"></span><span class="t-caption font-semibold ${confCls}"></span>`;
    head.children[1].textContent = `Step ${idx + 1} · ${s.say}`;
    head.children[2].textContent = Math.round(s.conf * 100) + "%";
    head.title = "Seek video to this moment";
    head.onclick = () => { const p = document.getElementById("vd-player"); try { p.currentTime = s.ts; p.play(); } catch (e) {} };
    const intent = document.createElement("input");
    intent.value = s.intent;
    intent.className = "cfg-input t-caption mt-1.5";
    intent.placeholder = "Agent intent…";
    intent.oninput = () => { s.intent = intent.value; hideSaved(); };
    const params = document.createElement("input");
    params.value = s.params;
    params.className = "cfg-input t-caption mt-1 font-mono";
    params.placeholder = "params, {{slots}} for values that change";
    params.oninput = () => { s.params = params.value; hideSaved(); };
    const foot = document.createElement("div");
    foot.className = "mt-1 flex items-center gap-2";
    const q = document.createElement("span");
    q.className = "muted t-caption flex-1 truncate italic";
    q.textContent = `“${s.quote}”`;
    const ap = document.createElement("label");
    ap.className = "t-caption flex flex-none items-center gap-1";
    const cb = document.createElement("input");
    cb.type = "checkbox"; cb.checked = s.approval;
    cb.onchange = () => { s.approval = cb.checked; hideSaved(); };
    ap.append(cb, document.createTextNode("needs approval"));
    const rm = document.createElement("button");
    rm.className = "t-caption flex-none rounded px-1.5 py-0.5 text-neutral-400 hover:text-red-600";
    rm.textContent = "✕"; rm.title = "Delete step";
    rm.onclick = () => { v.steps.splice(idx, 1); renderSteps(); renderVideos(); updateVideoActions(); };
    foot.append(q, ap, rm);
    card.append(head, intent, params, foot);
    box.appendChild(card);
  });
  updateVideoActions();
}
function hideSaved() { document.getElementById("vd-saved").classList.add("hidden"); }
function showSaved(msg) {
  const el = document.getElementById("vd-saved");
  el.textContent = msg;
  el.classList.remove("hidden");
}
function addStep() {
  const v = selectedVideo(); if (!v) return;
  v.steps.push({ ts: Math.round(document.getElementById("vd-player").currentTime || 0), say: "New step", intent: "", params: "", approval: false, conf: 0.5, quote: "" });
  renderSteps();
  updateVideoActions();
}
async function saveDraft() {
  const v = selectedVideo(); if (!v) return;
  if (v.serverId) {
    try {
      const r = await fetch(`/api/videos/${encodeURIComponent(v.serverId)}/draft`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ steps: v.steps }),
      });
      if (r.ok) {
        showSaved("Draft saved to rosed.");
        renderVideos();
        return;
      }
      const j = await r.json().catch(() => ({}));
      showSaved("⚠️ Save failed: " + (j.error || `HTTP ${r.status}`));
      return;
    } catch (e) {}
  }
  // Local fallback: no backend (or upload failed) — draft lives in the page.
  showSaved("Draft saved locally (no video backend — upload again once rosed is reachable).");
}
function useInChat() {
  const v = selectedVideo();
  if (!v || !v.steps.length) { bubble("Generate instructions first — the draft is empty."); return; }
  const text = `From video "${v.name}":\n` + v.steps.map((s, i) => `${i + 1}. ${s.intent} [${s.params}]${s.approval ? " (needs approval)" : ""}`).join("\n");
  document.getElementById("composer").value = text;
  document.getElementById("composer").focus();
  bubble(`Draft from <strong>${v.name.replace(/</g, "&lt;")}</strong> loaded into the composer — review, then hit Run. Low-confidence steps will pause for approval at replay.`);
}

/// On page load: fill the sidebar chat list and open the most
/// recent chat. Each chat is a separate run; nothing is merged
/// into one long thread.
async function restoreHistory() {
  const chats = await refreshChats();
  if (chats.length) await openChat(chats[0].chat_id);
}

(function initUiUpdates() {
  CFG_FIELDS.forEach((id) => {
    const el = document.getElementById(id);
    if (el) CFG_DEFAULTS[id] = el.type === "checkbox" ? el.checked : el.value;
  });
  try {
    const saved = JSON.parse(localStorage.getItem("rose-config-draft") || "null");
    if (saved) applyConfig(saved);
  } catch (e) {}
  syncBrainNote();
  renderVideos();
  probeVideoBackend().then((ok) => { if (ok) refreshServerVideos(); });
})();

setInterval(refreshStatus, 2000);
setInterval(refreshFrame, 1000);
refreshStatus();
refreshFrame();
restoreHistory();
