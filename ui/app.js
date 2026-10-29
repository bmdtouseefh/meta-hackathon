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
        <button class="px-4 py-1.5 rounded bg-green-700 text-white font-semibold">Approve</button>
        <button class="px-4 py-1.5 rounded bg-red-700 text-white font-semibold">Deny</button>
      </div>
    </div>`;
  const [approve, deny] = d.querySelectorAll("button");
  approve.onclick = async () => {
    approve.disabled = true;
    await runGoal(goal, true, d, chatId);
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
        headRow.className = "text-xs font-semibold uppercase tracking-wide opacity-60";
        headRow.textContent = `Step ${step + 1}`;
        row.appendChild(headRow);
        steps.appendChild(row);
        rows.set(step, row);
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
      line(row, `👀 Saw ${ev.width}×${ev.height} screenshot (${ev.frame_bytes} bytes)${win}`, "text-sm");
      if (ev.frame && ev.run_id) {
        const a = document.createElement("a");
        a.href = `/api/runs/${encodeURIComponent(ev.run_id)}/frames/${ev.step}`;
        a.target = "_blank";
        a.rel = "noopener";
        a.className = "text-xs underline opacity-80";
        a.textContent = "🖼 View the screenshot the brain saw";
        row.appendChild(a);
      }
      const hist = ev.history && ev.history.length ? ev.history.join(" → ") : "(empty — first step)";
      line(row, `↩ Fed back to brain: this screenshot + history: ${hist}`, "text-xs opacity-70");
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

async function runGoal(goal, force, approvalEl, chatId = selectedChat) {
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
      body: JSON.stringify({ goal, force, chat_id: chatId }),
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
  runGoal(v, false, null, selectedChat);
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

/// On page load: fill the sidebar chat list and open the most
/// recent chat. Each chat is a separate run; nothing is merged
/// into one long thread.
async function restoreHistory() {
  const chats = await refreshChats();
  if (chats.length) await openChat(chats[0].chat_id);
}

setInterval(refreshStatus, 2000);
setInterval(refreshFrame, 1000);
refreshStatus();
refreshFrame();
restoreHistory();
