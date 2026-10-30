//! DOM grounding helpers: the text list of elements the brain picks
//! from, and resolution of its `element: N` picks to click points.
//!
//! (The omniparser sidecar client that used to live here is gone: vision
//! grounding is now the backend's DOM/AX list when it has one, and the
//! constant numbered grid — `rose-grid` — everywhere else.)

use rose_core::Element;

/// Text twin of the grounded elements: `[N] role: name @ (x,y WxH)` lines
/// so the brain picks by meaning *and* position (duplicate names happen —
/// three "Got it" buttons in three dialogs — and the box tells them apart).
/// Numbers are 1-based in list order, matching `resolve_element`. Empty
/// when there is nothing grounded.
pub fn format_elements(elements: &[Element]) -> String {
    if elements.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "Interactive elements visible now (pick by NUMBER, e.g. {\"action\":\"click\",\"element\":7}):",
    );
    for (i, el) in elements.iter().enumerate() {
        let b = el.bounds;
        out.push_str(&format!(
            "\n[{}] {} @ ({},{},{}x{})",
            i + 1,
            el.label,
            b.x,
            b.y,
            b.width,
            b.height
        ));
    }
    out
}

/// Resolve the brain's `element: N` (1-based list id, or "e-N") to a click
/// point using the element list from the same step. Returns `None` when
/// the id is unknown — the loop must replan, never blind-click.
pub fn resolve_element(elements: &[Element], raw: &str) -> Option<(u32, u32)> {
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    let n: usize = digits.parse().ok()?;
    if n == 0 || n > elements.len() {
        return None;
    }
    // The list numbers elements 1..N in order, so index N-1 is the target.
    // (Also matches ElementId "e-N" if a caller passes one through.)
    let el = elements.get(n - 1)?;
    Some((
        el.bounds.x + el.bounds.width / 2,
        el.bounds.y + el.bounds.height / 2,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rose_core::{ElementId, ElementSource, Rect};

    fn el(label: &str, x: u32, y: u32, w: u32, h: u32) -> Element {
        Element {
            id: ElementId(format!("x-{}", label)),
            label: label.to_string(),
            role: None,
            bounds: Rect {
                x,
                y,
                width: w,
                height: h,
            },
            confidence: 1.0,
            source: ElementSource::Cdp,
        }
    }

    #[test]
    fn resolves_element_ids_to_centres() {
        let els = vec![el("a", 100, 200, 200, 50), el("b", 1200, 100, 200, 300)];
        assert_eq!(resolve_element(&els, "2"), Some((1300, 250)));
        assert_eq!(resolve_element(&els, "e-1"), Some((200, 225)));
        assert_eq!(resolve_element(&els, "9"), None);
        assert_eq!(resolve_element(&els, "nope"), None);
    }

    #[test]
    fn element_list_numbers_by_position() {
        assert_eq!(format_elements(&[]), "");
        let list = format_elements(&[
            el("button: Close", 0, 0, 10, 10),
            el("button: Got it", 0, 0, 10, 10),
        ]);
        assert!(
            list.starts_with("Interactive elements visible now"),
            "{list}"
        );
        assert!(list.contains("\n[1] button: Close @ (0,0,10x10)"), "{list}");
        assert!(
            list.contains("\n[2] button: Got it @ (0,0,10x10)"),
            "{list}"
        );
    }
}
