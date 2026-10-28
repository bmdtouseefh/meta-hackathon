# CHANGELOG

One line per completed task (IMPLEMENTATION.md, section 11). No secrets, no real screenshots.
- 2026-10-28 deb fix: control.tar.gz members must be archive-rooted (./control, ./postinst); nesting under DEBIAN/ broke dpkg and segfaulted apt's parser.

- 2026-10-28 T0.1 workspace skeleton: 8 crates + apps/rosed compile, `cargo build && cargo test` green, clippy `-D warnings` clean.
- 2026-10-28 T0.2 rose-core contracts: section 3 types/traits (desktop, perception, models, store, events, governor data, config schema) + 7 serde round-trip tests green.
- 2026-10-28 T0.3 config: config/rose.example.toml + loader (file + ROSE_MUSE_API_KEY env, secrets redacted) + Services factories; `rosed --print-config` green.
- 2026-10-28 dep note: `async-trait 0.1` added to rose-core (spec section 3 annotates `#[async_trait]`; native `async fn` in traits is not object-safe for the boxed planners/verifiers the agent loop needs).
- 2026-10-28 T1.1 FakeDesktop: grid + 8 coloured cells, click highlights, action log; 3 tests green, no display needed.
- 2026-10-28 web shell (T1.4 part 1): rose-web serves embedded Tailwind UI + /api/status + /api/frame; FrameSource seam keeps web off backends directly; bind guard (non-loopback needs allow_remote). Verified live: status JSON, 1600x900 JFIF frame.
- 2026-10-28 dep notes: `image 0.25` + `chrono` (fake); `axum 0.8` + `serde_json` + `async-trait 0.1` (web); `tokio 1` (fake/web/rosed).
- 2026-10-28 T1.2 X11Desktop: root-window capture (BGRX->JPEG) + XTEST input (click/dblclick/type/key/scroll/drag/wait) via spawn_blocking; keysym table + scroll/button mapping unit-tested; wired into rosed factory (construction lazy, no display needed).
- 2026-10-28 observability: RUST_LOG env-filter, startup banner (version/bind/backend/brain/display/size, no secrets), debug request logs, warn on frame failures, GET /api/health (ok/version/uptime).
- 2026-10-28 theme: Meta blue #0064E0 primary actions (ui/ + ui-draft); layout untouched.
- 2026-10-28 deploy: `deploy/deploy.sh [user@host]` builds .deb, scps, apt-installs, runs `rosed install`, restarts services, verifies (print-config, unit states, /api/health).
- 2026-10-28 release: workspace version 0.1.1 (apt skips reinstalls at the same version, so every deploy bumps).
- 2026-10-28 T3.1 Muse Spark client: `rose-models::MuseClient::next_action` (goal + history + JPEG -> §6 JSON; OpenAI-compatible chat completions, developer prompt from prompts/brain.md, json_object, retry 429/5xx once, never 400). Fixture tests green, no network. Endpoint/model configurable; key per-step from web Setup (memory) or env.
- 2026-10-28 web key entry: Setup modal -> POST /api/keys (GET status, DELETE clear); value never logged/persisted/returned. Implements amended rule 7 (env/config or web memory).
- 2026-10-28 single-step prototype: POST /api/step {goal, force} -> capture -> brain -> keyword gate -> act. Live-verified with fake brain (acted + done cards). Full loop stays M2.
- 2026-10-28 chat-first UI ported to real ui/ with live wiring (status/frame/keys/step, approval cards, Meta blue). Draft remains the mockup.
- 2026-10-28 release: workspace version 0.1.2.
- 2026-10-28 fix: rosed.service ExecStart pointed at ~/.local/bin/rosed while the .deb installs to /usr/bin/rosed, so the service could never start. Template now defaults to /usr/bin/rosed and `rosed install` rewrites ExecStart to the exact binary it runs from (verified in dry-run).
- 2026-10-28 release: workspace version 0.1.3.
- 2026-10-28 dep notes: `x11rb 0.13 + xtest` (x11); `tracing 0.1` (web/rosed).
