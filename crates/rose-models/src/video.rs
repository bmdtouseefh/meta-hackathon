//! Video → instructions: Muse Spark native video understanding.
//!
//! Spark reads a video's frames AND its embedded narration in one call,
//! so there is no SAM stage and no separate transcription stage.
//! Grounded in Meta Model API docs (dev.meta.ai video-understanding +
//! file-handling): upload to the Files API as multipart form data with
//! purpose user_data to get a file id, then reference it from a
//! Responses API call in an input_file block. Only video/mp4 is
//! understood — other containers must be normalized with ffmpeg before
//! calling this (ffmpeg ships on the VM per deploy/deb.sh).

use rose_core::{Result, RoseError};
use serde_json::Value;

const VIDEO_PROMPT: &str = include_str!("../../../prompts/video.md");

/// One draft instruction parsed from the model's reply.
#[derive(Debug, Clone, PartialEq)]
pub struct DraftStep {
    pub ts: u64,
    pub say: String,
    pub intent: String,
    pub params: String,
    pub approval: bool,
    pub conf: f32,
    pub quote: String,
}

/// Char-boundary-safe truncation (never splits a codepoint).
fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
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

fn num_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_f64().map(|f| f.max(0.0).round() as u64)),
        Value::String(s) => s
            .trim()
            .parse::<f64>()
            .ok()
            .map(|f| f.max(0.0).round() as u64),
        _ => None,
    }
}

fn num_f32(v: &Value) -> Option<f32> {
    match v {
        Value::Number(n) => n.as_f64().map(|f| (f as f32).clamp(0.0, 1.0)),
        Value::String(s) => s
            .trim()
            .parse::<f64>()
            .ok()
            .map(|f| (f as f32).clamp(0.0, 1.0)),
        _ => None,
    }
}

/// Parse the model's reply: `{"steps": [...]}` or a bare `[...]`.
/// Pure: unit-tested with fixtures, no network.
pub fn parse_steps(text: &str) -> Result<Vec<DraftStep>> {
    let text = strip_fences(text);
    let v: Value = serde_json::from_str(&text).map_err(|e| {
        RoseError::Model(format!(
            "video reply is not valid JSON ({e}): {}",
            truncate(&text, 200)
        ))
    })?;
    let arr = match &v {
        Value::Array(a) => a.clone(),
        Value::Object(_) => v
            .get("steps")
            .and_then(|s| s.as_array())
            .cloned()
            .ok_or_else(|| RoseError::Model("video reply has no \"steps\" array".to_string()))?,
        _ => {
            return Err(RoseError::Model(
                "video reply is not a JSON object or array".to_string(),
            ));
        }
    };
    let mut steps = Vec::new();
    for (i, item) in arr.iter().take(50).enumerate() {
        let say = item
            .get("say")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .trim();
        let say = if say.is_empty() {
            format!("Step {}", i + 1)
        } else {
            truncate(say, 200)
        };
        let intent = item
            .get("intent")
            .and_then(|s| s.as_str())
            .map(|s| truncate(s.trim(), 500))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| say.clone());
        steps.push(DraftStep {
            ts: item.get("ts").and_then(num_u64).unwrap_or(0).min(86_400),
            say,
            intent,
            params: item
                .get("params")
                .and_then(|s| s.as_str())
                .map(|s| truncate(s, 300))
                .unwrap_or_default(),
            approval: item
                .get("approval")
                .and_then(|b| b.as_bool())
                .unwrap_or(false),
            conf: item.get("conf").and_then(num_f32).unwrap_or(0.5),
            quote: item
                .get("quote")
                .and_then(|s| s.as_str())
                .map(|s| truncate(s.trim(), 300))
                .unwrap_or_default(),
        });
    }
    Ok(steps)
}

/// Pull the model's text out of a Responses API envelope
/// (`output[].content[].text`), with a Chat Completions
/// (`choices[0].message.content`) fallback. Returns `None` when
/// neither shape carries text.
fn response_text(v: &Value) -> Option<String> {
    let mut out = String::new();
    if let Some(outputs) = v.get("output").and_then(|o| o.as_array()) {
        for item in outputs {
            if let Some(content) = item.get("content").and_then(|c| c.as_array()) {
                for part in content {
                    if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                        out.push_str(t);
                    }
                }
            }
            if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                out.push_str(t);
            }
        }
    }
    if out.trim().is_empty() {
        if let Some(t) = v
            .pointer("/choices/0/message/content")
            .and_then(|c| c.as_str())
        {
            out.push_str(t);
        }
    }
    if out.trim().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Upload once (auto-expires in 30 days so team storage never grows
/// forever), ask Spark for the step draft, delete the upload
/// best-effort, and return the validated steps.
pub async fn extract_instructions(
    endpoint: &str,
    model: &str,
    api_key: &str,
    filename: &str,
    mp4: &[u8],
) -> Result<Vec<DraftStep>> {
    let endpoint = endpoint.trim_end_matches('/');
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| RoseError::Model(format!("http client: {e}")))?;

    // 1. Files API: purpose first, file last (docs' latency advice).
    let part = reqwest::multipart::Part::bytes(mp4.to_vec())
        .file_name(filename.to_string())
        .mime_str("video/mp4")
        .map_err(|e| RoseError::Model(format!("bad upload mime: {e}")))?;
    let form = reqwest::multipart::Form::new()
        .text("purpose", "user_data")
        .text("expires_after[anchor]", "created_at")
        .text("expires_after[seconds]", "2592000")
        .part("file", part);
    let up = http
        .post(format!("{endpoint}/files"))
        .bearer_auth(api_key)
        .multipart(form)
        .send()
        .await
        .map_err(|e| RoseError::Model(format!("file upload failed: {e}")))?;
    let status = up.status();
    let text = up.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(RoseError::Model(format!(
            "file upload HTTP {status}: {}",
            truncate(&text, 300)
        )));
    }
    let file_id: String = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.get("id").and_then(|i| i.as_str()).map(str::to_string))
        .ok_or_else(|| RoseError::Model("upload response has no file id".to_string()))?;

    // 2. Responses API: one call reads frames + narration together.
    let prompt = format!(
        "{VIDEO_PROMPT}\n\nFile: {filename}\nReply with ONE JSON object {{\"steps\": [...]}} only."
    );
    let body = serde_json::json!({
        "model": model,
        "max_output_tokens": 8000,
        "input": [{
            "type": "message",
            "role": "user",
            "content": [
                { "type": "input_text", "text": prompt },
                { "type": "input_file", "file_id": file_id },
            ],
        }],
    });
    let url = format!("{endpoint}/responses");
    let mut last_err = String::new();
    for attempt in 1..=2 {
        let resp = http
            .post(&url)
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| RoseError::Model(format!("video request failed: {e}")))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            last_err = format!("transient HTTP {status}");
            tracing::warn!(attempt, %status, "spark video transient error, retrying");
            tokio::time::sleep(std::time::Duration::from_secs(attempt * 2)).await;
            continue;
        }
        if !status.is_success() {
            // Best-effort cleanup before reporting.
            let _ = http
                .delete(format!("{endpoint}/files/{file_id}"))
                .bearer_auth(api_key)
                .send()
                .await;
            return Err(RoseError::Model(format!(
                "video HTTP {status}: {}",
                truncate(&text, 300)
            )));
        }
        let parsed: Value = serde_json::from_str(&text).map_err(|e| {
            RoseError::Model(format!(
                "bad responses envelope ({e}): {}",
                truncate(&text, 200)
            ))
        })?;
        match response_text(&parsed).map(|t| parse_steps(&t)) {
            Some(Ok(steps)) => {
                let _ = http
                    .delete(format!("{endpoint}/files/{file_id}"))
                    .bearer_auth(api_key)
                    .send()
                    .await;
                return Ok(steps);
            }
            Some(Err(e)) => {
                last_err = e.to_string();
                tracing::warn!(attempt, error = %last_err, "bad video JSON, retrying once");
            }
            None => {
                last_err = "response carried no text".to_string();
                tracing::warn!(attempt, "empty video response, retrying once");
            }
        }
    }
    // Best-effort cleanup on the way out.
    let _ = http
        .delete(format!("{endpoint}/files/{file_id}"))
        .bearer_auth(api_key)
        .send()
        .await;
    Err(RoseError::Model(format!(
        "spark could not draft instructions: {last_err}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_steps_object_shape() {
        let steps = parse_steps(
            r#"{"steps": [
                {"ts": 12, "say": "Open portal", "intent": "open portal",
                 "params": "url = {{url}}", "approval": false, "conf": 0.93,
                 "quote": "opening the portal"},
                {"ts": 31.7, "say": "Go to Invoices", "intent": "",
                 "approval": "yes", "conf": 1.5}
            ]}"#,
        )
        .expect("parse");
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].ts, 12);
        assert_eq!(steps[0].params, "url = {{url}}");
        assert!(!steps[0].approval);
        // Rounding, defaults, clamping.
        assert_eq!(steps[1].ts, 32);
        assert_eq!(
            steps[1].intent, "Go to Invoices",
            "empty intent falls back to say"
        );
        assert!(!steps[1].approval, "non-bool approval defaults to false");
        assert_eq!(steps[1].conf, 1.0, "confidence clamps to 1");
        assert_eq!(steps[1].quote, "");
    }

    #[test]
    fn parses_bare_array_and_fences() {
        let steps = parse_steps("```json\n[{\"say\": \"Only\"}]\n```").expect("parse");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].say, "Only");
        assert_eq!(steps[0].ts, 0);
        assert_eq!(steps[0].conf, 0.5);
    }

    #[test]
    fn rejects_garbage_and_missing_steps() {
        assert!(parse_steps("not json at all").is_err());
        assert!(parse_steps(r#"{"status": "done"}"#).is_err());
        assert!(
            parse_steps("[1, 2]").is_ok(),
            "non-objects become defaulted steps"
        );
    }

    #[test]
    fn caps_at_fifty_steps() {
        let mut items = Vec::new();
        for i in 0..70 {
            items.push(format!("{{\"say\": \"s{i}\"}}"));
        }
        let steps = parse_steps(&format!("[{}]", items.join(","))).expect("parse");
        assert_eq!(steps.len(), 50);
    }

    #[test]
    fn extracts_text_from_responses_envelope() {
        let v: Value = serde_json::json!({
            "output": [
                {"type": "reasoning", "summary": []},
                {"type": "message", "content": [
                    {"type": "output_text", "text": "{\"steps\": []}"},
                ]},
            ],
        });
        assert_eq!(response_text(&v).as_deref(), Some("{\"steps\": []}"));
    }

    #[test]
    fn extracts_text_from_chat_envelope_fallback() {
        let v: Value = serde_json::json!({
            "choices": [{"message": {"content": "hello"}}],
        });
        assert_eq!(response_text(&v).as_deref(), Some("hello"));
        let v: Value = serde_json::json!({"output": []});
        assert_eq!(response_text(&v), None);
    }
}
