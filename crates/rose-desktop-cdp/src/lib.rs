//! CDP browser backend: accuracy + reliability for browser tasks.
//!
//! Talks to the VM's **existing Chrome** (`google-chrome-stable`) over its
//! remote-debugging endpoint instead of installing Chromium. chromiumoxide
//! speaks CDP to any recent Chrome, so the logged-in profile, cookies and
//! session survive — pixels can't do that and XTEST clicks can't hit a
//! 12px icon twice in a row.
//!
//! Why this beats X11 pixels for web work:
//! - `capture()` is `Page.captureScreenshot` (exact compositor frame, no
//!   VNC/XSHM tear) at JPEG q85 to match the X11 path's bytes.
//! - `act()` is CDP `Input.dispatchMouseEvent` / `insertText` /
//!   `dispatchKeyEvent` — the browser's own input pipeline, no XTEST
//!   race between motion and click, no keysym table gaps.
//! - `elements()` is the live DOM (`getBoundingClientRect`, filtered to
//!   visible interactables) — ground truth, not a vision guess. Element
//!   ids look like `cdp-12`; the loop resolves them same-step to a click
//!   point, exactly like the SoM overlay path.
//!
//! Selection: `CdpDesktop::connect(endpoint)` attaches to the running
//! Chrome. If the endpoint refuses **and** `launch_if_missing` is set, we
//! launch the detected chrome binary headed at 1600x900 on the current
//! `DISPLAY` so it lands on the same X11 desktop the X11 backend sees.

use async_trait::async_trait;
use chromiumoxide::{Browser, BrowserConfig, Page};
use futures::StreamExt;
use rose_core::{
    Action, Actor, DesktopBackend, DesktopInfo, DomOverlay, Element, ElementId, ElementSource,
    Frame, FrameId, MouseButton, Perception, Rect, Result, RoseError, Target,
};
use std::sync::{Arc, Mutex};

// ---------- pure helpers (unit-tested, no browser needed) ----------

/// Stable element id for badge number N (1-based, matching the SoM
/// convention the brain and `resolve_element` use): `cdp-N`.
pub fn element_id_for_index(n: usize) -> String {
    format!("cdp-{n}")
}

/// Parse `cdp-N` back to the 1-based badge number. Anything else is not ours.
pub fn parse_element_index(id: &str) -> Option<usize> {
    id.strip_prefix("cdp-")?.parse::<usize>().ok()
}

/// Centre of a rect, rounded down — the click point for an element.
pub fn center_of(b: Rect) -> (u32, u32) {
    (b.x + b.width / 2, b.y + b.height / 2)
}

/// Shorten a DOM label for the brain trace; keep it one line.
pub fn truncate_label(s: &str, max: usize) -> String {
    let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        return one_line;
    }
    let cut: String = one_line.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// True for keys that modify the next key instead of producing input.
pub fn is_modifier_token(token: &str) -> bool {
    matches!(
        token.to_lowercase().as_str(),
        "ctrl" | "control" | "alt" | "shift" | "super" | "meta" | "win" | "command" | "cmd"
    )
}

/// CDP `modifiers` bitmask: Alt=1, Ctrl=2, Meta/Command=4, Shift=8.
/// Matches `chromiumoxide_cdp` Input.dispatchKeyEvent semantics.
pub fn modifiers_mask(keys: &[String]) -> i64 {
    let mut mask = 0i64;
    for k in keys {
        match k.to_lowercase().as_str() {
            "alt" => mask |= 1,
            "ctrl" | "control" => mask |= 2,
            "super" | "meta" | "win" | "command" | "cmd" => mask |= 4,
            "shift" => mask |= 8,
            _ => {}
        }
    }
    mask
}

/// Normalise our key tokens to CDP `key` names.
pub fn normalize_key_name(token: &str) -> &str {
    match token.to_lowercase().as_str() {
        "return" | "enter" => "Enter",
        "esc" | "escape" => "Escape",
        "back" | "backspace" => "Backspace",
        "del" | "delete" => "Delete",
        "space" | "spacebar" => " ",
        "tab" => "Tab",
        "left" => "ArrowLeft",
        "up" => "ArrowUp",
        "right" => "ArrowRight",
        "down" => "ArrowDown",
        "home" => "Home",
        "end" => "End",
        "pageup" | "page_up" => "PageUp",
        "pagedown" | "page_down" => "PageDown",
        _ => token,
    }
}

/// Candidate chrome binaries, in preference order. The VM ships
/// `google-chrome-stable`; dev laptops vary, so probe several.
pub const CHROME_CANDIDATES: &[&str] = &[
    "google-chrome-stable",
    "google-chrome",
    "chrome",
    "chromium-browser",
    "chromium",
];

/// Pick the first candidate that exists on `PATH` (or honour an explicit
/// override). Pure over an injected lookup so tests stay hermetic.
pub fn pick_binary(override_bin: &str, mut exists: impl FnMut(&str) -> bool) -> Option<String> {
    if !override_bin.trim().is_empty() {
        return Some(override_bin.to_string());
    }
    if let Ok(env) = std::env::var("CHROME_BIN") {
        if !env.trim().is_empty() {
            return Some(env);
        }
    }
    CHROME_CANDIDATES
        .iter()
        .find(|c| exists(c))
        .map(|s| s.to_string())
}

/// True when `which`-style lookup finds the binary.
fn binary_exists(name: &str) -> bool {
    which::which(name).is_ok()
}

// ---------- backend ----------

struct Inner {
    endpoint: String,
    launch_if_missing: bool,
    chrome_binary: String,
    browser: tokio::sync::RwLock<Option<Browser>>,
    /// Last `elements()` result, for resolving `Target::Element(cdp-N)`.
    cache: Mutex<Vec<Element>>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

/// CDP browser backend. Clone shares the same `Browser` connection.
#[derive(Debug, Clone)]
pub struct CdpDesktop {
    inner: Arc<Inner>,
}

fn cdp_err(e: impl ToString) -> RoseError {
    RoseError::Desktop(format!("cdp: {}", e.to_string()))
}

fn spawn_handler(mut handler: chromiumoxide::Handler) {
    tokio::spawn(async move {
        loop {
            match handler.next().await {
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "cdp handler event error");
                }
                None => break,
            }
        }
    });
}

/// Dial the endpoint; on refusal optionally launch headed Chrome.
/// Returns a live `Browser` with its event pump spawned.
async fn dial(endpoint: &str, launch_if_missing: bool, chrome_binary: &str) -> Result<Browser> {
    match Browser::connect(endpoint.to_string()).await {
        Ok((mut browser, handler)) => {
            spawn_handler(handler);
            // Pull in tabs that predated our connection (restored session).
            let _ = browser.fetch_targets().await;
            return Ok(browser);
        }
        Err(e) if !launch_if_missing => {
            return Err(cdp_err(e));
        }
        Err(e) => {
            tracing::warn!(error = %e, endpoint = %endpoint, "cdp connect failed, launching chrome");
        }
    }
    let binary = pick_binary(chrome_binary, binary_exists).ok_or_else(|| {
        RoseError::Desktop(
            "cdp: no chrome binary found (tried google-chrome-stable, google-chrome, chromium-browser, chromium; set CHROME_BIN or [cdp].chrome_binary)".to_string(),
        )
    })?;
    let (browser, handler) = Browser::launch(
        BrowserConfig::builder()
            .chrome_executable(binary)
            .with_head()
            .window_size(1600, 900)
            .arg("--disable-gpu")
            .arg("--disable-dev-shm-usage")
            .arg("--no-first-run")
            .build()
            .map_err(|e| RoseError::Desktop(format!("cdp config: {e}")))?,
    )
    .await
    .map_err(cdp_err)?;
    spawn_handler(handler);
    Ok(browser)
}

impl CdpDesktop {
    /// Sync constructor for the composition root: no I/O here, so `rosed`
    /// starts even when Chrome isn't up yet. The CDP handshake happens
    /// lazily on first `capture`/`act`/`elements` and reconnects if the
    /// socket drops.
    pub fn new(endpoint: &str, launch_if_missing: bool, chrome_binary: &str) -> Self {
        Self {
            inner: Arc::new(Inner {
                endpoint: endpoint.to_string(),
                launch_if_missing,
                chrome_binary: chrome_binary.to_string(),
                browser: tokio::sync::RwLock::new(None),
                cache: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Connect, optionally launching headed Chrome on failure.
    ///
    /// Launch uses the detected `google-chrome*` binary on the current
    /// `DISPLAY` at 1600x900 so the window lands where the X11 backend
    /// (and VNC) can also see it — two views of the same browser.
    /// Eager variant for tests and CLIs that want fail-fast.
    pub async fn connect_or_launch(
        endpoint: &str,
        launch_if_missing: bool,
        chrome_binary_override: &str,
    ) -> Result<Self> {
        let this = Self::new(endpoint, launch_if_missing, chrome_binary_override);
        this.ensure_browser().await?;
        Ok(this)
    }

    pub fn endpoint(&self) -> &str {
        &self.inner.endpoint
    }

    /// Lazily connect (or reconnect). Idempotent: concurrent callers share
    /// one dial via the write lock; a failed handshake leaves the slot
    /// empty so the next call retries — Chrome restarts self-heal.
    async fn ensure_browser(&self) -> Result<()> {
        if self.inner.browser.read().await.is_some() {
            return Ok(());
        }
        let mut slot = self.inner.browser.write().await;
        if slot.is_some() {
            return Ok(());
        }
        let browser = dial(
            &self.inner.endpoint,
            self.inner.launch_if_missing,
            &self.inner.chrome_binary,
        )
        .await?;
        *slot = Some(browser);
        Ok(())
    }

    /// Drop the cached connection so the next call redials.
    async fn evict_browser(&self) {
        *self.inner.browser.write().await = None;
    }

    /// The page the agent should drive: last http(s) tab, else last page,
    /// else a fresh `about:blank`. Heuristic beats "first target" because
    /// Chrome keeps stale/devtools targets around.
    async fn active_page(&self) -> Result<Page> {
        self.ensure_browser().await?;
        // Hold the read guard only to get the page list; `Page` is an
        // independent handle (Arc inside) so it outlives the guard.
        let pages = {
            let guard = self.inner.browser.read().await;
            let browser = guard.as_ref().expect("ensured");
            match browser.pages().await {
                Ok(p) => p,
                Err(e) => {
                    drop(guard);
                    self.evict_browser().await;
                    return Err(cdp_err(e));
                }
            }
        };
        if !pages.is_empty() {
            // Prefer the most recently opened http(s) page (iterate reversed).
            for page in pages.iter().rev() {
                if let Ok(Some(url)) = page.url().await {
                    if url.starts_with("http://") || url.starts_with("https://") {
                        return Ok(page.clone());
                    }
                }
            }
            return Ok(pages.last().cloned().expect("non-empty"));
        }
        // No pages: create one. Needs the browser again — re-lock briefly.
        let page = {
            let guard = self.inner.browser.read().await;
            let browser = guard.as_ref().expect("ensured");
            browser.new_page("about:blank").await.map_err(cdp_err)?
        };
        Ok(page)
    }

    /// Visible canvas area as a share of the viewport. Best-effort:
    /// anything failing (mid-navigation, no page yet) counts as 0.0.
    async fn canvas_coverage(&self) -> f64 {
        let page = match self.active_page().await {
            Ok(p) => p,
            Err(_) => return 0.0,
        };
        let ratio = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            page.evaluate(CANVAS_COVERAGE_JS),
        )
        .await
        .ok()
        .and_then(|r| r.map_err(cdp_err).ok())
        .and_then(|res| res.into_value::<f64>().ok())
        .unwrap_or(0.0);
        if ratio.is_finite() {
            ratio.clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// Resolve any `Target` to CSS pixels, via the DOM cache for elements.
    fn resolve_point(&self, target: &Target) -> Result<(f64, f64)> {
        match target {
            Target::Point { x, y } => Ok((*x as f64, *y as f64)),
            Target::Element(id) => {
                let n = parse_element_index(&id.0).ok_or_else(|| {
                    RoseError::Desktop(format!(
                        "cdp: element {id} is not a DOM id (want cdp-N from elements())"
                    ))
                })?;
                // Badge numbers are 1-based (SoM convention); the cache is
                // 0-based, so cdp-N is cache[N-1].
                let pos = n.checked_sub(1).ok_or_else(|| {
                    RoseError::Desktop(format!("cdp: bad element number in {id}"))
                })?;
                let cache = self.inner.cache.lock().expect("cdp cache");
                let el = cache.get(pos).ok_or_else(|| {
                    RoseError::Desktop(format!(
                        "cdp: element {id} not in last elements() ({} cached) — re-ground first",
                        cache.len()
                    ))
                })?;
                let (x, y) = center_of(el.bounds);
                Ok((x as f64, y as f64))
            }
        }
    }

    /// Screenshot bytes at JPEG q85 (matches the X11 backend's encoding).
    async fn screenshot_jpeg(&self) -> Result<(Vec<u8>, u32, u32)> {
        use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
        use chromiumoxide::page::ScreenshotParams;

        let page = self.active_page().await?;
        let params = ScreenshotParams::builder()
            .format(CaptureScreenshotFormat::Jpeg)
            .quality(85)
            .build();
        let bytes = page.screenshot(params).await.map_err(cdp_err)?;
        // Dimensions from layout metrics; fall back to the 1600x900
        // server geometry when the page is mid-navigation.
        let (w, h) = match page.layout_metrics().await {
            Ok(m) => {
                let w = m.css_layout_viewport.client_width.max(1) as u32;
                let h = m.css_layout_viewport.client_height.max(1) as u32;
                if w >= 100 && h >= 100 {
                    (w, h)
                } else {
                    (1600, 900)
                }
            }
            Err(_) => (1600, 900),
        };
        Ok((bytes, w, h))
    }
}

// ---------- DesktopBackend: same seam as X11, CDP underneath ----------

#[async_trait]
impl DesktopBackend for CdpDesktop {
    async fn info(&self) -> Result<DesktopInfo> {
        let page = self.active_page().await?;
        let url = page.url().await.map_err(cdp_err)?;
        let title = page.get_title().await.map_err(cdp_err)?;
        let active_window = match (title, url) {
            (Some(t), Some(u)) if !t.is_empty() => Some(format!("{t} — {u}")),
            (Some(t), _) if !t.is_empty() => Some(t),
            (_, Some(u)) => Some(u),
            _ => Some("Chrome (CDP)".to_string()),
        };
        // Real viewport when available; the fixed fallback keeps the
        // agent's 1:1 coordinate assumption intact.
        let (width, height) = match page.layout_metrics().await {
            Ok(m) => (
                (m.css_layout_viewport.client_width.max(1)) as u32,
                (m.css_layout_viewport.client_height.max(1)) as u32,
            ),
            Err(_) => (1600, 900),
        };
        Ok(DesktopInfo {
            display: format!("cdp:{}", self.inner.endpoint),
            width,
            height,
            active_window,
        })
    }

    async fn capture(&self) -> Result<Frame> {
        // 15s cap: a hung renderer must fail the step, not the run.
        let (jpeg, width, height) =
            tokio::time::timeout(std::time::Duration::from_secs(15), self.screenshot_jpeg())
                .await
                .map_err(|_| RoseError::Desktop("cdp: screenshot timed out".to_string()))??;
        Ok(Frame {
            id: FrameId::new(),
            captured_at: chrono::Utc::now(),
            width,
            height,
            jpeg,
            active_window: self.info().await.ok().and_then(|i| i.active_window),
        })
    }

    async fn act(&self, action: &Action, _actor: Actor) -> Result<()> {
        use chromiumoxide::cdp::browser_protocol::input::{
            DispatchMouseEventParams, DispatchMouseEventType, InsertTextParams,
            MouseButton as CdpMouseButton,
        };
        use chromiumoxide::layout::Point as CdpPoint;

        // Every CDP round-trip gets a 15s budget; Verify-on-next-screenshot
        // (the agent loop) is what confirms the effect.
        let act_fut = async {
            let page = self.active_page().await?;
            match action {
                Action::Click { target, button } => {
                    let (x, y) = self.resolve_point(target)?;
                    tracing::info!(x = x as u32, y = y as u32, ?button, "cdp click");
                    // page.click() is always left; other buttons use raw dispatch.
                    if *button == MouseButton::Left {
                        page.click(CdpPoint::new(x, y)).await.map_err(cdp_err)?;
                    } else {
                        let btn = match button {
                            MouseButton::Left => CdpMouseButton::Left,
                            MouseButton::Middle => CdpMouseButton::Middle,
                            MouseButton::Right => CdpMouseButton::Right,
                        };
                        let press = DispatchMouseEventParams::builder()
                            .x(x)
                            .y(y)
                            .button(btn)
                            .r#type(DispatchMouseEventType::MousePressed)
                            .click_count(1)
                            .build()
                            .map_err(|e| RoseError::Desktop(format!("cdp click build: {e}")))?;
                        page.execute(press).await.map_err(cdp_err)?;
                        let btn = match button {
                            MouseButton::Left => CdpMouseButton::Left,
                            MouseButton::Middle => CdpMouseButton::Middle,
                            MouseButton::Right => CdpMouseButton::Right,
                        };
                        let release = DispatchMouseEventParams::builder()
                            .x(x)
                            .y(y)
                            .button(btn)
                            .r#type(DispatchMouseEventType::MouseReleased)
                            .click_count(1)
                            .build()
                            .map_err(|e| RoseError::Desktop(format!("cdp click build: {e}")))?;
                        page.execute(release).await.map_err(cdp_err)?;
                    }
                }
                Action::DoubleClick { target } => {
                    use chromiumoxide::types::ClickOptions;
                    let (x, y) = self.resolve_point(target)?;
                    tracing::info!(x = x as u32, y = y as u32, "cdp double-click");
                    let opts = ClickOptions::builder().click_count(2).build();
                    page.click_with(CdpPoint::new(x, y), opts)
                        .await
                        .map_err(cdp_err)?;
                }
                Action::Type { text } => {
                    tracing::info!(chars = text.chars().count(), "cdp type (insertText)");
                    let cmd = InsertTextParams::builder()
                        .text(text.as_str())
                        .build()
                        .map_err(|e| RoseError::Desktop(format!("cdp type build: {e}")))?;
                    page.execute(cmd).await.map_err(cdp_err)?;
                }
                Action::PressKey { keys } => {
                    tracing::info!(?keys, "cdp pressKey");
                    press_keys_cdp(&page, keys).await?;
                }
                Action::Scroll { target, dy } => {
                    let (x, y) = self.resolve_point(target)?;
                    tracing::info!(x = x as u32, y = y as u32, dy, "cdp scroll");
                    let cmd = DispatchMouseEventParams::builder()
                        .x(x)
                        .y(y)
                        .r#type(DispatchMouseEventType::MouseWheel)
                        .delta_y(*dy as f64)
                        .build()
                        .map_err(|e| RoseError::Desktop(format!("cdp scroll build: {e}")))?;
                    page.execute(cmd).await.map_err(cdp_err)?;
                }
                Action::Drag { from, to } => {
                    let (x0, y0) = self.resolve_point(from)?;
                    let (x1, y1) = self.resolve_point(to)?;
                    tracing::info!(
                        from = format!("{},{}", x0 as u32, y0 as u32),
                        to = format!("{},{}", x1 as u32, y1 as u32),
                        "cdp drag"
                    );
                    page.move_mouse(CdpPoint::new(x0, y0))
                        .await
                        .map_err(cdp_err)?;
                    let press = DispatchMouseEventParams::builder()
                        .x(x0)
                        .y(y0)
                        .button(CdpMouseButton::Left)
                        .r#type(DispatchMouseEventType::MousePressed)
                        .click_count(1)
                        .build()
                        .map_err(|e| RoseError::Desktop(format!("cdp drag build: {e}")))?;
                    page.execute(press).await.map_err(cdp_err)?;
                    page.move_mouse(CdpPoint::new(x1, y1))
                        .await
                        .map_err(cdp_err)?;
                    let release = DispatchMouseEventParams::builder()
                        .x(x1)
                        .y(y1)
                        .button(CdpMouseButton::Left)
                        .r#type(DispatchMouseEventType::MouseReleased)
                        .click_count(1)
                        .build()
                        .map_err(|e| RoseError::Desktop(format!("cdp drag build: {e}")))?;
                    page.execute(release).await.map_err(cdp_err)?;
                }
                Action::Wait { ms } => {
                    tokio::time::sleep(std::time::Duration::from_millis((*ms).min(10_000) as u64))
                        .await;
                }
            }
            Ok::<(), RoseError>(())
        };
        tokio::time::timeout(std::time::Duration::from_secs(15), act_fut)
            .await
            .map_err(|_| RoseError::Desktop("cdp: action timed out".to_string()))?
    }

    async fn dom_overlay(&self, _frame_jpeg: &[u8]) -> Result<Option<DomOverlay>> {
        if canvas_needs_pixels(self.canvas_coverage().await) {
            tracing::info!("cdp: canvas-dominated page, leaving grounding to pixels");
            return Ok(None);
        }
        let elements = match self.elements_via_ax().await {
            Some(els) if !els.is_empty() => els,
            // Unreachable or empty tree: the Stepper's sidecar fallback
            // covers it; an empty overlay would only hide the screenshot.
            _ => return Ok(None),
        };
        Ok(Some(DomOverlay { elements }))
    }
}

/// Dispatch a key chord via CDP: modifiers down, main key press, modifiers up.
///
/// Single keys send keyDown+keyUp; chords hold modifiers across the main
/// press. Unknown tokens fall back to their first char (Latin-1) so plain
/// letters keep working even if the name table missed them.
async fn press_keys_cdp(page: &Page, keys: &[String]) -> Result<()> {
    if keys.is_empty() {
        return Err(RoseError::Desktop("cdp: empty PressKey".to_string()));
    }
    let modifiers = modifiers_mask(keys);
    let mains: Vec<String> = keys
        .iter()
        .filter(|k| !is_modifier_token(k))
        .map(|k| {
            let mut name = normalize_key_name(k).to_string();
            if name.is_empty() {
                name = k.clone();
            }
            name
        })
        .collect();
    // Modifier-only (e.g. ["Shift"]) still needs a tap to take effect.
    let to_send: Vec<String> = if mains.is_empty() {
        vec![normalize_key_name(&keys[0]).to_string()]
    } else {
        mains
    };
    for name in &to_send {
        send_key(page, name, modifiers).await?;
    }
    Ok(())
}

/// One keyDown+keyUp pair. Owned `String` avoids closure-lifetime issues.
async fn send_key(page: &Page, key_name: &str, mods: i64) -> Result<()> {
    use chromiumoxide::cdp::browser_protocol::input::{
        DispatchKeyEventParams, DispatchKeyEventType,
    };
    let down = DispatchKeyEventParams::builder()
        .r#type(DispatchKeyEventType::KeyDown)
        .key(key_name)
        .modifiers(mods)
        .build()
        .map_err(|e| RoseError::Desktop(format!("cdp key build: {e}")))?;
    page.execute(down).await.map_err(cdp_err)?;
    let up = DispatchKeyEventParams::builder()
        .r#type(DispatchKeyEventType::KeyUp)
        .key(key_name)
        .modifiers(mods)
        .build()
        .map_err(|e| RoseError::Desktop(format!("cdp key build: {e}")))?;
    page.execute(up).await.map_err(cdp_err)?;
    Ok(())
}

// ---------- Perception: the accessibility tree is the ground truth ----------
//
// Token math is why this uses the AX tree first, not the DOM: one AX node
// serialises as `button "Submit" [cdp-3] (120,45 80x24)` — roughly 10-15
// tokens — versus a DOM scrape plus an annotated screenshot (image tokens
// every step). The AX tree is also what assistive tech sees, so it covers
// shadow DOM, custom elements and canvas fallbacks that querySelector
// misses, with browser-computed accessible names instead of our innerText
// guess. Geometry still comes from DOM.getBoxModel (DevTools-accurate),
// resolved per actionable node via its backendDOMNodeId.

/// Actionable AX roles. Everything else is text/layout — expensive noise
/// in the prompt and unclickable anyway.
pub fn is_actionable_role(role: &str) -> bool {
    matches!(
        role.to_lowercase().as_str(),
        "button"
            | "link"
            | "textbox"
            | "searchbox"
            | "combobox"
            | "checkbox"
            | "radio"
            | "switch"
            | "menuitem"
            | "menuitemcheckbox"
            | "menuitemradio"
            | "tab"
            | "slider"
            | "spinbutton"
    )
}

/// Human-readable string out of an AX value's JSON payload.
pub fn ax_string(v: Option<&AxValue>) -> String {
    let v = match v {
        Some(v) => v,
        None => return String::new(),
    };
    match &v.value {
        Some(serde_json::Value::String(s)) => s.trim().to_string(),
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// Quad (8 floats: 4 x,y corners) -> bounding `Rect`. None when degenerate.
pub fn quad_to_rect(quad: &[f64]) -> Option<Rect> {
    if quad.len() < 8 {
        return None;
    }
    let xs = [quad[0], quad[2], quad[4], quad[6]];
    let ys = [quad[1], quad[3], quad[5], quad[7]];
    let (x0, x1) = (
        xs.iter().cloned().fold(f64::INFINITY, f64::min),
        xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
    );
    let (y0, y1) = (
        ys.iter().cloned().fold(f64::INFINITY, f64::min),
        ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
    );
    if !x0.is_finite() || !y0.is_finite() || x1 <= x0 || y1 <= y0 {
        return None;
    }
    if x1 - x0 < 2.0 || y1 - y0 < 2.0 {
        return None;
    }
    Some(Rect {
        x: x0.max(0.0) as u32,
        y: y0.max(0.0) as u32,
        width: (x1 - x0).max(1.0) as u32,
        height: (y1 - y0).max(1.0) as u32,
    })
}

/// Max AX-grounded elements per observation: bounds the prompt (~100 x
/// ~12 tokens) and the BoxModel fan-out (100 local CDP calls, ~ms each).
pub const MAX_AX_ELEMENTS: usize = 100;

/// Max elements listed per observation: past ~40 the list buries the
/// signal and the brain falls back to pixel guessing. Named controls
/// win the cut (stable order otherwise).
pub const MAX_OVERLAY_ELEMENTS: usize = 40;

/// Above this share of the viewport covered by <canvas>, the AX tree is
/// mostly chrome around a painting the tree cannot see (charts, games,
/// maps, editors) — pixel grounding covers what DOM listing cannot.
pub const CANVAS_FALLBACK_COVERAGE: f64 = 0.35;

/// General, not site-specific: canvas-dominated pages are opaque to the
/// accessibility tree, so the Stepper should ground them from pixels.
pub fn canvas_needs_pixels(coverage: f64) -> bool {
    coverage >= CANVAS_FALLBACK_COVERAGE
}

/// Visible canvas area as a share of the viewport (0..1), all canvases
/// unioned by bounding box. Best-effort: any failure means 0.0, i.e. the
/// DOM path runs as before.
const CANVAS_COVERAGE_JS: &str = r#"(() => {
  try {
    const vw = window.innerWidth || 1, vh = window.innerHeight || 1;
    let a = 0;
    for (const c of document.querySelectorAll("canvas")) {
      try {
        const r = c.getBoundingClientRect();
        const w = Math.max(0, Math.min(r.right, vw) - Math.max(r.left, 0));
        const h = Math.max(0, Math.min(r.bottom, vh) - Math.max(r.top, 0));
        a += w * h;
      } catch (_) {}
    }
    return a / (vw * vh);
  } catch (_) { return 0; }
})()"#;

/// Order elements for the overlay: named controls first (stable
/// document order within each group), truncate to `cap`, renumber
/// badges 1..N in final order.
pub fn rank_elements(els: Vec<Element>, cap: usize) -> Vec<Element> {
    let (mut named, unnamed): (Vec<Element>, Vec<Element>) =
        els.into_iter().partition(|el| el.label.contains(": "));
    named.extend(unnamed);
    named.truncate(cap);
    for (i, el) in named.iter_mut().enumerate() {
        el.id = ElementId(element_id_for_index(i + 1));
    }
    named
}

use chromiumoxide::cdp::browser_protocol::accessibility::AxValue;

/// Raw DOM hit from the evaluate JS below.
#[derive(Debug, Clone, serde::Deserialize)]
struct RawHit {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    tag: String,
    text: String,
}

/// JS that lists visible interactables. Runs in page context; deliberately
/// conservative (no iframes, no shadow DOM in v1 — those come back as
/// "no elements, use pixels" instead of wrong boxes).
const DOM_QUERY_JS: &str = r#"(() => {
  const sel = "a[href], button, input, select, textarea, [role=button], [role=link], [role=checkbox], [role=menuitem], summary, [onclick]";
  const out = [];
  for (const el of document.querySelectorAll(sel)) {
    try {
      const r = el.getBoundingClientRect();
      if (r.width < 2 || r.height < 2) continue;
      if (r.bottom < 0 || r.right < 0) continue;
      if (r.top > window.innerHeight || r.left > window.innerWidth) continue;
      const st = getComputedStyle(el);
      if (st.visibility === "hidden" || st.display === "none") continue;
      const text = (el.innerText || el.value || el.getAttribute("aria-label") || el.getAttribute("title") || el.getAttribute("placeholder") || el.tagName || "").trim().slice(0, 80);
      out.push({x: r.x, y: r.y, width: r.width, height: r.height, tag: (el.tagName || "?").toLowerCase(), text});
      if (out.length >= 200) break;
    } catch (_) {}
  }
  return out;
})()"#;

#[async_trait]
impl Perception for CdpDesktop {
    async fn elements(&self, _frame: &Frame, _hint: Option<&str>) -> Result<Vec<Element>> {
        // AX tree first (token-cheap, semantic); DOM scrape as fallback.
        if let Some(elements) = self.elements_via_ax().await {
            if !elements.is_empty() {
                *self.inner.cache.lock().expect("cdp cache") = elements.clone();
                return Ok(elements);
            }
            tracing::info!("cdp: AX tree yielded nothing usable, falling back to DOM scrape");
        }
        let elements = self.elements_via_dom().await?;
        *self.inner.cache.lock().expect("cdp cache") = elements.clone();
        Ok(elements)
    }
}

impl CdpDesktop {
    /// AX-tree grounding: role + accessible name per node, geometry via
    /// `DOM.getBoxModel(backendDOMNodeId)`. Returns None when the tree is
    /// unreachable so the caller falls back to the DOM scrape.
    async fn elements_via_ax(&self) -> Option<Vec<Element>> {
        use chromiumoxide::cdp::browser_protocol::accessibility::{
            AxPropertyName, GetFullAxTreeParams,
        };
        use chromiumoxide::cdp::browser_protocol::dom::GetBoxModelParams;

        let page = self.active_page().await.ok()?;
        let tree = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            page.execute(GetFullAxTreeParams::builder().depth(100).build()),
        )
        .await
        .ok()?
        .map_err(cdp_err)
        .ok()?;
        // Viewport for offscreen filtering; fall back to 1600x900 geometry.
        let (vw, vh) = match page.layout_metrics().await {
            Ok(m) => (
                m.css_layout_viewport.client_width.max(1) as f64,
                m.css_layout_viewport.client_height.max(1) as f64,
            ),
            Err(_) => (1600.0, 900.0),
        };
        let mut out = Vec::new();
        for node in &tree.nodes {
            if node.ignored {
                continue;
            }
            let role = ax_string(node.role.as_ref()).to_lowercase();
            if !is_actionable_role(&role) {
                continue;
            }
            // Skip disabled nodes: unactionable, pure prompt noise.
            if node.properties.as_ref().is_some_and(|props| {
                props.iter().any(|p| {
                    matches!(p.name, AxPropertyName::Disabled)
                        && matches!(&p.value.value, Some(serde_json::Value::Bool(true)))
                })
            }) {
                continue;
            }
            let backend = match node.backend_dom_node_id {
                Some(id) => id,
                None => continue,
            };
            // Name, else value (textboxes), else description — truncated.
            let mut name = ax_string(node.name.as_ref());
            if name.is_empty() {
                name = ax_string(node.value.as_ref());
            }
            if name.is_empty() {
                name = ax_string(node.description.as_ref());
            }
            let label = {
                let t = truncate_label(&name, 80);
                if t.is_empty() {
                    // Nameless control: still useful when the role is rare
                    // (a lone slider), but skip nameless links/buttons —
                    // they are usually icon duplicates of a named sibling.
                    if role == "link" || role == "button" {
                        continue;
                    }
                    role.clone()
                } else {
                    format!("{role}: {t}")
                }
            };
            // Geometry: one local CDP call per node, 2s cap each.
            let geom = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                page.execute(
                    GetBoxModelParams::builder()
                        .backend_node_id(backend)
                        .build(),
                ),
            )
            .await
            .ok()
            .and_then(|r| r.map_err(cdp_err).ok())
            .and_then(|ret| quad_to_rect(ret.model.border.inner()));
            let bounds = match geom {
                Some(b) => b,
                None => continue,
            };
            // Offscreen / zero-area: unclickable, skip.
            if bounds.x as f64 >= vw || bounds.y as f64 >= vh {
                continue;
            }
            out.push(Element {
                id: ElementId(String::new()), // numbered by rank_elements below
                label,
                role: Some(role),
                bounds,
                confidence: 1.0,
                source: ElementSource::Cdp,
            });
            if out.len() >= MAX_AX_ELEMENTS {
                break;
            }
        }
        Some(rank_elements(out, MAX_OVERLAY_ELEMENTS))
    }

    /// DOM-scrape fallback: `getBoundingClientRect` over visible
    /// interactables. Less semantic than the AX tree (no shadow DOM, guessed
    /// names) but needs only one round-trip, so it covers pages where the
    /// AX tree is disabled or empty.
    async fn elements_via_dom(&self) -> Result<Vec<Element>> {
        let page = self.active_page().await?;
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            page.evaluate(DOM_QUERY_JS),
        )
        .await
        .map_err(|_| RoseError::Desktop("cdp: elements() timed out".to_string()))?
        .map_err(cdp_err)?;
        let hits: Vec<RawHit> = res
            .into_value()
            .map_err(|e| RoseError::Desktop(format!("cdp elements decode: {e}")))?;
        let els: Vec<Element> = hits
            .into_iter()
            .map(|h| {
                let label = {
                    let t = truncate_label(&h.text, 80);
                    if t.is_empty() {
                        h.tag.clone()
                    } else {
                        format!("{}: {}", h.tag, t)
                    }
                };
                Element {
                    id: ElementId(String::new()), // numbered by rank_elements below
                    label,
                    role: Some(h.tag),
                    bounds: Rect {
                        x: h.x.max(0.0) as u32,
                        y: h.y.max(0.0) as u32,
                        width: h.width.max(1.0) as u32,
                        height: h.height.max(1.0) as u32,
                    },
                    confidence: 1.0,
                    source: ElementSource::Cdp,
                }
            })
            .collect();
        Ok(rank_elements(els, MAX_OVERLAY_ELEMENTS))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn element_ids_roundtrip() {
        assert_eq!(element_id_for_index(1), "cdp-1");
        assert_eq!(element_id_for_index(199), "cdp-199");
        assert_eq!(parse_element_index("cdp-12"), Some(12));
        assert_eq!(parse_element_index("cdp-x"), None);
        assert_eq!(parse_element_index("e-7"), None);
        assert_eq!(parse_element_index(""), None);
    }

    #[test]
    fn center_of_rect() {
        let b = Rect {
            x: 100,
            y: 200,
            width: 90,
            height: 30,
        };
        assert_eq!(center_of(b), (145, 215));
    }

    #[test]
    fn labels_truncated_one_line() {
        assert_eq!(truncate_label("  Click\n  me  now ", 80), "Click me now");
        let long = "x".repeat(200);
        let cut = truncate_label(&long, 80);
        assert!(cut.chars().count() <= 80);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn modifier_detection_and_mask() {
        assert!(is_modifier_token("ctrl"));
        assert!(is_modifier_token("Shift"));
        assert!(!is_modifier_token("Return"));
        let mask = modifiers_mask(&["ctrl".to_string(), "Shift".to_string(), "a".to_string()]);
        assert_eq!(mask, 2 | 8);
        assert_eq!(modifiers_mask(&["alt".to_string()]), 1);
        assert_eq!(modifiers_mask(&["a".to_string()]), 0);
    }

    #[test]
    fn key_names_normalised() {
        assert_eq!(normalize_key_name("Return"), "Enter");
        assert_eq!(normalize_key_name("esc"), "Escape");
        assert_eq!(normalize_key_name("left"), "ArrowLeft");
        assert_eq!(normalize_key_name("F5"), "F5");
    }

    #[test]
    fn chrome_pick_order_and_override() {
        // Override wins without touching the filesystem.
        let got = pick_binary("/opt/chrome", |_| panic!("must not probe"));
        assert_eq!(got, Some("/opt/chrome".to_string()));
        // First existing candidate wins.
        let got = pick_binary("", |name| name == "chromium-browser");
        assert_eq!(got, Some("chromium-browser".to_string()));
        // Nothing found -> None (caller turns this into a clear error).
        let got = pick_binary("", |_| false);
        assert_eq!(got, None);
    }

    #[test]
    fn chrome_candidates_cover_vm_and_laptops() {
        assert!(CHROME_CANDIDATES.contains(&"google-chrome-stable"));
        assert!(CHROME_CANDIDATES.contains(&"chromium-browser"));
    }

    #[test]
    fn actionable_roles_cover_controls_not_text() {
        for role in [
            "button",
            "link",
            "textbox",
            "searchbox",
            "combobox",
            "checkbox",
            "tab",
        ] {
            assert!(is_actionable_role(role), "{role} should ground");
            assert!(is_actionable_role(&role.to_uppercase()), "case-insensitive");
        }
        for role in ["StaticText", "generic", "image", "heading", ""] {
            assert!(!is_actionable_role(role), "{role} should not ground");
        }
    }

    #[test]
    fn ax_strings_extract_json_payloads() {
        use chromiumoxide::cdp::browser_protocol::accessibility::{AxValue, AxValueType};
        let named = AxValue {
            r#type: AxValueType::ComputedString,
            value: Some(serde_json::Value::String("  Submit ".to_string())),
            related_nodes: None,
            sources: None,
        };
        assert_eq!(ax_string(Some(&named)), "Submit");
        assert_eq!(ax_string(None), "");
        let noval = AxValue::new(AxValueType::String);
        assert_eq!(ax_string(Some(&noval)), "");
    }

    #[test]
    fn quads_become_rects_or_nothing() {
        // Border quad: (10,20) (90,20) (90,50) (10,50).
        let r = quad_to_rect(&[10.0, 20.0, 90.0, 20.0, 90.0, 50.0, 10.0, 50.0]).expect("rect");
        assert_eq!((r.x, r.y, r.width, r.height), (10, 20, 80, 30));
        // Degenerate: too few points, zero area, hairlines.
        assert_eq!(quad_to_rect(&[0.0, 0.0]), None);
        assert_eq!(
            quad_to_rect(&[5.0, 5.0, 5.0, 5.0, 5.0, 5.0, 5.0, 5.0]),
            None
        );
        assert_eq!(
            quad_to_rect(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0]),
            None
        );
    }

    fn bare(label: &str) -> Element {
        Element {
            id: ElementId("cdp-?".to_string()),
            label: label.to_string(),
            role: None,
            bounds: Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            },
            confidence: 1.0,
            source: ElementSource::Cdp,
        }
    }

    #[test]
    fn canvas_pages_route_to_pixels() {
        assert!(!canvas_needs_pixels(0.0));
        assert!(!canvas_needs_pixels(0.34));
        assert!(canvas_needs_pixels(0.35));
        assert!(canvas_needs_pixels(0.9));
        assert!(!canvas_needs_pixels(f64::NAN));
        assert!(canvas_needs_pixels(2.0), "clamped upstream, still pixels");
    }

    #[test]
    fn ranking_prefers_named_caps_and_renumbers() {
        let els = vec![
            bare("link"),
            bare("button: A"),
            bare("button"),
            bare("link: B"),
        ];
        let ranked = rank_elements(els, 3);
        // Named first (stable), capped, badges 1-based in final order.
        assert_eq!(
            ranked.iter().map(|e| e.id.0.clone()).collect::<Vec<_>>(),
            vec!["cdp-1", "cdp-2", "cdp-3"]
        );
        assert_eq!(ranked[0].label, "button: A");
        assert_eq!(ranked[1].label, "link: B");
        assert_eq!(ranked[2].label, "link");
    }

    #[tokio::test]
    async fn connect_refused_fails_fast_without_launch() {
        // Nothing listens on 9 (discard port): must error, never hang.
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            CdpDesktop::connect_or_launch("http://127.0.0.1:9", false, ""),
        )
        .await;
        assert!(res.is_ok(), "outer timeout fired — connect hung");
        assert!(res.unwrap().is_err());
    }
}
