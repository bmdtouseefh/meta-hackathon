// Rose web shell: chat-first. Buttons that need M2/T1.4 stay disabled.
const DOT = "inline-block w-3 h-3 rounded-full ";
const threadEl = () => document.getElementById("live-tail");

function bubble(html, mine) {
  const d = document.createElement("div");
  if (mine) {
    d.className = "self-end max-w-[85%] px-[2%] py-2 rounded-2xl rounded-br-sm bg-[#0064E0] text-white";
    d.textContent = html;
  } else {
    d.className = "self-start max-w-[90%] flex gap-2";
    d.innerHTML = `<span class="flex-none">🌌</span><div class="px-[2%] py-2 rounded-2xl rounded-tl-sm bg-neutral-100 dark:bg-neutral-800">${html}</div>`;
  }
  threadEl().appendChild(d);
  d.scrollIntoView({ block: "nearest" });
}

function approvalCard(say, goal) {
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
    await runGoal(goal, true, d);
  };
  deny.onclick = () => {
    d.querySelector("div div").innerHTML += `<div class="text-sm text-neutral-500 mt-1">Denied — nothing acted on.</div>`;
    approve.disabled = true;
    deny.disabled = true;
  };
  threadEl().appendChild(d);
  d.scrollIntoView({ block: "nearest" });
}

async function runGoal(goal, force, card) {
  let view;
  try {
    const r = await fetch("/api/step", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ goal, force }),
    });
    view = await r.json();
    if (!r.ok) throw new Error(view.error || `HTTP ${r.status}`);
  } catch (e) {
    bubble(`⚠️ Step failed: ${e.message}`);
    return;
  }
  const target = card || null;
  if (view.approval_needed) {
    approvalCard(view.say, goal);
  } else if (view.done) {
    bubble(`✅ Done — ${view.say}`);
  } else if (view.acted) {
    bubble(`${view.say} <span class="text-xs opacity-70">· acted ✓</span>`);
  } else {
    bubble(view.say);
  }
  if (target) target.querySelectorAll("button").forEach((b) => (b.disabled = true));
}

function send() {
  const box = document.getElementById("composer");
  const v = box.value.trim();
  if (!v) return;
  bubble(v, true);
  box.value = "";
  runGoal(v, false);
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

setInterval(refreshStatus, 2000);
setInterval(refreshFrame, 1000);
refreshStatus();
refreshFrame();
