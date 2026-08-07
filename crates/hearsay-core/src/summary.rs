//! AI titles and summaries.
//!
//! This is the only code in Hearsay that sends anything off the machine, and it runs
//! only when the user asks for a summary with their own key. Recording, transcription,
//! search and playback never call it — a missing key degrades the app to transcripts,
//! it does not block anything.
//!
//! Summaries are derived data. They are generated from stored segments and can be
//! regenerated at any time without re-transcribing, which is why the transcript, not the
//! summary, is the source of truth.
//!
//! Rust has no official Anthropic SDK, so this speaks the Messages API over HTTP
//! directly.

use crate::db::Segment;
use crate::secrets;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";

/// Server-side fallback: if a safety classifier declines the request, the API retries it
/// on another model within the same call rather than handing back a refusal. A meeting
/// transcript can mention almost anything, so a false positive should not cost the user
/// their summary.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

/// The model used for summaries when Anthropic is the provider.
pub const DEFAULT_MODEL: &str = "claude-opus-5";

const GEMINI_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models";
/// Gemini model used for summaries.
///
/// An alias rather than a version number, deliberately. `gemini-2.5-flash` was pinned
/// here and stopped being available to new keys within weeks — Google retires numbered
/// models faster than a local-first app gets rebuilt. `gemini-flash-latest` is
/// maintained by Google to point at the current Flash model, so it cannot rot the same
/// way. Flash rather than Pro because this is summarising, not reasoning.
pub const DEFAULT_GEMINI_MODEL: &str = "gemini-flash-latest";

/// Generous, because thinking tokens count against this ceiling and a truncated summary
/// is worse than a slow one.
const MAX_TOKENS: u32 = 16_000;

/// Summaries of an hour-long meeting take a while. Well above the default so a long
/// transcript does not fail on a client-side timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// A generated summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    /// A short title the model proposes. Stored as `ai_title`, never overwriting the
    /// user's own title.
    pub title: String,
    /// The summary body, in markdown.
    pub summary_md: String,
    /// Extracted commitments. Rendered into the markdown before storage so the summary
    /// is self-contained.
    #[serde(default)]
    pub action_items: Vec<ActionItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionItem {
    pub text: String,
    /// Who owns it: "you", "them", or "unassigned". The model is told not to guess names.
    #[serde(default)]
    pub owner: String,
}

impl Summary {
    /// The full markdown to store, with action items appended as a section.
    pub fn to_markdown(&self) -> String {
        let mut markdown = self.summary_md.trim().to_string();
        if self.action_items.is_empty() {
            return markdown;
        }
        markdown.push_str("\n\n## Action items\n\n");
        for item in &self.action_items {
            let owner = match item.owner.as_str() {
                "you" => "You",
                "them" => "Them",
                _ => "Unassigned",
            };
            markdown.push_str(&format!("- **{owner}** — {}\n", item.text.trim()));
        }
        markdown
    }
}

/// Whether summaries are available right now. The UI uses this to explain the absence of
/// a summary rather than showing a broken button.
pub fn is_available() -> bool {
    secrets::has_summary_key()
}

/// Which service generates summaries.
///
/// Two providers rather than one because the choice is the user's: it is their key, their
/// account, and their data leaving the machine. Everything else in the app is identical
/// either way — only this one call differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Anthropic,
    Gemini,
}

impl Provider {
    pub fn current() -> Self {
        match secrets::summary_provider().as_str() {
            "gemini" => Provider::Gemini,
            _ => Provider::Anthropic,
        }
    }

    pub fn default_model(self) -> &'static str {
        match self {
            Provider::Anthropic => DEFAULT_MODEL,
            Provider::Gemini => DEFAULT_GEMINI_MODEL,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Anthropic => "anthropic",
            Provider::Gemini => "gemini",
        }
    }
}

/// Generates a summary from an event's transcript.
///
/// Blocking; call it from a worker thread. Returns a clear error when there is no key,
/// no transcript, or the API declines — the caller shows these to the user verbatim.
pub fn summarize(
    segments: &[Segment],
    mute_spans: &[(i64, i64)],
    model: &str,
) -> Result<Summary> {
    let transcript = render_transcript(segments, mute_spans);
    if transcript.trim().is_empty() {
        return Err(anyhow!(
            "this recording has no transcript yet, and summaries are written from the \
             transcript"
        ));
    }

    match Provider::current() {
        Provider::Anthropic => summarize_anthropic(&transcript, model),
        Provider::Gemini => summarize_gemini(&transcript, model),
    }
}

fn summarize_anthropic(transcript: &str, model: &str) -> Result<Summary> {
    let key = secrets::api_key()?.ok_or_else(|| {
        anyhow!(
            "no Anthropic API key is set. Add one in settings — everything else in \
             Hearsay works without it."
        )
    })?;

    let request = serde_json::json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        // Ask the API to fall back rather than return a refusal.
        "fallbacks": "default",
        "system": SYSTEM_PROMPT,
        // A JSON schema instead of asking for markdown and parsing it back out: the
        // title and the action items arrive as separate fields rather than as headings
        // this code would have to find.
        "output_config": {
            "format": {
                "type": "json_schema",
                "schema": schema(),
            }
        },
        "messages": [{
            "role": "user",
            "content": format!(
                "Here is the transcript of a meeting.\n\n\
                 `You` is the person who recorded it; `Them` is everyone else.\n\n\
                 <transcript>\n{transcript}\n</transcript>"
            ),
        }],
    });

    let client = reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("could not build the HTTP client")?;

    let response = client
        .post(API_URL)
        .header("x-api-key", &key)
        .header("anthropic-version", API_VERSION)
        .header("anthropic-beta", FALLBACK_BETA)
        .header("content-type", "application/json")
        .json(&request)
        .send()
        .context("could not reach the Anthropic API")?;

    let status = response.status();
    let body: serde_json::Value = response
        .json()
        .context("the Anthropic API returned a response that could not be read")?;

    if !status.is_success() {
        // Report what the API said, but never echo the request — it contains the
        // transcript, and the headers contain the key.
        let message = body
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("no detail given");
        return Err(anyhow!("the Anthropic API rejected the request ({status}): {message}"));
    }

    // Check why generation stopped before reading any content: a refusal comes back as
    // a successful response with empty or partial content.
    match body.get("stop_reason").and_then(serde_json::Value::as_str) {
        Some("refusal") => {
            return Err(anyhow!(
                "the model declined to summarise this recording. The transcript and \
                 audio are untouched."
            ))
        }
        Some("max_tokens") => {
            return Err(anyhow!(
                "the summary was cut short because the recording is very long. Try \
                 summarising a shorter recording."
            ))
        }
        _ => {}
    }

    let text = body
        .get("content")
        .and_then(serde_json::Value::as_array)
        .and_then(|blocks| {
            blocks
                .iter()
                .find(|block| block.get("type").and_then(serde_json::Value::as_str) == Some("text"))
        })
        .and_then(|block| block.get("text"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow!("the model returned no summary text"))?;

    serde_json::from_str::<Summary>(text)
        .context("the model's summary did not match the expected shape")
}

/// The same job against Google's Gemini API.
///
/// A different shape entirely — the schema lives in `generationConfig.responseSchema`,
/// the system prompt in `systemInstruction`, and the answer comes back nested under
/// `candidates`. Only this function knows any of that.
fn summarize_gemini(transcript: &str, model: &str) -> Result<Summary> {
    let key = secrets::gemini_key()?.ok_or_else(|| {
        anyhow!(
            "no Gemini API key is set. Add one in settings — everything else in Hearsay \
             works without it."
        )
    })?;

    let request = serde_json::json!({
        "systemInstruction": { "parts": [{ "text": SYSTEM_PROMPT }] },
        "contents": [{
            "role": "user",
            "parts": [{ "text": format!(
                "Here is the transcript of a meeting.\n\n\
                 `You` is the person who recorded it; `Them` is everyone else.\n\n\
                 <transcript>\n{transcript}\n</transcript>"
            )}],
        }],
        "generationConfig": {
            "responseMimeType": "application/json",
            "responseSchema": gemini_schema(),
        },
    });

    let client = reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("could not build the HTTP client")?;

    let response = client
        .post(format!("{GEMINI_URL}/{model}:generateContent"))
        // In the header, not the query string: a key in a URL ends up in logs.
        .header("x-goog-api-key", &key)
        .header("content-type", "application/json")
        .json(&request)
        .send()
        .context("could not reach the Gemini API")?;

    let status = response.status();
    let body: serde_json::Value = response
        .json()
        .context("the Gemini API returned a response that could not be read")?;

    if !status.is_success() {
        let message = body
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("no detail given");
        if status.as_u16() == 404 {
            return Err(anyhow!(
                "Gemini does not offer the model {model} to this key ({message}). \
                 Google retires models periodically; Hearsay defaults to \
                 `gemini-flash-latest`, which tracks the current one."
            ));
        }
        return Err(anyhow!("the Gemini API rejected the request ({status}): {message}"));
    }

    let candidate = body
        .get("candidates")
        .and_then(serde_json::Value::as_array)
        .and_then(|list| list.first())
        .ok_or_else(|| anyhow!("Gemini returned no summary"))?;

    // Same reason as the Anthropic path: find out why it stopped before reading content.
    match candidate.get("finishReason").and_then(serde_json::Value::as_str) {
        Some("SAFETY") | Some("PROHIBITED_CONTENT") => {
            return Err(anyhow!(
                "Gemini declined to summarise this recording. The transcript and audio \
                 are untouched."
            ))
        }
        Some("MAX_TOKENS") => {
            return Err(anyhow!(
                "the summary was cut short because the recording is very long."
            ))
        }
        _ => {}
    }

    let text = candidate
        .get("content")
        .and_then(|content| content.get("parts"))
        .and_then(serde_json::Value::as_array)
        .and_then(|parts| parts.first())
        .and_then(|part| part.get("text"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow!("Gemini returned no summary text"))?;

    serde_json::from_str::<Summary>(text)
        .context("Gemini's summary did not match the expected shape")
}

/// The same fields as [`schema`], in the dialect Gemini accepts.
///
/// Gemini uses uppercase type names and rejects `additionalProperties`, so the schema
/// cannot simply be shared between the two providers.
fn gemini_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "OBJECT",
        "properties": {
            "title": { "type": "STRING" },
            "summary_md": { "type": "STRING" },
            "action_items": {
                "type": "ARRAY",
                "items": {
                    "type": "OBJECT",
                    "properties": {
                        "text": { "type": "STRING" },
                        "owner": { "type": "STRING", "enum": ["you", "them", "unassigned"] },
                    },
                    "required": ["text", "owner"],
                },
            },
        },
        "required": ["title", "summary_md", "action_items"],
        "propertyOrdering": ["title", "summary_md", "action_items"],
    })
}

fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "title": {
                "type": "string",
                "description": "A specific title of at most 60 characters, in sentence case.",
            },
            "summary_md": {
                "type": "string",
                "description": "The summary in markdown, using ## headings. No title heading.",
            },
            "action_items": {
                "type": "array",
                "description": "Commitments actually made. Empty if none were.",
                "items": {
                    "type": "object",
                    "properties": {
                        "text": { "type": "string" },
                        "owner": { "type": "string", "enum": ["you", "them", "unassigned"] },
                    },
                    "required": ["text", "owner"],
                    "additionalProperties": false,
                },
            },
        },
        "required": ["title", "summary_md", "action_items"],
        "additionalProperties": false,
    })
}

const SYSTEM_PROMPT: &str = "\
You summarise meeting transcripts for the person who recorded them.

The transcript comes from automatic speech recognition, so expect mangled names, wrong \
homophones, and missing punctuation. Read through those errors rather than quoting them.

Write the summary for someone who was in the meeting and wants to remember what happened \
— not for someone who missed it. Lead with what was decided and what changed. Leave out \
the greetings, the scheduling chatter, and anything that would be obvious to a person who \
was there.

Only record what the transcript supports. If something was discussed without being \
settled, say it was left open rather than inventing a resolution. Where the transcript is \
too garbled to interpret, leave it out instead of guessing.

Action items are commitments someone actually made, not topics that were mentioned. \
Attribute each to `you` (the person recording) or `them`, and use `unassigned` when \
nobody clearly took it — never guess a name.

A line reading `[mic muted]` means the recorder deliberately muted their microphone. \
Their side of the conversation is missing there. Do not speculate about what was said.

Use sentence case in headings. Keep it proportionate: a ten-minute call needs a short \
paragraph, not a structured report.";

/// Renders segments and mute spans into the text the model reads.
///
/// Timestamps are included so the summary can be traced back to the recording, and mute
/// spans are marked in place so a gap in the conversation is never mistaken for silence.
fn render_transcript(segments: &[Segment], mute_spans: &[(i64, i64)]) -> String {
    #[derive(Debug)]
    enum Line<'a> {
        Spoken(&'a Segment),
        Muted(i64, i64),
    }

    let mut lines: Vec<Line<'_>> = segments.iter().map(Line::Spoken).collect();
    lines.extend(mute_spans.iter().map(|(start, end)| Line::Muted(*start, *end)));
    lines.sort_by_key(|line| match line {
        Line::Spoken(segment) => segment.start_ms,
        Line::Muted(start, _) => *start,
    });

    let mut out = String::new();
    for line in lines {
        match line {
            Line::Spoken(segment) => {
                let speaker = if segment.channel == "mic" { "You" } else { "Them" };
                out.push_str(&format!(
                    "[{}] {speaker}: {}\n",
                    clock(segment.start_ms),
                    segment.text.trim()
                ));
            }
            Line::Muted(start, end) => {
                out.push_str(&format!("[{}] [mic muted until {}]\n", clock(start), clock(end)));
            }
        }
    }
    out
}

fn clock(ms: i64) -> String {
    let total = (ms.max(0) / 1000) as u64;
    format!("{:02}:{:02}", total / 60, total % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(id: i64, channel: &str, start_ms: i64, text: &str) -> Segment {
        Segment {
            id,
            event_id: 1,
            channel: channel.to_string(),
            start_ms,
            end_ms: start_ms + 1000,
            text: text.to_string(),
        }
    }

    #[test]
    fn the_transcript_labels_who_was_speaking() {
        let rendered = render_transcript(
            &[
                segment(1, "system", 0, "Welcome everyone"),
                segment(2, "mic", 5_000, "Thanks for having me"),
            ],
            &[],
        );
        assert!(rendered.contains("[00:00] Them: Welcome everyone"));
        assert!(rendered.contains("[00:05] You: Thanks for having me"));
    }

    /// A muted stretch must reach the model, or it will summarise a gap as if nothing
    /// was said there.
    #[test]
    fn muted_stretches_appear_in_the_transcript_sent_to_the_model() {
        let rendered = render_transcript(
            &[
                segment(1, "system", 0, "Any objections?"),
                segment(2, "system", 90_000, "Right, moving on"),
            ],
            &[(10_000, 65_000)],
        );
        assert!(
            rendered.contains("[00:10] [mic muted until 01:05]"),
            "got: {rendered}"
        );
        // And it must land between the two spoken lines, not at the end.
        let muted_at = rendered.find("mic muted").expect("marker present");
        let moving_on = rendered.find("moving on").expect("later line present");
        assert!(muted_at < moving_on);
    }

    #[test]
    fn action_items_are_rendered_into_the_stored_markdown() {
        let summary = Summary {
            title: "Migration planning".into(),
            summary_md: "## What was decided\n\nWe are moving in November.".into(),
            action_items: vec![
                ActionItem {
                    text: "Draft the rollout plan".into(),
                    owner: "you".into(),
                },
                ActionItem {
                    text: "Confirm the freeze window".into(),
                    owner: "unassigned".into(),
                },
            ],
        };

        let markdown = summary.to_markdown();
        assert!(markdown.contains("## Action items"));
        assert!(markdown.contains("- **You** — Draft the rollout plan"));
        assert!(markdown.contains("- **Unassigned** — Confirm the freeze window"));
    }

    #[test]
    fn a_summary_with_no_commitments_has_no_action_items_section() {
        let summary = Summary {
            title: "Weekly sync".into(),
            summary_md: "Nothing was decided.".into(),
            action_items: vec![],
        };
        assert!(!summary.to_markdown().contains("Action items"));
    }

    #[test]
    fn the_schema_forbids_extra_fields() {
        let schema = schema();
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));
        assert_eq!(
            schema["properties"]["action_items"]["items"]["additionalProperties"],
            serde_json::json!(false)
        );
    }

    #[test]
    fn summaries_deserialise_from_the_model_shape() {
        // Three hashes: the JSON contains the sequence `"##` (a quote followed by a
        // markdown heading), which would close a one- or two-hash raw string early.
        let raw = r###"{"title":"Q3 planning","summary_md":"## Decisions\n\nShip in July.",
                        "action_items":[{"text":"Send the doc","owner":"them"}]}"###;
        let summary: Summary = serde_json::from_str(raw).expect("summary parses");
        assert_eq!(summary.title, "Q3 planning");
        assert_eq!(summary.action_items.len(), 1);
        assert_eq!(summary.action_items[0].owner, "them");
    }
}
