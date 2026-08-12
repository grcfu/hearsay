//! Asking questions about a recording.
//!
//! The second thing in Hearsay that sends anything off the machine, and it runs under the
//! same terms as the first: the user's own key, the user's chosen provider, and only when
//! they type a question and press send. Nothing is sent in the background, no question is
//! asked on the user's behalf, and no recording is uploaded — only the transcript text of
//! the one recording being asked about.
//!
//! It exists so that finding something in a recording does not mean copying the whole
//! transcript into somebody else's chat window, which would send the same text to a
//! service the user has not chosen, under an account they may not control.
//!
//! Answers are grounded in the transcript and nothing else. The model is told to say when
//! the transcript does not contain the answer rather than filling the gap — the same rule
//! as summaries (`CLAUDE.md` §8a), and for the same reason: a recording is a record, and a
//! plausible invention in front of it is worse than an admission of not knowing.

use crate::db::Segment;
use crate::secrets;
use crate::summary::{
    render_transcript, speaker_or_default, Provider, API_URL, API_VERSION, FALLBACK_BETA,
    GEMINI_URL,
};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Generous, for the same reason the summary ceiling is (`summary::MAX_TOKENS`):
/// **thinking tokens count against it.** An answer is only a few paragraphs, but
/// `gemini-flash-latest` thinks by default, and a hard question over an hour of transcript
/// can spend thousands of tokens before writing a word. Sized to the answer, the model
/// stops mid-thought and the reply comes back as `MAX_TOKENS` with no text — which this
/// code correctly reports as a failure, making every question look broken.
const MAX_TOKENS: u32 = 16_000;

/// Shorter than a summary's, because a question is asked while somebody waits for it.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// One message in a conversation about a recording.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    /// `user` or `assistant`.
    pub role: String,
    pub content: String,
}

impl Turn {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
        }
    }

    fn is_assistant(&self) -> bool {
        self.role == "assistant"
    }
}

/// Answers `question` about a recording, given everything asked so far.
///
/// Blocking; call it from a worker thread. `history` is the conversation before this
/// question, oldest first, and does not include it.
pub fn ask(
    segments: &[Segment],
    mute_spans: &[(i64, i64)],
    history: &[Turn],
    question: &str,
    model: &str,
    speaker: Option<&str>,
) -> Result<String> {
    let question = question.trim();
    if question.is_empty() {
        return Err(anyhow!("there is no question to ask"));
    }

    let speaker = speaker_or_default(speaker);
    let transcript = render_transcript(segments, mute_spans, Some(speaker));
    if transcript.trim().is_empty() {
        return Err(anyhow!(
            "this recording has no transcript yet, and questions are answered from the \
             transcript"
        ));
    }

    match Provider::current() {
        Provider::Anthropic => ask_anthropic(&transcript, history, question, model, speaker),
        Provider::Gemini => ask_gemini(&transcript, history, question, model, speaker),
    }
}

fn ask_anthropic(
    transcript: &str,
    history: &[Turn],
    question: &str,
    model: &str,
    speaker: &str,
) -> Result<String> {
    let key = secrets::api_key()?.ok_or_else(|| {
        anyhow!(
            "no Anthropic API key is set. Add one in settings — everything else in \
             Hearsay works without it."
        )
    })?;

    let mut messages: Vec<serde_json::Value> = history
        .iter()
        .map(|turn| {
            serde_json::json!({
                "role": if turn.is_assistant() { "assistant" } else { "user" },
                "content": turn.content,
            })
        })
        .collect();
    messages.push(serde_json::json!({ "role": "user", "content": question }));

    let request = serde_json::json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        // As with summaries: a transcript can mention almost anything, and a false
        // positive from a safety classifier should not cost the user their answer.
        "fallbacks": "default",
        "system": system_prompt(transcript, speaker),
        "messages": messages,
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
        // Never echo the request: it carries the transcript, and the headers the key.
        let message = body
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("no detail given");
        return Err(anyhow!("the Anthropic API rejected the question ({status}): {message}"));
    }

    match body.get("stop_reason").and_then(serde_json::Value::as_str) {
        Some("refusal") => {
            return Err(anyhow!(
                "the model declined to answer that. The transcript and audio are untouched."
            ))
        }
        Some("max_tokens") => {
            return Err(anyhow!(
                "the answer was cut short. Try asking something narrower."
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
        .ok_or_else(|| anyhow!("the model returned no answer"))?;

    Ok(text.trim().to_string())
}

/// The same job against Gemini, which names the assistant role `model` and puts the
/// system prompt in `systemInstruction`.
fn ask_gemini(
    transcript: &str,
    history: &[Turn],
    question: &str,
    model: &str,
    speaker: &str,
) -> Result<String> {
    let key = secrets::gemini_key()?.ok_or_else(|| {
        anyhow!(
            "no Gemini API key is set. Add one in settings — everything else in Hearsay \
             works without it."
        )
    })?;

    let mut contents: Vec<serde_json::Value> = history
        .iter()
        .map(|turn| {
            serde_json::json!({
                "role": if turn.is_assistant() { "model" } else { "user" },
                "parts": [{ "text": turn.content }],
            })
        })
        .collect();
    contents.push(serde_json::json!({
        "role": "user",
        "parts": [{ "text": question }],
    }));

    let request = serde_json::json!({
        "systemInstruction": { "parts": [{ "text": system_prompt(transcript, speaker) }] },
        "contents": contents,
        "generationConfig": { "maxOutputTokens": MAX_TOKENS },
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
        return Err(anyhow!("the Gemini API rejected the question ({status}): {message}"));
    }

    let candidate = body
        .get("candidates")
        .and_then(serde_json::Value::as_array)
        .and_then(|list| list.first())
        .ok_or_else(|| anyhow!("Gemini returned no answer"))?;

    match candidate.get("finishReason").and_then(serde_json::Value::as_str) {
        Some("SAFETY") | Some("PROHIBITED_CONTENT") => {
            return Err(anyhow!(
                "Gemini declined to answer that. The transcript and audio are untouched."
            ))
        }
        Some("MAX_TOKENS") => {
            return Err(anyhow!(
                "the answer was cut short. Try asking something narrower."
            ))
        }
        _ => {}
    }

    let text = candidate
        .get("content")
        .and_then(|content| content.get("parts"))
        .and_then(serde_json::Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| anyhow!("Gemini returned no answer text"))?;

    Ok(text.trim().to_string())
}

/// The instructions, with the transcript attached as the material to answer from.
///
/// The transcript goes here rather than into the first user turn so that the conversation
/// stays a conversation: every message in `messages` is something the person or the model
/// actually said, and nothing has to be skipped when the history is replayed.
pub(crate) fn system_prompt(transcript: &str, speaker: &str) -> String {
    format!(
        "{}\n\n\
         `{speaker}` is the person who made the recording; `Them` is everyone else.\n\n\
         <transcript>\n{transcript}\n</transcript>",
        INSTRUCTIONS.replace("{name}", speaker)
    )
}

/// Editable in one place, like the summary prompt. Rules, not a persona.
const INSTRUCTIONS: &str = "\
You are helping {name} search and understand a recording they made. The transcript is \
below, with timestamps.

- Answer only from the transcript. It is the whole record of what was said.
- When the transcript does not answer the question, say so plainly and stop. Do not \
  reason towards a likely answer, and do not offer general knowledge instead — {name} is \
  asking what was said in this conversation, not what is usually true.
- Cite timestamps as [MM:SS] for anything specific, so {name} can go and hear it.
- Quote the transcript when the exact words matter, especially for numbers, names, \
  dates, and commitments.
- `[mic muted until MM:SS]` means the microphone was deliberately turned off for that \
  stretch. Never speculate about what was said there; if the answer might be in a muted \
  stretch, say that.
- Transcription makes mistakes. If a name or number looks garbled, say what the \
  transcript reads and that it may be misheard rather than silently correcting it.
- Be brief and direct. This is a working tool, not a report. Short paragraphs or bullets, \
  no preamble, no offer of further help.";

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(id: i64, channel: &str, start_ms: i64, text: &str) -> Segment {
        Segment {
            id,
            event_id: 1,
            channel: channel.to_string(),
            start_ms,
            end_ms: start_ms + 1_000,
            text: text.to_string(),
        }
    }

    #[test]
    fn the_prompt_carries_the_transcript_and_the_name() {
        let segments = [segment(1, "mic", 0, "I can start on Monday")];
        let transcript = render_transcript(&segments, &[], Some("Anikait"));
        let prompt = system_prompt(&transcript, "Anikait");

        assert!(prompt.contains("I can start on Monday"));
        assert!(prompt.contains("[00:00] Anikait:"));
        assert!(prompt.contains("`Anikait` is the person who made the recording"));
        // The instructions are addressed to the same name the transcript labels use.
        assert!(prompt.contains("helping Anikait search"));
    }

    /// The rule that matters most: an answer that is not in the recording is worse than
    /// no answer, because the whole point is to be able to trust the record.
    #[test]
    fn the_prompt_forbids_answering_from_outside_the_transcript() {
        let prompt = system_prompt("[00:00] You: hello", "You");
        assert!(prompt.contains("Answer only from the transcript"));
        assert!(prompt.contains("do not offer general knowledge instead"));
        assert!(prompt.contains("Never speculate"));
    }

    #[test]
    fn a_blank_question_is_refused_before_anything_is_sent() {
        let segments = [segment(1, "system", 0, "something was said")];
        let error = ask(&segments, &[], &[], "   ", "model", None)
            .expect_err("a blank question should not reach the network");
        assert!(error.to_string().contains("no question"));
    }

    #[test]
    fn a_recording_with_no_transcript_cannot_be_asked_about() {
        let error = ask(&[], &[], &[], "what did they say?", "model", None)
            .expect_err("there is nothing to answer from");
        assert!(error.to_string().contains("no transcript"));
    }

    #[test]
    fn turns_carry_their_role() {
        assert_eq!(Turn::user("q").role, "user");
        assert!(Turn::assistant("a").is_assistant());
        assert!(!Turn::user("q").is_assistant());
    }
}
