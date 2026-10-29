//! SoM grounding client: screenshot -> overlay elements (IMPLEMENTATION.md T3.3).
//!
//! Talks to `grounding-worker` (lite contour parser, OmniParser-V2
//! compatible contract) over HTTP. Pure mapping + small HTTP client;
//! the heavy CV lives in the sidecar so this crate stays within its
//! §2 deps (reqwest/serde_json/tokio/tracing/base64) and runs on
//! 4 CPU / 4 GB RAM (sidecar RSS < 500 MB).
//!
//! Same-idx sub-call: the agent loop calls `parse()` between `capture()`
//! and `next_action()` and sends the *annotated* JPEG to the brain.
//! Grounding never pushes history or burns a step.

use base64::Engine;
use rose_core::{Element, ElementId, ElementSource, Rect, Result, RoseError};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq)]
pub struct GroundedOverlay {
    /// Elements offered to the brain (ids are 1..N, stable for this frame).
    pub elements: Vec<Element>,
    /// Annotated JPEG (red boxes + numbered badges) to send to the brain
    /// instead of the raw screenshot.
    pub annotated_jpeg: Vec<u8>,
    /// Sidecar parse time in ms (for traces).
    pub parse_ms: u64,
}

#[derive(Debug, Deserialize)]
struct ParseElement {
    id: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    #[allow(dead_code)]
    cx: u32,
    #[allow(dead_code)]
    cy: u32,
    #[serde(default)]
    conf: f32,
    #[serde(default)]
    label: String,
}

#[derive(Debug, Deserialize)]
struct ParseResponse {
    #[allow(dead_code)]
    width: u32,
    #[allow(dead_code)]
    height: u32,
    #[serde(default)]
    ms: u64,
    #[serde(default)]
    elements: Vec<ParseElement>,
    #[serde(default)]
    annotated_b64: String,
}

/// Map one sidecar element into a core `Element`, dropping anything below
/// `min_conf` (0..1) or with a degenerate box.
fn to_element(el: &ParseElement, min_conf: f32) -> Option<Element> {
    if el.w == 0 || el.h == 0 {
        return None;
    }
    if el.conf < min_conf {
        return None;
    }
    Some(Element {
        id: ElementId(format!("e-{}", el.id)),
        label: if el.label.is_empty() {
            format!("control {}", el.id)
        } else {
            el.label.clone()
        },
        role: None,
        bounds: Rect {
            x: el.x,
            y: el.y,
            width: el.w,
            height: el.h,
        },
        confidence: el.conf,
        source: ElementSource::Vision,
    })
}

/// Pure: parse a `/parse` body. Unit-tested, no network.
pub fn parse_response_body(text: &str, min_conf: f32) -> Result<GroundedOverlay> {
    let resp: ParseResponse = serde_json::from_str(text).map_err(|e| {
        RoseError::Model(format!("bad grounding body ({e}): {}", truncate(text, 200)))
    })?;
    let annotated_jpeg = base64::engine::general_purpose::STANDARD
        .decode(&resp.annotated_b64)
        .map_err(|e| RoseError::Model(format!("bad annotated_b64: {e}")))?;
    let elements = resp
        .elements
        .iter()
        .filter_map(|el| to_element(el, min_conf))
        .collect();
    Ok(GroundedOverlay {
        elements,
        annotated_jpeg,
        parse_ms: resp.ms,
    })
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

pub struct GroundingClient {
    http: reqwest::Client,
    endpoint: String,
    min_conf: f32,
}

impl GroundingClient {
    pub fn new(endpoint: &str, min_conf_0_100: u32) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(8))
            .build()
            .map_err(|e| RoseError::Model(format!("grounding http client: {e}")))?;
        Ok(Self {
            http,
            endpoint: endpoint.trim_end_matches('/').to_string(),
            min_conf: (min_conf_0_100.min(100) as f32) / 100.0,
        })
    }

    /// Screenshot JPEG -> overlay. `None` on any failure: the caller falls
    /// back to the raw screenshot + old x,y path so a dead sidecar never
    /// blocks a run. Never retries (one extra RTT max per step).
    pub async fn parse(&self, jpeg: &[u8]) -> Option<GroundedOverlay> {
        let image_b64 = base64::engine::general_purpose::STANDARD.encode(jpeg);
        let url = format!("{}/parse", self.endpoint);
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({ "image_b64": image_b64 }))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            tracing::warn!(status = %resp.status(), "grounding /parse non-2xx, falling back to raw screenshot");
            return None;
        }
        let text = resp.text().await.ok()?;
        match parse_response_body(&text, self.min_conf) {
            Ok(overlay) => Some(overlay),
            Err(e) => {
                tracing::warn!(error = %e, "bad grounding body, falling back to raw screenshot");
                None
            }
        }
    }
}

/// Resolve the brain's `element: N` (1-based SoM id, or "e-N") to a click
/// point using the overlay from the same step. Returns `None` when the id
/// is unknown — the loop must replan, never blind-click.
pub fn resolve_element(elements: &[Element], raw: &str) -> Option<(u32, u32)> {
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    let n: usize = digits.parse().ok()?;
    if n == 0 || n > elements.len() {
        return None;
    }
    // Overlay numbers elements 1..N in order, so index N-1 is the target.
    // (Also matches ElementId "e-N" from to_element.)
    let el = elements.get(n - 1)?;
    Some((
        el.bounds.x + el.bounds.width / 2,
        el.bounds.y + el.bounds.height / 2,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn body_with(elements: &str) -> String {
        let fake_jpeg = base64::engine::general_purpose::STANDARD.encode([0xff, 0xd8]);
        format!(
            r#"{{"width":1600,"height":900,"ms":120,"elements":[{elements}],"annotated_b64":"{fake_jpeg}"}}"#
        )
    }

    #[test]
    fn parses_overlay_and_filters_low_conf() {
        let body = body_with(
            r#"{"id":1,"x":98,"y":198,"w":204,"h":54,"cx":200,"cy":225,"conf":0.9,"label":"control 1"},
                {"id":2,"x":10,"y":10,"w":5,"h":5,"cx":12,"cy":12,"conf":0.01,"label":"noise"}"#,
        );
        let overlay = parse_response_body(&body, 0.05).expect("parse");
        assert_eq!(overlay.elements.len(), 1);
        assert_eq!(overlay.elements[0].id.0, "e-1");
        assert_eq!(overlay.elements[0].bounds.x, 98);
        assert_eq!(overlay.parse_ms, 120);
    }

    #[test]
    fn resolves_element_ids_to_centres() {
        let body = body_with(
            r#"{"id":1,"x":100,"y":200,"w":200,"h":50,"cx":200,"cy":225,"conf":0.9,"label":"a"},
                {"id":2,"x":1200,"y":100,"w":200,"h":300,"cx":1300,"cy":250,"conf":0.8,"label":"b"}"#,
        );
        let overlay = parse_response_body(&body, 0.0).expect("parse");
        assert_eq!(resolve_element(&overlay.elements, "2"), Some((1300, 250)));
        assert_eq!(resolve_element(&overlay.elements, "e-1"), Some((200, 225)));
        assert_eq!(resolve_element(&overlay.elements, "9"), None);
        assert_eq!(resolve_element(&overlay.elements, "nope"), None);
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(parse_response_body("not json", 0.0).is_err());
        assert!(parse_response_body(r#"{"width":1}"#, 0.0).is_err());
    }
}
