//! Muse Spark client: the one model call in Phase 1.
//!
//! Grounded in Meta Model API docs (dev.meta.ai): OpenAI-compatible
//! `POST {endpoint}/chat/completions`, `Authorization: Bearer <key>`,
//! `developer` message steers, short history resent per call (stateless),
//! retry 429/5xx with backoff, never retry 400.

use base64::Engine;
use rose_core::{Action, MouseButton, Result, RoseError, StepSummary, Target};
use serde::Deserialize;

/// The §6 reply: one JSON object, nothing else.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelReply {
    Act { action: Action, say: String },
    Done { say: String },
    Ask { say: String },
    Fail { say: String },
}

#[derive(Debug, Deserialize)]
struct RawReply {
    status: String,
    action: Option<String>,
    x: Option<u32>,
    y: Option<u32>,
    text: Option<String>,
    dy: Option<i32>,
    say: Option<String>,
}

/// Parse one model reply. Pure: unit-tested with fixtures, no network.
pub fn parse_reply(text: &str) -> Result<ModelReply> {
    let raw: RawReply = serde_json::from_str(text.trim()).map_err(|e| {
        RoseError::Model(format!(
            "reply is not valid JSON ({e}): {}",
            truncate(text, 200)
        ))
    })?;
    let say = raw.say.unwrap_or_default();
    match raw.status.as_str() {
        "done" => Ok(ModelReply::Done { say }),
        "ask" => Ok(ModelReply::Ask { say }),
        "fail" => Ok(ModelReply::Fail { say }),
        "act" => {
            let action = raw.action.as_deref().unwrap_or("");
            match action {
                "click" => {
                    let (x, y) = match (raw.x, raw.y) {
                        (Some(x), Some(y)) => (x, y),
                        _ => return Err(RoseError::Model("click needs x and y".to_string())),
                    };
                    Ok(ModelReply::Act {
                        action: Action::Click {
                            target: Target::Point { x, y },
                            button: MouseButton::Left,
                        },
                        say,
                    })
                }
                "type" => match raw.text {
                    Some(text) => Ok(ModelReply::Act {
                        action: Action::Type { text },
                        say,
                    }),
                    None => Err(RoseError::Model("type needs text".to_string())),
                },
                "key" => match raw.text {
                    Some(text) => Ok(ModelReply::Act {
                        action: Action::PressKey { keys: vec![text] },
                        say,
                    }),
                    None => Err(RoseError::Model("key needs text".to_string())),
                },
                "scroll" => Ok(ModelReply::Act {
                    action: Action::Scroll {
                        // The §6 format carries no scroll target; centre is
                        // the sane default and T1.4+ grounds it properly.
                        target: Target::Point { x: 800, y: 450 },
                        dy: raw.dy.unwrap_or(-600),
                    },
                    say,
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

    /// Goal + short history + current JPEG -> one JSON action.
    /// Bad reply: one retry, then a clear error (never panic on model output).
    pub async fn next_action(
        &self,
        goal: &str,
        history: &[StepSummary],
        jpeg: &[u8],
    ) -> Result<ModelReply> {
        let image_b64 = base64::engine::general_purpose::STANDARD.encode(jpeg);
        let user_text = format!(
            "Goal: {goal}\nPrevious actions (most recent last):\n{}\nCurrent screenshot attached (1600x900). Reply with ONE JSON object only.",
            Self::history_text(history)
        );
        let body = serde_json::json!({
            "model": self.model,
            "response_format": { "type": "json_object" },
            "messages": [
                { "role": "developer", "content": BRAIN_PROMPT },
                { "role": "user", "content": [
                    { "type": "text", "text": user_text },
                    { "type": "image_url", "image_url": {
                        "url": format!("data:image/jpeg;base64,{image_b64}")
                    }},
                ]},
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
        if let ModelReply::Act { action, say } = click {
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
    fn garbage_is_an_error_not_a_panic() {
        assert!(parse_reply("not json at all").is_err());
        assert!(parse_reply(r#"{"status":"dance"}"#).is_err());
        assert!(parse_reply(r#"{"status":"act","action":"click","say":"no coords"}"#).is_err());
        assert!(parse_reply("").is_err());
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
