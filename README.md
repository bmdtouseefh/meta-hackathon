# Rose — the private desktop agent you teach by showing

**Show Rose once. It does it forever — on your VM, never in someone else's cloud.**

Rose lives on your own Ubuntu VM, sees the XFCE desktop, and acts on it with mouse and keyboard. Teach it a task with a narrated screen recording and it replays the task on demand — or hand the desktop to any AI coding agent over MCP. Your screen, recordings, and credentials stay on your machine. Unlike cloud computer-use bots, there is no SaaS operator that can see your workflows.

Built for the Meta Global AI Developer Hackathon (Indie Developer track, *Build for Your World*).

> Rose is built for private workflow automation: it supports open models — Muse Spark Open for video → instructions and Muse Glimmer for live execution (point `endpoint`/`model` and `video_endpoint`/`video_model` at any OpenAI-compatible provider). For this demo we use hosted Muse Spark 1.3 throughout.

## Demo

Watch Rose in action:

[![Rose demo](https://img.youtube.com/vi/ehAVQfn9i4s/hqdefault.jpg)](https://youtu.be/ehAVQfn9i4s)

> [Watch on YouTube](https://youtu.be/ehAVQfn9i4s)

## Why Rose exists

Cloud screen-control bots work — until the workflow is one you can't upload. A trading screen with your strategy on it. An ops console behind VPN login. Anything where the recording itself is the secret. Your choices were: hand it to a third-party SaaS anyway, write brittle pixel-coordinate scripts, buy a heavyweight RPA suite, or just keep doing it by hand every morning.

Rose is the alternative built for that case:

- **Private by architecture.** One static Rust binary (`rosed`) on your VM. Desktop, browser session, recordings, skills, and run traces live under `~/.local/share/rose/`. Nothing goes to a third party except Meta model API calls — and live execution follows a Glimmer-compatible path for full self-hosting.
- **Teachable, not programmable.** No selectors, no scripts. Record yourself doing the task while narrating it ("filter by March — this changes monthly — then Export"). One native Muse Spark call reads the video's frames *and* narration together and drafts a reusable skill: each step keeps its evidence (`video timestamp`, `narration quote`, `confidence`) and changeable values become `{{parameters}}`.
- **Reliable hands.** The agent never guesses pixels. It aims by numbered grid cell (`g14,3`, sub-cell fractions for small targets), resolved server-side against live geometry — steady even on canvases and charts where DOM-based bots go blind.
- **Agents welcome.** The same desktop is exposed as an MCP server (12 tools), so any coding agent — opencode, Muse Code, your own harness — can drive it and gets test-run history for free.

## Two ways to use it

**1. Rose drives (video → skill).** Record → Generate steps in Skill Studio (click a step to seek the video moment it came from) → correct, set `{{params}}`, approve gated steps → Run. Morning screening, invoice exports, folder triage — replay with new inputs.

**2. Your agent drives (MCP).** Point any MCP client at the VM and it gets eyes and hands: screenshot, click/type/key/scroll/drag/wait, shell, plus `test.start`/`test.finish` so the Web UI shows exactly what the agent did, step by step. The agent can recursively improve itself mid-run: revise a learned skill via `skill.update`, and write its own helper scripts via `shell.exec` to replace slow GUI steps with fast deterministic checks.

Both ride the same audited `act()` path: one atomic action at a time, Governor approval gates on irreversible steps, human Take Control / Hand Back lease at any moment, full trace persisted per run.

## Quickstart

Fresh Ubuntu Server (local VM or EC2):

```bash
./deploy/setup.sh            # OS deps once (sudo): desktop, Chrome, VNC
# then install rosed from the GitHub release (user, no sudo)
# grab rose_*_x86_64.tar.gz from https://github.com/bmdtouseefh/meta-hackathon/releases/latest, then:
tar xzf rose_*_x86_64.tar.gz && cd rose-* && ./install.sh
# or in one go from a checkout: ./deploy/setup.sh --tarball=dist/rose_*_x86_64.tar.gz
```

Tunnel in (localhost-only is the security boundary — never expose the ports):

```bash
ssh -N -L 8787:127.0.0.1:8787 <vm>   # Web UI + API + MCP
ssh -N -L 5901:127.0.0.1:5901 <vm>   # VNC fallback (works even if rosed is down)
```

Open `http://127.0.0.1:8787`: live gridded desktop view, instruction box, run timeline, approvals, Skill Studio.

First run (pick one — the key is never written to disk, logs, or traces):

1. **With a Muse key:** click **Setup → Keys** in the UI and paste it (lives in server memory for the session only), *or* `export ROSE_MUSE_API_KEY=...` before launching `rosed`.
2. **Without a key:** set `brain = "fake"` in Setup → Config (or `config.toml`) to click through scripted runs offline.

Then either type an instruction and hit Run, or record yourself doing the task while narrating it, upload the video, hit Generate, and review the drafted skill in Skill Studio before replaying it with new inputs.

Update an existing VM:

```bash
./deploy/deploy.sh <user@host>   # from a checkout: build, scp, install
# or on the VM: download the newer rose_*_x86_64.tar.gz from Releases, then tar xzf + ./install.sh
```

Offline dev (no X server, no API key):

```bash
cp config/rose.example.toml ~/.config/rose/config.toml  # set backend = "fake", brain = "fake"
cargo run -p rosed
```

### Let your coding agent drive it

```json
{ "mcpServers": { "rose-desktop": {
  "url": "http://127.0.0.1:8787/mcp", "transport": "streamable-http" } } }
```

Tool loop: `test.start` → `desktop.screenshot` (gridded — pass a cell like `g14,3` straight to the action tools) → act → `test.finish` with a verdict. `GET /mcp/tools` lists the catalog. `shell.exec` (30s timeout, destructive commands refused) launches the app under test — and lets the agent leave behind scripts that shortcut the GUI next run (query a file/DB instead of reading a screen). `skill.update` lets it fold those shortcuts back into the skill, so replays get faster and more deterministic. `GET /api/status` reports backend, brain, and MCP tool count.

## The video → skill loop

```
record (narrate while you work) → upload → Generate → review in Skill Studio
→ fix params/approvals → replay on new inputs → read-back verification
```

Literally-typed values stay inline; anything that changes run to run (dates, tickers, thresholds, file paths) becomes a `{{slot}}`. Steps under 0.65 confidence are highlighted for review. Secrets typed during recording become placeholders, never stored. Verification reads the real record (file on disk, new list row) — not an on-screen "Saved ✓" toast.

Reviewed drafts can be saved as named skills in **Skills & workflows** (right rail, under the video panel): rename, preview steps, run any time — running loads the instructions into the composer as a normal run, same loop and approvals. You can also write a skill by hand without a video. Skills persist in SQLite, so they survive restarts.

## Honest trust boundary

- Never leaves the VM: desktop pixels (except model calls below), browser profile/logins, recordings, skills, traces, typed secrets.
- Leaves the VM today: screenshots + goal text to the Muse Spark API (one call per step, one per video), when `brain = "muse"`. For demo recordings, use synthetic data.
- Path to fully local: the execution loop is model-agnostic and Glimmer-compatible — pin the brain to a self-hosted open-weights endpoint (`endpoint`/`model` in config, or `ROSE_MODELS_ENDPOINT`) and only the one-time video draft needs a hosted call.

## Layout

```
apps/rosed/            # composition root: config -> backends -> web+MCP serve
crates/rose-core/      # Action/Frame/config contracts
crates/rose-desktop-x11/  # X11 capture + XTEST input
crates/rose-desktop-fake/ # in-memory backend (tests, offline dev)
crates/rose-models/    # Muse Spark client, video instructions, grounding
crates/rose-governor/  # pure policy: actions -> Allow/Deny/Approve
crates/rose-agent/     # agent loop + Lease (human/agent input ownership)
crates/rose-mcp/       # MCP tool catalog + JSON-RPC dispatch (no I/O itself)
crates/rose-web/       # HTTP API + MCP routes + embedded UI
crates/rose-grid/      # constant grid overlay (server-side aim reference)
crates/rose-store/     # run traces (JSONL + frame files)
ui/                    # index.html + app.js + style.css (embedded in binary)
```

`backend = "x11" | "fake"`. `brain = "muse" | "fake"`. Policy (`require_approval_for`), budgets, and keys come from config/env; typed secrets are never written to traces.

## Next actions

1. Enable OS testing features for Windows and macOS
2. Automate agent deployment through terraform

## Status

Working end to end on a live VM: gridded live view, narrated video → reviewed skill → replay with new parameters, MCP-driven runs from external agents, approvals + takeover mid-run. See `IMPLEMENTATION.md` for the build plan and `CHANGELOG.md` for what's landed.
