//! Scripted test double: returns canned [`super::ModelReply`]s in order.
//! No network, no key. Used by agent-loop tests (M2) and offline dev.

use super::ModelReply;
use rose_core::{Action, MouseButton, Result, StepSummary, Target};
use std::sync::Mutex;

pub struct FakeBrain {
    script: Mutex<Vec<ModelReply>>,
    calls: Mutex<Vec<(String, Vec<StepSummary>)>>,
    /// Last vision call's inputs, for asserting what the brain was shown.
    last_vision: Mutex<Option<(usize, usize)>>,
}

impl FakeBrain {
    /// A scripted look-click-done run: the text brain asks to see
    /// the desktop, then clicks (10,10), then is done.
    pub fn demo() -> Self {
        Self::scripted(vec![
            ModelReply::Look {
                say: "I need to see the desktop".to_string(),
            },
            ModelReply::Act {
                action: Action::Click {
                    target: Target::Element(rose_core::ElementId("g1,1@0.42,0.42".to_string())),
                    button: MouseButton::Left,
                },
                say: "Clicking the first cell".to_string(),
                enter_after: false,
            },
            ModelReply::Done {
                say: "Demo complete".to_string(),
            },
        ])
    }

    pub fn scripted(replies: Vec<ModelReply>) -> Self {
        Self {
            script: Mutex::new(replies),
            calls: Mutex::new(Vec::new()),
            last_vision: Mutex::new(None),
        }
    }

    /// Text-mode decisions draw from the same script: a scripted
    /// shell run is how text steps are regression-tested end to end.
    pub async fn next_action_text(
        &self,
        goal: &str,
        history: &[StepSummary],
    ) -> Result<ModelReply> {
        self.next_action(goal, history, &[], &[]).await
    }

    pub async fn next_action(
        &self,
        goal: &str,
        history: &[StepSummary],
        jpeg: &[u8],
        elements: &[rose_core::Element],
    ) -> Result<ModelReply> {
        *self.last_vision.lock().expect("fake brain") = Some((jpeg.len(), elements.len()));
        self.calls
            .lock()
            .expect("fake brain")
            .push((goal.to_string(), history.to_vec()));
        let mut script = self.script.lock().expect("fake brain");
        if script.is_empty() {
            return Ok(ModelReply::Fail {
                say: "script exhausted".to_string(),
            });
        }
        Ok(script.remove(0))
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().expect("fake brain").len()
    }

    /// (jpeg bytes, element count) of the most recent vision call.
    pub fn last_vision(&self) -> Option<(usize, usize)> {
        *self.last_vision.lock().expect("fake brain")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replays_script_in_order() {
        let brain = FakeBrain::demo();
        let r1 = brain.next_action_text("g", &[]).await.expect("r1");
        assert!(matches!(r1, ModelReply::Look { .. }));
        let r2 = brain.next_action("g", &[], &[], &[]).await.expect("r2");
        assert!(matches!(r2, ModelReply::Act { .. }));
        let r3 = brain.next_action_text("g", &[]).await.expect("r3");
        assert!(matches!(r3, ModelReply::Done { .. }));
        assert_eq!(brain.call_count(), 3);
    }
}
