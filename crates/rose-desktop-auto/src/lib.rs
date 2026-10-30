//! Router backend: X11 everywhere, CDP when a Chrome window is frontmost.
//!
//! The all-or-nothing backends both fail half the job: `x11` pixel-clicks
//! 12px web icons unreliably, while `cdp` cannot see shell-launched apps,
//! native dialogs, or a second Chrome on another profile — and CDP has no
//! omnibox, so the classic `ctrl+l, type URL, Return` flow silently no-ops
//! (verified on a real TradingView run: three no-change steps, user stopped
//! it). `auto` keeps X11 semantics by default and switches to CDP per
//! observation, gated on the EWMH active-window title (`… - Google Chrome`).
//!
//! Consistency rule: `capture()` selects fresh and remembers the choice;
//! `act()` reuses the remembered backend so coordinates stay in one space
//! for the step. On a transport error each side falls back to the other
//! once (focus moves mid-step, Chrome crashed between calls).

use async_trait::async_trait;
use rose_core::{Action, Actor, DesktopBackend, DesktopInfo, DomOverlay, Frame, Result};
use rose_desktop_cdp::CdpDesktop;
use rose_desktop_x11::X11Desktop;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    X11,
    Cdp,
}

/// Pure routing rule, unit-tested: Chrome window titles end with
/// `" - Google Chrome"` (verified live: `New Tab - Google Chrome`,
/// WM_CLASS `Google-chrome`). Everything else — terminal, file dialogs,
/// the first-run ToS window, unknown Chrome UI — stays on X11 pixels.
pub fn use_cdp_for_title(title: Option<&str>) -> bool {
    let t = title.unwrap_or("").to_lowercase();
    t.ends_with(" - google chrome") || t.ends_with(" - chromium")
}

pub struct AutoDesktop {
    x11: X11Desktop,
    cdp: CdpDesktop,
    /// Backend that served the last observation; `act()` reuses it so a
    /// step's coordinates never straddle two spaces. Defaults to X11.
    last: Mutex<Route>,
}

impl std::fmt::Debug for AutoDesktop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutoDesktop")
            .field("display", &self.x11.display())
            .field("cdp", &self.cdp.endpoint())
            .finish_non_exhaustive()
    }
}

impl AutoDesktop {
    /// Sync constructor for the composition root: no I/O. Both inner
    /// backends are lazy too, so `rosed` starts with X down and Chrome down.
    pub fn new(
        display: &str,
        cdp_endpoint: &str,
        launch_if_missing: bool,
        chrome_binary: &str,
    ) -> Self {
        Self {
            x11: X11Desktop::new(display.to_string()),
            cdp: CdpDesktop::new(cdp_endpoint, launch_if_missing, chrome_binary),
            last: Mutex::new(Route::X11),
        }
    }

    fn remember(&self, route: Route) {
        *self.last.lock().expect("auto route") = route;
    }

    fn remembered(&self) -> Route {
        *self.last.lock().expect("auto route")
    }

    async fn capture_via(&self, route: Route) -> Result<Frame> {
        match route {
            Route::X11 => self.x11.capture().await,
            Route::Cdp => self.cdp.capture().await,
        }
    }

    async fn act_via(&self, route: Route, action: &Action, actor: Actor) -> Result<()> {
        match route {
            Route::X11 => self.x11.act(action, actor).await,
            Route::Cdp => self.cdp.act(action, actor).await,
        }
    }

    /// Fresh route from the live active-window title.
    async fn select(&self) -> Route {
        match self.x11.info().await {
            Ok(info) => {
                let route = if use_cdp_for_title(info.active_window.as_deref()) {
                    Route::Cdp
                } else {
                    Route::X11
                };
                tracing::info!(
                    active_window = ?info.active_window,
                    route = ?route,
                    "auto select"
                );
                route
            }
            Err(e) => {
                // X organ unreachable (VNC restarting?): CDP may still serve.
                tracing::warn!(error = %e, "auto: x11 info failed, trying cdp");
                Route::Cdp
            }
        }
    }
}

#[async_trait]
impl DesktopBackend for AutoDesktop {
    async fn info(&self) -> Result<DesktopInfo> {
        // X11 owns the truth about screen + focus; also refreshes the
        // sticky route as a side effect for the next act().
        let info = self.x11.info().await?;
        self.remember(if use_cdp_for_title(info.active_window.as_deref()) {
            Route::Cdp
        } else {
            Route::X11
        });
        Ok(info)
    }

    async fn capture(&self) -> Result<Frame> {
        let route = self.select().await;
        self.remember(route);
        match self.capture_via(route).await {
            Ok(frame) => Ok(frame),
            Err(e) => {
                // Focus moved (or Chrome died) between select and capture:
                // one cross-fallback, then the error is real.
                let other = match route {
                    Route::X11 => Route::Cdp,
                    Route::Cdp => Route::X11,
                };
                tracing::warn!(error = %e, tried = ?route, "auto: capture failed, falling back");
                let frame = self.capture_via(other).await?;
                self.remember(other);
                Ok(frame)
            }
        }
    }

    async fn act(&self, action: &Action, actor: Actor) -> Result<()> {
        let route = self.remembered();
        match self.act_via(route, action, actor).await {
            Ok(()) => Ok(()),
            Err(e) => {
                // Stale route (focus moved between capture and act):
                // reselect once; only retry when the route actually changed.
                let fresh = self.select().await;
                if fresh == route {
                    return Err(e);
                }
                self.remember(fresh);
                self.act_via(fresh, action, actor).await
            }
        }
    }

    async fn dom_overlay(&self, frame_jpeg: &[u8]) -> Result<Option<DomOverlay>> {
        match self.x11.info().await {
            Ok(info) if use_cdp_for_title(info.active_window.as_deref()) => {
                self.cdp.dom_overlay(frame_jpeg).await
            }
            Ok(_) => Ok(None), // native window: no DOM; sidecar covers pixels
            Err(_) => self.cdp.dom_overlay(frame_jpeg).await, // X down: best effort
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_titles_route_to_cdp() {
        assert!(use_cdp_for_title(Some("New Tab - Google Chrome")));
        assert!(use_cdp_for_title(Some("GOOGL Stock Price - Google Chrome")));
        assert!(use_cdp_for_title(Some("NEW TAB - GOOGLE CHROME")));
        assert!(use_cdp_for_title(Some("Settings - Chromium")));
    }

    #[test]
    fn everything_else_stays_on_x11() {
        assert!(!use_cdp_for_title(None));
        assert!(!use_cdp_for_title(Some("")));
        assert!(!use_cdp_for_title(Some("Terminal")));
        assert!(!use_cdp_for_title(Some("Files")));
        assert!(!use_cdp_for_title(Some("FakeDesktop")));
        // First-run ToS dialog: Chrome chrome, but unknown UI — pixels win.
        assert!(!use_cdp_for_title(Some(
            "Google Chrome and ChromeOS Additional Terms of Service"
        )));
        // Bare brand without the " - " window-title separator.
        assert!(!use_cdp_for_title(Some("Google Chrome")));
    }

    #[test]
    fn constructor_is_lazy_and_defaults_to_x11() {
        let d = AutoDesktop::new(":1", "http://127.0.0.1:9", false, "");
        assert_eq!(d.remembered(), Route::X11);
        let dbg = format!("{d:?}");
        assert!(dbg.contains("AutoDesktop"));
    }
}
