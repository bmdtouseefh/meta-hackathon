//! Muse Spark client: the one model call in Phase 1.
//!
//! Grounded in Meta Model API docs (dev.meta.ai): OpenAI-compatible
//! `POST {endpoint}/chat/completions`, `Authorization: Bearer <key>`,
//! `developer` message steers, short history resent per call (stateless),
//! retry 429/5xx with backoff, never retry 400.

use base64::Engine;
use rose_core::{Action, ElementId, MouseButton, Result, RoseError, StepSummary, Target};
use serde::Deserialize;

/// The §6 reply: one JSON object, nothing else.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelReply {
    Act {
        action: Action,
        say: String,
        /// After a `Type`, also press Return in the same decision.
        /// Typing into a field and submitting it is one human gesture;
        /// as separate replies it burned a whole screenshot+brain
        /// round-trip (and a step) just to press Enter. The loop still
        /// dispatches two atomic `act()` calls, verified together on
        /// the next screenshot.
        enter_after: bool,
    },
    /// Run a shell command natively (`text` is the command). For
    /// anything a command line can do directly — launching apps,
    /// files, processes — instead of driving a terminal by pixels.
    Shell {
        command: String,
        say: String,
    },
    /// Text-mode reply: "I need to see the desktop before deciding."
    /// Only meaningful from a text (no-screenshot) call; the loop then
    /// takes a screenshot and asks again with vision.
    Look {
        say: String,
    },
    Done {
        say: String,
    },
    Ask {
        say: String,
    },
    Fail {
        say: String,
    },
}

#[derive(Debug, Deserialize)]
struct RawReply {
    status: String,
    action: Option<String>,
    x: Option<u32>,
    y: Option<u32>,
    element: Option<serde_json::Value>,
    text: Option<String>,
    dy: Option<i32>,
    ms: Option<u32>,
    enter: Option<bool>,
    say: Option<String>,
}

/// Split a chord the prompt advertises ("ctrl+l", "ctrl+shift+t") into the
/// separate key tokens the X11 backend expects (["ctrl", "l"], ...).
/// A single key name passes through untouched.
fn split_chord(text: &str) -> Vec<String> {
    text.split('+')
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .collect()
}

/// Parse one model reply. Pure: unit-tested with fixtures, no network.
///
/// The reply must be one JSON object, but the model sometimes narrates
/// around it ("Found the video — I'll pull its directions…{...}"), which
/// used to fail the whole run after retry. Direct parse first; on failure
/// recover the object between the first `{` and the last `}` and parse
/// that. Anything else is still an error, never a guess.
pub fn parse_reply(text: &str) -> Result<ModelReply> {
    let text = strip_fences(text);
    match serde_json::from_str::<RawReply>(&text) {
        Ok(raw) => to_reply(raw),
        Err(first_err) => {
            if let Some(start) = text.find('{') {
                if let Some(end) = text.rfind('}') {
                    if end > start {
                        if let Ok(raw) = serde_json::from_str::<RawReply>(&text[start..=end]) {
                            return to_reply(raw);
                        }
                    }
                }
            }
            Err(RoseError::Model(format!(
                "reply is not valid JSON ({first_err}): {}",
                truncate(&text, 200)
            )))
        }
    }
}

fn to_reply(raw: RawReply) -> Result<ModelReply> {
    let say = raw.say.unwrap_or_default();
    match raw.status.as_str() {
        "done" => Ok(ModelReply::Done { say }),
        "ask" => Ok(ModelReply::Ask { say }),
        "fail" => Ok(ModelReply::Fail { say }),
        "act" => {
            let action = raw.action.as_deref().unwrap_or("");
            match action {
                "click" | "double_click" => {
                    // SoM path (preferred when grounding overlay is on):
                    // {"action":"click","element":7}. Legacy pixel path
                    // ({"x":812,"y":240}) still parses for fallback runs.
                    let target = match &raw.element {
                        Some(v) => {
                            let s = if let Some(n) = v.as_u64() {
                                n.to_string()
                            } else if let Some(s) = v.as_str() {
                                s.to_string()
                            } else {
                                return Err(RoseError::Model(
                                    "click element must be a number or string".to_string(),
                                ));
                            };
                            Target::Element(ElementId(s))
                        }
                        None => match (raw.x, raw.y) {
                            (Some(x), Some(y)) => Target::Point { x, y },
                            _ => {
                                return Err(RoseError::Model(
                                    "click/double_click needs element or x and y".to_string(),
                                ))
                            }
                        },
                    };
                    let action = if action == "click" {
                        Action::Click {
                            target,
                            button: MouseButton::Left,
                        }
                    } else {
                        // Desktop icons only *select* on a single click; they
                        // launch on a double-click. Without this variant the
                        // model could never open one, however well it aimed.
                        Action::DoubleClick { target }
                    };
                    Ok(ModelReply::Act {
                        action,
                        say,
                        enter_after: false,
                    })
                }
                "wait" => Ok(ModelReply::Act {
                    action: Action::Wait {
                        ms: raw.ms.unwrap_or(1_500).clamp(100, 10_000),
                    },
                    say,
                    enter_after: false,
                }),
                "type" => match raw.text {
                    Some(text) => Ok(ModelReply::Act {
                        action: Action::Type { text },
                        say,
                        enter_after: raw.enter.unwrap_or(false),
                    }),
                    None => Err(RoseError::Model("type needs text".to_string())),
                },
                "key" => match raw.text {
                    // The prompt tells the model to emit chords as one
                    // string ("ctrl+l"); the backend wants separate tokens.
                    // Wrapping the whole string as one key made every chord
                    // fail with "unknown key" after the model had replied.
                    Some(text) => Ok(ModelReply::Act {
                        action: Action::PressKey {
                            keys: split_chord(&text),
                        },
                        say,
                        enter_after: false,
                    }),
                    None => Err(RoseError::Model("key needs text".to_string())),
                },
                "look" => Ok(ModelReply::Look { say }),
                "shell" => match raw.text {
                    Some(command) if !command.trim().is_empty() => {
                        Ok(ModelReply::Shell { command, say })
                    }
                    _ => Err(RoseError::Model(
                        "shell needs text (the command)".to_string(),
                    )),
                },
                "scroll" => Ok(ModelReply::Act {
                    action: Action::Scroll {
                        // The §6 format carries no scroll target; centre is
                        // the sane default and T1.4+ grounds it properly.
                        target: Target::Point { x: 800, y: 450 },
                        dy: raw.dy.unwrap_or(-600),
                    },
                    say,
                    enter_after: false,
                }),
                other => Err(RoseError::Model(format!("unknown action {other:?}"))),
            }
        }
        other => Err(RoseError::Model(format!("unknown status {other:?}"))),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

const BRAIN_PROMPT: &str = include_str!("../../../prompts/brain.md");

pub struct MuseClient {
    http: reqwest::Client,
    endpoint: String,
    model: String,
    api_key: String,
}

impl MuseClient {
    pub fn new(endpoint: &str, model: &str, api_key: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| RoseError::Model(format!("http client: {e}")))?;
        Ok(Self {
            http,
            endpoint: endpoint.trim_end_matches('/').to_string(),
            model: model.to_string(),
            api_key: api_key.to_string(),
        })
    }

    fn history_text(history: &[StepSummary]) -> String {
        if history.is_empty() {
            return "(no previous actions)".to_string();
        }
        history
            .iter()
            .rev()
            .take(10)
            .rev()
            .map(|s| format!("- step {}: {} ({})", s.idx, s.intent, s.say))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Text-only step instructions: no screenshot is attached, so the
    /// model must not invent pixel actions. Shell-first; `look` is the
    /// only way to get a screenshot.
    const TEXT_MODE: &'static str = "\n\n# This step: TEXT ONLY (no screenshot attached)\n\
        You cannot see the desktop this step. Do not emit click/double_click/type/key/scroll — you have nothing to aim at.\n\
        Decide from the goal and history (including shell outputs) alone:\n\
        - If the goal (or its next step) can be done from the shell, reply \"shell\". \
        Opening/launching an app or a URL is ALWAYS a shell step (e.g. setsid google-chrome 'URL' &): never ask to look just to find its icon.\n\
        - If shell output in the history already proves the goal is complete, reply \"done\".\n\
        - Only if the next step genuinely requires seeing and operating the GUI (clicking a control in an already-open app), reply {\"status\":\"act\",\"action\":\"look\",\"say\":\"I need to see the desktop to ...\"} and a screenshot will be taken for your next reply.\n\
        - If the goal references a recorded video (e.g. \"mail-video\"), its Spark-drafted steps already exist: list ~/.local/share/rose/videos/ to find the id, `cat` that id's steps.json, and follow those steps instead of searching the disk for the video. Never re-derive what steps.json already says.\n";

    /// Goal + short history + current JPEG + grounded element list ->
    /// one JSON action. The list is the text twin of the annotated image
    /// (same 1..N order) so the brain picks by meaning, not by squinting.
    pub async fn next_action(
        &self,
        goal: &str,
        history: &[StepSummary],
        jpeg: &[u8],
        elements: &[rose_core::Element],
    ) -> Result<ModelReply> {
        self.call(goal, history, Some(jpeg), elements).await
    }

    /// Goal + short history, NO screenshot -> one JSON action.
    /// Shell steps run on this path, so a command-line task never
    /// pays for a screenshot capture or a vision call per step.
    pub async fn next_action_text(
        &self,
        goal: &str,
        history: &[StepSummary],
    ) -> Result<ModelReply> {
        self.call(goal, history, None, &[]).await
    }

    /// Bad reply: one retry, then a clear error (never panic on model output).
    async fn call(
        &self,
        goal: &str,
        history: &[StepSummary],
        jpeg: Option<&[u8]>,
        elements: &[rose_core::Element],
    ) -> Result<ModelReply> {
        let (user_text, developer, image_part) = match jpeg {
            Some(jpeg) => {
                let image_b64 = base64::engine::general_purpose::STANDARD.encode(jpeg);
                let element_list = crate::grounding::format_elements(elements);
                let elements_text = if element_list.is_empty() {
                    String::new()
                } else {
                    format!("\n{element_list}")
                };
                (
                    format!(
                        "Goal: {goal}\nPrevious actions (most recent last):\n{}\nCurrent screenshot attached (stay inside its pixels; X11 is 1600x900, browser viewports are shorter).{elements_text}\nReply with ONE JSON object only.",
                        Self::history_text(history)
                    ),
                    BRAIN_PROMPT.to_string(),
                    Some(serde_json::json!({ "type": "image_url", "image_url": {
                        "url": format!("data:image/jpeg;base64,{image_b64}")
                    }})),
                )
            }
            None => (
                format!(
                    "Goal: {goal}\nPrevious actions and shell outputs (most recent last):\n{}\nNo screenshot is attached this step. Reply with ONE JSON object only.",
                    Self::history_text(history)
                ),
                format!("{BRAIN_PROMPT}{}", Self::TEXT_MODE),
                None,
            ),
        };
        let mut content = vec![serde_json::json!({ "type": "text", "text": user_text })];
        if let Some(part) = image_part {
            content.push(part);
        }
        let body = serde_json::json!({
            "model": self.model,
            "response_format": { "type": "json_object" },
            "messages": [
                { "role": "developer", "content": developer },
                { "role": "user", "content": content },
            ],
        });

        let url = format!("{}/chat/completions", self.endpoint);
        let mut last_err = String::new();
        // One attempt + one retry (transient 429/5xx or one bad-JSON reply).
        for attempt in 1..=2 {
            let resp = self
                .http
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
                .map_err(|e| RoseError::Model(format!("request failed: {e}")))?;
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                last_err = format!("transient HTTP {status}");
                tracing::warn!(attempt, %status, "muse transient error, retrying");
                tokio::time::sleep(std::time::Duration::from_secs(attempt * 2)).await;
                continue;
            }
            if !status.is_success() {
                // 400 and friends: the request is wrong, never retry.
                return Err(RoseError::Model(format!(
                    "HTTP {status}: {}",
                    truncate(&text, 300)
                )));
            }
            #[derive(Deserialize)]
            struct Choice {
                message: ChoiceMessage,
            }
            #[derive(Deserialize)]
            struct ChoiceMessage {
                content: Option<String>,
            }
            #[derive(Deserialize)]
            struct ChatResponse {
                choices: Vec<Choice>,
            }
            let parsed: ChatResponse = serde_json::from_str(&text).map_err(|e| {
                RoseError::Model(format!("bad chat envelope ({e}): {}", truncate(&text, 200)))
            })?;
            let content = parsed
                .choices
                .first()
                .and_then(|c| c.message.content.clone())
                .unwrap_or_default();
            let content = strip_fences(&content);
            match parse_reply(&content) {
                Ok(reply) => return Ok(reply),
                Err(e) => {
                    last_err = e.to_string();
                    tracing::warn!(attempt, error = %last_err, "bad model JSON, retrying once");
                }
            }
        }
        Err(RoseError::Model(format!("gave up after retry: {last_err}")))
    }
}

fn strip_fences(s: &str) -> String {
    let t = s.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t);
    t.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_statuses() {
        let click = parse_reply(
            r#"{"status":"act","action":"click","x":812,"y":240,"text":null,"say":"Clicking Export"}"#,
        )
        .expect("click");
        assert!(matches!(click, ModelReply::Act { .. }));
        if let ModelReply::Act { action, say, .. } = click {
            assert_eq!(
                action,
                Action::Click {
                    target: Target::Point { x: 812, y: 240 },
                    button: MouseButton::Left,
                }
            );
            assert_eq!(say, "Clicking Export");
        }
        assert!(matches!(
            parse_reply(r#"{"status":"done","say":"The CSV is downloaded"}"#).unwrap(),
            ModelReply::Done { .. }
        ));
        assert!(matches!(
            parse_reply(r#"{"status":"ask","say":"Which month?"}"#).unwrap(),
            ModelReply::Ask { .. }
        ));
        assert!(matches!(
            parse_reply(r#"{"status":"fail","say":"Nope"}"#).unwrap(),
            ModelReply::Fail { .. }
        ));
    }

    #[test]
    fn parses_element_clicks() {
        // SoM path: the brain picks an overlay number, Rust maps it later.
        let e =
            parse_reply(r#"{"status":"act","action":"click","element":7,"say":"Clicking Export"}"#)
                .expect("element click");
        if let ModelReply::Act { action, .. } = e {
            assert_eq!(
                action,
                Action::Click {
                    target: Target::Element(ElementId("7".to_string())),
                    button: MouseButton::Left,
                }
            );
        } else {
            panic!("want act");
        }
        let s = parse_reply(
            r#"{"status":"act","action":"double_click","element":"e-3","say":"Opening"}"#,
        )
        .expect("element double_click");
        assert!(matches!(
            s,
            ModelReply::Act {
                action: Action::DoubleClick { .. },
                ..
            }
        ));
        // Grid-cell picks arrive as element strings too ("g<col>,<row>");
        // the Stepper resolves them against the frame's grid geometry.
        let g = parse_reply(
            r#"{"status":"act","action":"click","element":"g14,3","say":"Clicking cell 14,3"}"#,
        )
        .expect("grid cell click");
        assert!(matches!(
            g,
            ModelReply::Act {
                action: Action::Click {
                    target: Target::Element(ElementId(ref s)),
                    ..
                },
                ..
            } if s == "g14,3"
        ));
        // Legacy pixels still parse for fallback runs.
        assert!(parse_reply(r#"{"status":"act","action":"click","x":1,"y":2,"say":"p"}"#).is_ok());
        assert!(parse_reply(r#"{"status":"act","action":"click","say":"no target"}"#).is_err());
    }

    #[test]
    fn parses_type_key_scroll_and_fences() {
        // Fences are stripped by next_action before parsing; unit-test both.
        let fenced =
            "```json\n{\"status\":\"act\",\"action\":\"type\",\"text\":\"hi\",\"say\":\"t\"}\n```";
        let t = parse_reply(&strip_fences(fenced)).expect("fenced");
        assert!(matches!(
            t,
            ModelReply::Act {
                action: Action::Type { .. },
                ..
            }
        ));
        let k = parse_reply(r#"{"status":"act","action":"key","text":"Return","say":"k"}"#)
            .expect("key");
        assert!(matches!(
            k,
            ModelReply::Act {
                action: Action::PressKey { .. },
                ..
            }
        ));
        let s = parse_reply(r#"{"status":"act","action":"scroll","dy":-600,"say":"s"}"#)
            .expect("scroll");
        if let ModelReply::Act {
            action: Action::Scroll { dy, .. },
            ..
        } = s
        {
            assert_eq!(dy, -600);
        } else {
            panic!("want scroll");
        }
    }

    #[test]
    fn parses_double_click_wait_and_key_chords() {
        let d = parse_reply(
            r#"{"status":"act","action":"double_click","x":120,"y":200,"say":"Opening Chrome"}"#,
        )
        .expect("double_click");
        assert!(matches!(
            d,
            ModelReply::Act {
                action: Action::DoubleClick {
                    target: Target::Point { x: 120, y: 200 }
                },
                ..
            }
        ));
        let w = parse_reply(r#"{"status":"act","action":"wait","ms":3000,"say":"Waiting"}"#)
            .expect("wait");
        assert!(matches!(
            w,
            ModelReply::Act {
                action: Action::Wait { ms: 3000 },
                ..
            }
        ));
        // Chords arrive as one "ctrl+l" string but act as two key tokens.
        let k = parse_reply(r#"{"status":"act","action":"key","text":"ctrl+l","say":"k"}"#)
            .expect("chord");
        assert!(matches!(
            k,
            ModelReply::Act {
                action: Action::PressKey { keys },
                ..
            } if keys == vec!["ctrl".to_string(), "l".to_string()]
        ));
        assert_eq!(split_chord("Return"), vec!["Return".to_string()]);
        assert_eq!(
            split_chord(" ctrl + shift + t "),
            vec!["ctrl".to_string(), "shift".to_string(), "t".to_string()]
        );
    }

    #[test]
    fn parses_look_requests() {
        let l = parse_reply(r#"{"status":"act","action":"look","say":"Need to see it"}"#)
            .expect("look");
        assert!(matches!(l, ModelReply::Look { .. }));
    }

    #[test]
    fn parses_shell_commands() {
        let s = parse_reply(
            r#"{"status":"act","action":"shell","text":"ls ~/Downloads","say":"Listing"}"#,
        )
        .expect("shell");
        assert!(matches!(
            s,
            ModelReply::Shell { command, .. } if command == "ls ~/Downloads"
        ));
        assert!(parse_reply(r#"{"status":"act","action":"shell","say":"no cmd"}"#).is_err());
    }

    #[test]
    fn type_can_carry_enter_after() {
        let t = parse_reply(
            r#"{"status":"act","action":"type","text":"example.com","enter":true,"say":"Going"}"#,
        )
        .expect("type+enter");
        assert!(matches!(
            t,
            ModelReply::Act {
                enter_after: true,
                ..
            }
        ));
        let plain = parse_reply(r#"{"status":"act","action":"type","text":"x","say":"t"}"#)
            .expect("plain type");
        assert!(matches!(
            plain,
            ModelReply::Act {
                enter_after: false,
                ..
            }
        ));
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(parse_reply("not json at all").is_err());
        assert!(parse_reply(r#"{"status":"dance"}"#).is_err());
        assert!(parse_reply(r#"{"status":"act","action":"click","say":"no coords"}"#).is_err());
        assert!(parse_reply("").is_err());
    }

    #[test]
    fn narration_around_json_is_recovered_not_fatal() {
        // The exact shape that killed a real run: prose, then the object.
        let r = parse_reply(
            "Found the instruction video — I'll pull its directions to send that first mail.\
             { \"status\": \"act\", \"action\": \"shell\", \"text\": \"cat steps.json\", \"say\": \"Reading\" }",
        )
        .expect("recovers the object");
        assert!(matches!(
            r,
            ModelReply::Shell { ref command, .. } if command == "cat steps.json"
        ));
        // Trailing chatter is tolerated too.
        let r = parse_reply(
            "{ \"status\": \"done\", \"say\": \"The CSV is downloaded\" } (anything else? just ask)",
        )
        .expect("trailing chatter tolerated");
        assert!(matches!(r, ModelReply::Done { .. }));
        // Braces with no JSON inside are still an error.
        assert!(parse_reply("uve { no json } here").is_err());
    }

    #[test]
    fn history_text_caps_at_ten() {
        let h: Vec<StepSummary> = (0..15)
            .map(|i| StepSummary {
                idx: i,
                intent: format!("i{i}"),
                say: format!("s{i}"),
            })
            .collect();
        let text = MuseClient::history_text(&h);
        assert!(text.contains("step 14"));
        assert!(!text.contains("step 0"), "oldest trimmed");
    }
}
