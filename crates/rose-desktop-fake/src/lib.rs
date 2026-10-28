//! Deterministic in-memory desktop backend (IMPLEMENTATION.md T1.1).
//!
//! Serves a generated test-pattern frame (grid + coloured rectangles) and
//! records every `act()` into a log. Clicking a rectangle highlights it, so
//! verify logic has something observable. No display, no network.

use async_trait::async_trait;
use image::codecs::jpeg::JpegEncoder;
use image::{Rgb, RgbImage};
use rose_core::{Action, Actor, DesktopBackend, DesktopInfo, Frame, FrameId, Result, Target};
use std::sync::Mutex;

pub const FAKE_WIDTH: u32 = 1600;
pub const FAKE_HEIGHT: u32 = 900;
const COLS: usize = 4;
const ROWS: usize = 2;
const NCELLS: usize = COLS * ROWS;

const BG: Rgb<u8> = Rgb([32, 36, 43]);
const GRID: Rgb<u8> = Rgb([52, 57, 68]);
const CELL_COLORS: [Rgb<u8>; NCELLS] = [
    Rgb([176, 58, 90]),
    Rgb([15, 123, 77]),
    Rgb([154, 98, 6]),
    Rgb([58, 110, 176]),
    Rgb([110, 58, 176]),
    Rgb([58, 150, 150]),
    Rgb([170, 170, 170]),
    Rgb([120, 80, 40]),
];

fn cell_bounds(i: usize) -> (u32, u32, u32, u32) {
    let cw = FAKE_WIDTH / COLS as u32;
    let ch = FAKE_HEIGHT / ROWS as u32;
    let (c, r) = ((i % COLS) as u32, (i / COLS) as u32);
    (c * cw + 40, r * ch + 40, cw - 80, ch - 80)
}

struct State {
    log: Vec<(Action, Actor)>,
    highlighted: Option<usize>,
    captures: u64,
}

/// Deterministic fake backend. Clone shares the same underlying state.
pub struct FakeDesktop {
    state: std::sync::Arc<Mutex<State>>,
}

impl FakeDesktop {
    pub fn new() -> Self {
        Self {
            state: std::sync::Arc::new(Mutex::new(State {
                log: Vec::new(),
                highlighted: None,
                captures: 0,
            })),
        }
    }

    /// Every action received, in order (the audit log the loop replays).
    pub fn action_log(&self) -> Vec<(Action, Actor)> {
        self.state.lock().expect("fake state").log.clone()
    }

    pub fn highlighted(&self) -> Option<usize> {
        self.state.lock().expect("fake state").highlighted
    }

    fn render(highlighted: Option<usize>) -> Vec<u8> {
        let mut img = RgbImage::from_pixel(FAKE_WIDTH, FAKE_HEIGHT, BG);
        // Grid every 100 px.
        for x in (0..FAKE_WIDTH).step_by(100) {
            for y in 0..FAKE_HEIGHT {
                img.put_pixel(x, y, GRID);
            }
        }
        for y in (0..FAKE_HEIGHT).step_by(100) {
            for x in 0..FAKE_WIDTH {
                img.put_pixel(x, y, GRID);
            }
        }
        // Rectangles; the clicked one gets a white border.
        for (i, color) in CELL_COLORS.iter().enumerate() {
            let (x0, y0, w, h) = cell_bounds(i);
            for y in y0..y0 + h {
                for x in x0..x0 + w {
                    img.put_pixel(x, y, *color);
                }
            }
            if highlighted == Some(i) {
                let white = Rgb([255, 255, 255]);
                for t in 0..5 {
                    for x in x0..x0 + w {
                        img.put_pixel(x, y0 + t, white);
                        img.put_pixel(x, y0 + h - 1 - t, white);
                    }
                    for y in y0..y0 + h {
                        img.put_pixel(x0 + t, y, white);
                        img.put_pixel(x0 + w - 1 - t, y, white);
                    }
                }
            }
        }
        let mut buf = Vec::new();
        JpegEncoder::new_with_quality(&mut buf, 85)
            .encode_image(&img)
            .expect("jpeg encode");
        buf
    }
}

impl Default for FakeDesktop {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for FakeDesktop {
    fn clone(&self) -> Self {
        Self {
            state: std::sync::Arc::clone(&self.state),
        }
    }
}

#[async_trait]
impl DesktopBackend for FakeDesktop {
    async fn info(&self) -> Result<DesktopInfo> {
        Ok(DesktopInfo {
            display: ":fake".to_string(),
            width: FAKE_WIDTH,
            height: FAKE_HEIGHT,
            active_window: Some("FakeDesktop".to_string()),
        })
    }

    async fn capture(&self) -> Result<Frame> {
        let mut s = self.state.lock().expect("fake state");
        s.captures += 1;
        Ok(Frame {
            id: FrameId::new(),
            captured_at: chrono::Utc::now(),
            width: FAKE_WIDTH,
            height: FAKE_HEIGHT,
            jpeg: Self::render(s.highlighted),
            active_window: Some("FakeDesktop".to_string()),
        })
    }

    async fn act(&self, action: &Action, actor: Actor) -> Result<()> {
        let mut s = self.state.lock().expect("fake state");
        s.log.push((action.clone(), actor));
        // Clicking a rectangle highlights it (observable verify signal).
        let point = match action {
            Action::Click { target, .. } | Action::DoubleClick { target } => match target {
                Target::Point { x, y } => Some((*x, *y)),
                Target::Element(id) => {
                    id.0.strip_prefix("fake-cell-")
                        .and_then(|n| n.parse::<usize>().ok())
                        .map(|i| {
                            let (x, y, w, h) = cell_bounds(i % NCELLS);
                            (x + w / 2, y + h / 2)
                        })
                }
            },
            _ => None,
        };
        if let Some((x, y)) = point {
            for i in 0..NCELLS {
                let (x0, y0, w, h) = cell_bounds(i);
                if x >= x0 && x < x0 + w && y >= y0 && y < y0 + h {
                    s.highlighted = Some(i);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rose_core::MouseButton;

    fn is_jpeg(bytes: &[u8]) -> bool {
        bytes.len() > 4
            && bytes[0] == 0xff
            && bytes[1] == 0xd8
            && bytes[bytes.len() - 2] == 0xff
            && bytes[bytes.len() - 1] == 0xd9
    }

    #[tokio::test]
    async fn capture_is_1600x900_jpeg() {
        let d = FakeDesktop::new();
        let info = d.info().await.expect("info");
        assert_eq!((info.width, info.height), (1600, 900));
        let f = d.capture().await.expect("capture");
        assert_eq!((f.width, f.height), (1600, 900));
        assert!(is_jpeg(&f.jpeg));
    }

    #[tokio::test]
    async fn clicking_a_cell_changes_the_frame() {
        let d = FakeDesktop::new();
        let before = d.capture().await.expect("capture").jpeg;
        let (x, y, w, h) = cell_bounds(3);
        d.act(
            &Action::Click {
                target: Target::Point {
                    x: x + w / 2,
                    y: y + h / 2,
                },
                button: MouseButton::Left,
            },
            Actor::Agent,
        )
        .await
        .expect("act");
        assert_eq!(d.highlighted(), Some(3));
        let after = d.capture().await.expect("capture").jpeg;
        assert_ne!(before, after, "highlight must change the pixels");
    }

    #[tokio::test]
    async fn every_act_is_logged_in_order_with_actor() {
        let d = FakeDesktop::new();
        let click = Action::Click {
            target: Target::Point { x: 10, y: 10 },
            button: MouseButton::Right,
        };
        d.act(&click, Actor::Agent).await.expect("act");
        d.act(&Action::Wait { ms: 50 }, Actor::Human)
            .await
            .expect("act");
        let log = d.action_log();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0], (click, Actor::Agent));
        assert_eq!(log[1].1, Actor::Human);
    }
}
