//! Scripted test double: returns canned [`super::ModelReply`]s in order.
//! No network, no key. Used by agent-loop tests (M2) and offline dev.

use super::ModelReply;
use rose_core::{Action, MouseButton, Result, StepSummary, Target};
use std::sync::Mutex;

pub struct FakeBrain {
    script: Mutex<Vec<ModelReply>>,
    calls: Mutex<Vec<(String, Vec<StepSummary>)>>,
}

impl FakeBrain {
    /// A scripted click-type-done run: click (10,10), type, done.
    pub fn demo() -> Self {
        Self::scripted(vec![
            ModelReply::Act {
                action: Action::Click {
                    target: Target::Point { x: 10, y: 10 },
                    button: MouseButton::Left,
                },
                say: "Clicking the first cell".to_string(),
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
        }
    }

    pub async fn next_action(
        &self,
        goal: &str,
        history: &[StepSummary],
        _jpeg: &[u8],
    ) -> Result<ModelReply> {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replays_script_in_order() {
        let brain = FakeBrain::demo();
        let r1 = brain.next_action("g", &[], &[]).await.expect("r1");
        assert!(matches!(r1, ModelReply::Act { .. }));
        let r2 = brain.next_action("g", &[], &[]).await.expect("r2");
        assert!(matches!(r2, ModelReply::Done { .. }));
        assert_eq!(brain.call_count(), 2);
    }
}
