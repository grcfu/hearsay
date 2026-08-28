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

pub(crate) const API_URL: &str = "https://api.anthropic.com/v1/messages";
pub(crate) const API_VERSION: &str = "2023-06-01";

/// Server-side fallback: if a safety classifier declines the request, the API retries it
/// on another model within the same call rather than handing back a refusal. A meeting
/// transcript can mention almost anything, so a false positive should not cost the user
/// their summary.
pub(crate) const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

/// The model used for summaries when Anthropic is the provider.
pub const DEFAULT_MODEL: &str = "claude-opus-5";

pub(crate) const GEMINI_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models";

/// Gemini models to try for a summary, in order, stopping at the first that answers.
///
/// A list rather than one name, because the two ways of naming a single model both fail
/// and they fail in opposite directions:
///
/// - **A pinned version rots.** `gemini-2.5-flash` was pinned here and stopped being
///   available to new keys within weeks. Google retires numbered models faster than a
///   local-first app gets rebuilt.
/// - **An alias rides whatever Google shipped last week.** `gemini-flash-latest` was the
///   fix for the above, and it is hot-swapped to the newest Flash release — *including a
///   preview or experimental one*. A newly released model is capacity-starved for its
///   first weeks, and the API sheds that load as 503 "this model is currently
///   experiencing high demand". That is server-side and Google-wide: it is not the key,
///   not the request, and not something a paid tier or a longer backoff fixes.
///
/// So neither is trusted alone. A mature stable release is tried first — summarising is
/// not a frontier task, and §8a already picks Flash over Pro for the same reason — and
/// the alias sits at the end as the backstop that cannot rot. Whichever answers is
/// recorded in `events.model_used`, so what produced a summary is never a guess.
pub const GEMINI_MODELS: &[&'static str] = &[
    "gemini-3.5-flash",
    "gemini-3.6-flash",
    "gemini-2.5-flash",
    // Last, deliberately: if every name above has been retired, this is still something.
    "gemini-flash-latest",
];

/// The first candidate, for anything that needs a single name to show.
pub const DEFAULT_GEMINI_MODEL: &str = GEMINI_MODELS[0];

/// Generous, because thinking tokens count against this ceiling and a truncated summary
/// is worse than a slow one.
const MAX_TOKENS: u32 = 16_000;

/// Summaries of an hour-long meeting take a while. Well above the default so a long
/// transcript does not fail on a client-side timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// How many times a request is sent before a busy provider is reported as a failure.
///
/// Three, not more: past that the user is waiting long enough that being told to try
/// again themselves is more honest than a spinner that keeps its reasons to itself.
const SEND_ATTEMPTS: u32 = 3;

/// How long to wait before the second attempt when the provider does not say. Doubled
/// for each one after it.
///
/// Five, not the two it was: a capacity spike at either provider routinely outlasts the
/// six seconds three attempts used to span, and giving up inside it reports an outage
/// that has already passed. Where the provider names its own delay that is used instead
/// — see `Busy::retry_after`.
const RETRY_BACKOFF: Duration = Duration::from_secs(5);

/// The longest wait a provider can ask for and be obeyed.
///
/// A provider under load sometimes names a delay in minutes. Honouring that would leave
/// the user in front of a spinner with no way to tell it apart from a hang, and §8a's
/// three attempts are meant to be a blip absorbed rather than a queue joined. Past this
/// the wait is capped and, if the provider is still busy, reported.
const MAX_ASKED_WAIT: Duration = Duration::from_secs(30);

/// A provider that was too busy to look at the request.
///
/// Its own kind of error because it is the only kind worth sending again: it says
/// nothing about the request, so the same one is likely to succeed a moment later. A
/// rejected request — a bad key, a retired model, a transcript too long — will be
/// rejected identically however many times it is sent.
#[derive(Debug, thiserror::Error)]
#[error("{provider} is too busy to answer right now ({status}): {message}")]
pub struct Busy {
    pub provider: &'static str,
    pub status: u16,
    pub message: String,
    /// How long the provider asked to be left alone, when it said.
    ///
    /// Both providers will name a delay — Anthropic in a `retry-after` header, Gemini in
    /// a `RetryInfo` inside the error body — and it is the only party that knows how long
    /// its own spike will last. Discarding it and waiting a fixed two seconds instead is
    /// what had Hearsay give up after six seconds on an outage the provider had said to
    /// come back to in twenty.
    pub retry_after: Option<Duration>,
}

/// The delay a provider named in a `retry-after` header: `retry-after: 25`.
///
/// The HTTP-date form is deliberately not read: it is dated by the provider's clock, and
/// a skewed one here would turn a short wait into a long one or none at all. Where
/// neither this nor `asked_wait_body` finds a figure the doubling wait applies — a
/// missing hint is not an error, it just means the provider did not say.
pub(crate) fn asked_wait_header(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let seconds: f64 = headers.get("retry-after")?.to_str().ok()?.trim().parse().ok()?;
    duration_from_seconds(seconds)
}

/// The delay a provider named in the error body. Google returns its as `error.details[]
/// = { @type: ...RetryInfo, retryDelay: "25s" }` rather than as a header.
pub(crate) fn asked_wait_body(body: &serde_json::Value) -> Option<Duration> {
    let details = body.get("error")?.get("details")?.as_array()?;
    for detail in details {
        let kind = detail.get("@type").and_then(serde_json::Value::as_str).unwrap_or("");
        if !kind.ends_with("RetryInfo") {
            continue;
        }
        let delay = detail.get("retryDelay").and_then(serde_json::Value::as_str)?;
        let seconds: f64 = delay.trim_end_matches('s').parse().ok()?;
        return duration_from_seconds(seconds);
    }
    None
}

/// A wait only counts if it is positive and finite: a provider asking for zero or for
/// something nonsensical is telling us nothing, and `Duration::from_secs_f64` panics on
/// a negative or a NaN.
fn duration_from_seconds(seconds: f64) -> Option<Duration> {
    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(seconds))
}

/// A model this key cannot reach: retired, renamed, or never offered to it.
///
/// Its own kind of error for the same reason `Busy` is: it says nothing about the
/// request, only about the name it was addressed to, so the next candidate is worth
/// trying. Every other rejection is a verdict on what was sent and stops the attempt.
#[derive(Debug, thiserror::Error)]
#[error("{provider} does not offer the model {model} to this key: {message}")]
pub struct ModelGone {
    pub provider: &'static str,
    pub model: String,
    pub message: String,
}

/// Whether an error means another model is worth trying: the provider ran out of
/// capacity for this one, or does not have it at all.
fn worth_another_model(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Busy>().is_some() || error.downcast_ref::<ModelGone>().is_some()
}

/// Tries each model in turn, moving on when one is out of capacity or gone, and returns
/// the answer along with the name that produced it.
///
/// The retry in `with_retry` is the inner loop — a blip on one model is waited out before
/// the next is considered — because switching model on a one-second wobble would change
/// which model wrote a summary for no reason. Only a model that stays unavailable for all
/// three attempts is given up on.
pub fn across_models<T>(
    models: &[&'static str],
    report: impl FnMut(RetryNotice),
    attempt: impl FnMut(&str) -> Result<T>,
) -> Result<(T, &'static str)> {
    walking(models, RETRY_BACKOFF, report, attempt)
}

/// The walk, with the first wait passed in so tests need not sit through three backoffs
/// per candidate.
fn walking<T>(
    models: &[&'static str],
    first_wait: Duration,
    mut report: impl FnMut(RetryNotice),
    mut attempt: impl FnMut(&str) -> Result<T>,
) -> Result<(T, &'static str)> {
    let mut tried: Vec<&str> = Vec::new();
    let mut last: Option<anyhow::Error> = None;

    for model in models {
        match retrying(first_wait, &mut report, || attempt(model)) {
            Ok(value) => return Ok((value, model)),
            Err(error) if worth_another_model(&error) => {
                tracing::warn!("{model} is unavailable, trying the next: {error:#}");
                tried.push(model);
                last = Some(error);
            }
            // A verdict on the request. Every other model would return it too.
            Err(error) => return Err(error),
        }
    }

    let error = last.unwrap_or_else(|| anyhow!("no model was configured to try"));
    // Name them all: told only about the last, this reads as one model being broken
    // rather than as the provider being out of capacity across the board.
    Err(error.context(format!("tried {}", tried.join(", "))))
}

/// Whether a status means "come back later" rather than "your request was wrong".
///
/// 503 and Anthropic's 529 are the provider out of capacity, 429 is a rate limit, and
/// 500/502/504 are its own faults. None of them are a verdict on what was sent.
pub fn is_transient(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504 | 529)
}

/// What the caller is told while a busy provider is being waited out.
///
/// A retry is silent from the outside: the button stays spinning and nothing says why.
/// Waiting out the provider's own delay made that silence long enough to look like a
/// hang, so the wait is reported rather than merely logged — the pane can say the
/// provider is busy and when the next attempt goes out.
#[derive(Debug, Clone, Copy)]
pub struct RetryNotice<'a> {
    /// The attempt that just came back busy, counting from one.
    pub attempt: u32,
    /// How many will be made in total.
    pub of: u32,
    /// How long the next one is being held back.
    pub waiting: Duration,
    /// What the provider said.
    pub busy: &'a Busy,
}

/// A reporter for callers with nowhere to put the notice — tests, and anything not
/// driving a user interface.
pub fn ignore_retries(_: RetryNotice) {}

/// Sends a request, trying again while the provider says it is too busy.
///
/// **Only a `Busy` is retried.** A request that timed out or could not connect is not,
/// even though it may well succeed on a second try: the provider may have received and
/// processed the first one, and sending the transcript out again for an answer that may
/// already exist is both a second upload and a second charge on the user's key. A
/// `Busy` carries the provider's own word that it did not get that far.
///
/// This adds no outbound trigger. It is the same explicit press, sent again after the
/// provider declined to handle it.
pub fn with_retry<T>(
    report: impl FnMut(RetryNotice),
    attempt: impl FnMut() -> Result<T>,
) -> Result<T> {
    retrying(RETRY_BACKOFF, report, attempt)
}

/// How long to wait before sending again: the provider's own figure where it gave one,
/// capped, and the doubling wait where it did not.
///
/// The provider knows how long its own spike will last and this code does not, so its
/// figure wins over the fallback even when it is the shorter of the two.
fn next_wait(busy: &Busy, fallback: Duration) -> Duration {
    match busy.retry_after {
        Some(asked) => asked.min(MAX_ASKED_WAIT),
        None => fallback,
    }
}

/// The retry loop, with the first wait passed in so tests need not sit through it.
fn retrying<T>(
    first_wait: Duration,
    mut report: impl FnMut(RetryNotice),
    mut attempt: impl FnMut() -> Result<T>,
) -> Result<T> {
    let mut wait = first_wait;
    let mut sent = 0u32;

    loop {
        sent += 1;
        let error = match attempt() {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };

        // Anything else is the provider's verdict on the request, and sending it a
        // second time would only collect the same verdict.
        let Some(busy) = error.downcast_ref::<Busy>() else {
            return Err(error);
        };

        if sent >= SEND_ATTEMPTS {
            return Err(error.context(format!(
                "gave up after {sent} attempts. Nothing was saved and the transcript \
                 and audio are untouched — try again in a minute"
            )));
        }

        let this_wait = next_wait(busy, wait);

        tracing::warn!(
            "provider busy on attempt {sent} of {SEND_ATTEMPTS}, waiting {:?}: {error:#}",
            this_wait
        );
        report(RetryNotice {
            attempt: sent,
            of: SEND_ATTEMPTS,
            waiting: this_wait,
            busy,
        });
        std::thread::sleep(this_wait);
        wait *= 2;
    }
}

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
    pub fn to_markdown(&self, speaker: Option<&str>) -> String {
        let speaker = speaker_or_default(speaker);
        let mut markdown = self.summary_md.trim().to_string();
        if self.action_items.is_empty() {
            return markdown;
        }
        markdown.push_str("\n\n## Action items\n\n");
        for item in &self.action_items {
            let owner = match item.owner.as_str() {
                "you" => speaker,
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

    /// The models to try, in order. One for Anthropic: it does not shed load onto a
    /// preview model the way an alias hot-swapped to the newest Flash does, and 529 is
    /// already handled by the retry.
    pub fn models(self) -> &'static [&'static str] {
        match self {
            Provider::Anthropic => &[DEFAULT_MODEL],
            Provider::Gemini => GEMINI_MODELS,
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
/// A summary, and the model that actually produced it.
///
/// The name is returned rather than assumed because the caller's first choice may have
/// been out of capacity — `events.model_used` must record what answered, not what was
/// asked first.
pub struct Summarized {
    pub summary: Summary,
    pub model: &'static str,
}

pub fn summarize(
    segments: &[Segment],
    markers: &[(Marker, i64, i64)],
    models: &[&'static str],
    speaker: Option<&str>,
    report: impl FnMut(RetryNotice),
) -> Result<Summarized> {
    let speaker = speaker_or_default(speaker);
    let transcript = render_transcript(segments, markers, Some(speaker));
    if transcript.trim().is_empty() {
        return Err(anyhow!(
            "this recording has no transcript yet, and summaries are written from the \
             transcript"
        ));
    }

    match Provider::current() {
        Provider::Anthropic => {
            across_models(models, report, |model| {
                summarize_anthropic(&transcript, model, speaker)
            })
        }
        Provider::Gemini => {
            across_models(models, report, |model| {
                summarize_gemini(&transcript, model, speaker)
            })
        }
    }
    .map(|(summary, model)| Summarized { summary, model })
}

fn summarize_anthropic(transcript: &str, model: &str, speaker: &str) -> Result<Summary> {
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
        "system": system_prompt(speaker),
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
            "content": user_prompt(transcript, speaker),
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
    // Read before the body: `json()` consumes the response, and a `retry-after` lives up
    // here.
    let asked = asked_wait_header(response.headers());
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
        if is_transient(status.as_u16()) {
            return Err(Busy {
                provider: "the Anthropic API",
                status: status.as_u16(),
                message: message.to_string(),
                retry_after: asked.or_else(|| asked_wait_body(&body)),
            }
            .into());
        }
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
fn summarize_gemini(transcript: &str, model: &str, speaker: &str) -> Result<Summary> {
    let key = secrets::gemini_key()?.ok_or_else(|| {
        anyhow!(
            "no Gemini API key is set. Add one in settings — everything else in Hearsay \
             works without it."
        )
    })?;

    let request = serde_json::json!({
        "systemInstruction": { "parts": [{ "text": system_prompt(speaker) }] },
        "contents": [{
            "role": "user",
            "parts": [{ "text": user_prompt(transcript, speaker) }],
        }],
        "generationConfig": {
            // Set here as it is on the chat path: without it the model's own default
            // ceiling applies, which is lower than MAX_TOKENS, and a long summary comes
            // back truncated as a MAX_TOKENS finish with the work already paid for.
            "maxOutputTokens": MAX_TOKENS,
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
    // Read before the body: `json()` consumes the response, and a `retry-after` lives up
    // here.
    let asked = asked_wait_header(response.headers());
    let body: serde_json::Value = response
        .json()
        .context("the Gemini API returned a response that could not be read")?;

    if !status.is_success() {
        let message = body
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("no detail given");
        if is_transient(status.as_u16()) {
            return Err(Busy {
                provider: "the Gemini API",
                status: status.as_u16(),
                message: message.to_string(),
                retry_after: asked.or_else(|| asked_wait_body(&body)),
            }
            .into());
        }
        if status.as_u16() == 404 {
            // Typed, so the caller moves to the next candidate rather than reporting a
            // retired model as a failure.
            return Err(ModelGone {
                provider: "the Gemini API",
                model: model.to_string(),
                message: message.to_string(),
            }
            .into());
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

/// The instructions the model works from, with `{name}` standing in for the recorder.
///
/// Kept as one editable string rather than assembled from fragments: this is the file
/// someone opens when the summaries are not shaped the way they want, and a prompt split
/// across a dozen `push_str` calls is a prompt nobody edits.
const SYSTEM_PROMPT: &str = "\
You summarise recordings for {name}, who was there and recorded them.

In the transcript, `{name}` is them and `Them` is everyone else.

The transcript comes from automatic speech recognition, so expect mangled names, wrong \
homophones, and missing punctuation. Read through those errors rather than quoting them.

Write for someone who was in the room and wants to remember it — not for someone who \
missed it. Leave out the greetings, the scheduling chatter, and anything that would be \
obvious to a person who was there.

## Shape

Write `summary_md` as bullet points grouped under `##` headings. Not paragraphs.

Choose two to four headings that name what this particular conversation was actually \
about. Do not reuse a fixed template. A recruiting session might want `The role` and \
`How to apply`; a coffee chat might want `About Dana` and `Advice she gave`; a project \
meeting might want `What we decided` and `Still open`. Name the heading after the thing \
discussed, so the summary is skimmable months later. Never write a heading you have \
nothing substantial to put under.

Each bullet is one fact, one line. Nest at most one level deep.

End with a `## Worth remembering` section: the small concrete details that are easy to \
forget and useful later — names and roles, companies, deadlines and dates, numbers, \
tools or links mentioned, and the personal details worth recalling next time, like where \
someone studied or what they are working on. Omit the section entirely if the transcript \
genuinely has none.

Do not write an action items heading. Action items are a separate field and are rendered \
on their own.

## Accuracy

Only record what the transcript supports. If something was discussed without being \
settled, say it was left open rather than inventing a resolution. Where the transcript is \
too garbled to interpret, leave it out instead of guessing.

Action items are commitments someone actually made, not topics that were mentioned. \
Attribute each to `you` (meaning {name}) or `them`, and use `unassigned` when nobody \
clearly took it — never guess a name.

A line reading `[mic muted]` means {name} deliberately muted their microphone. Their side \
of the conversation is missing there. Do not speculate about what was said.

`[no microphone]` means the recording was not capturing {name} at all for that stretch — \
they may have been speaking, and none of it was recorded. Do not treat it as {name} \
staying quiet, and do not speculate about what they said. `[system audio not captured]` \
is the same absence on the other side, for a second or so while the recording was \
switched over.

Use sentence case in headings. Keep it proportionate: a ten-minute call needs a handful \
of bullets, not a report.";

/// The default label for the recorder when no name has been set.
pub const DEFAULT_SPEAKER: &str = "You";

/// Resolves the name to address the recorder by, falling back to [`DEFAULT_SPEAKER`].
pub(crate) fn speaker_or_default(speaker: Option<&str>) -> &str {
    match speaker {
        Some(name) if !name.trim().is_empty() => name.trim(),
        _ => DEFAULT_SPEAKER,
    }
}

fn system_prompt(speaker: &str) -> String {
    SYSTEM_PROMPT.replace("{name}", speaker)
}

/// The user turn. Separate from the system prompt so the transcript is clearly data.
fn user_prompt(transcript: &str, speaker: &str) -> String {
    format!(
        "Here is the transcript of a recording.\n\n\
         `{speaker}` is the person who recorded it; `Them` is everyone else.\n\n\
         <transcript>\n{transcript}\n</transcript>"
    )
}

/// Why a stretch of the recording has no speech in it.
///
/// Three different absences that all look like silence in the audio and mean quite
/// different things to a reader. Collapsing them would have the transcript assert things
/// that are not true: that someone chose to go quiet when in fact nothing was recording
/// them, or that a room fell silent when in fact the tap was down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    /// The microphone was open and deliberately silenced. §5.
    Muted,
    /// No microphone was open at all — the recording was listening only for this
    /// stretch, so nothing said in the room could have reached disk. §4.
    NoMicrophone,
    /// The system tap was down while a microphone was being opened. §4. Under a second,
    /// and the only stretch where it is the *other* side that is missing.
    SystemGap,
}

impl Marker {
    /// The kind string stored in `capture_spans`, or `None` for a mute span, which has a
    /// table of its own.
    pub fn from_kind(kind: &str) -> Option<Self> {
        match kind {
            crate::db::NO_MICROPHONE => Some(Marker::NoMicrophone),
            crate::db::SYSTEM_AUDIO_GAP => Some(Marker::SystemGap),
            _ => None,
        }
    }

    fn render(self, start: i64, end: i64) -> String {
        match self {
            Marker::Muted => format!("[{}] [mic muted until {}]\n", clock(start), clock(end)),
            Marker::NoMicrophone => format!(
                "[{}] [no microphone until {}]\n",
                clock(start),
                clock(end)
            ),
            // A duration, not an end time. This gap is always well under a second, so
            // `MM:SS until MM:SS` would print the same clock twice and read as though
            // nothing had been missed at all.
            Marker::SystemGap => format!(
                "[{}] [system audio not captured for {}]\n",
                clock(start),
                brief(end - start)
            ),
        }
    }
}

/// Every marked stretch of a recording, in the order they occurred.
///
/// Takes both tables because a transcript needs all of them at once, and building the
/// list is the same three lines at every call site otherwise.
pub fn markers(
    mute_spans: &[crate::db::MuteSpan],
    capture_spans: &[crate::db::CaptureSpan],
) -> Vec<(Marker, i64, i64)> {
    let mut all: Vec<(Marker, i64, i64)> = mute_spans
        .iter()
        .map(|span| (Marker::Muted, span.start_ms, span.end_ms))
        .collect();
    all.extend(capture_spans.iter().filter_map(|span| {
        Marker::from_kind(&span.kind).map(|marker| (marker, span.start_ms, span.end_ms))
    }));
    all.sort_by_key(|(_, start, _)| *start);
    all
}

/// Renders segments and marked stretches into the text the model reads.
///
/// Timestamps are included so the summary can be traced back to the recording, and every
/// marked stretch is placed in the timeline where it happened, so a gap in the
/// conversation is never mistaken for silence — and so the reason for the gap travels
/// with it.
pub fn render_transcript(
    segments: &[Segment],
    markers: &[(Marker, i64, i64)],
    speaker: Option<&str>,
) -> String {
    let speaker_label = speaker_or_default(speaker);
    #[derive(Debug)]
    enum Line<'a> {
        Spoken(&'a Segment),
        Marked(Marker, i64, i64),
    }

    let mut lines: Vec<Line<'_>> = segments.iter().map(Line::Spoken).collect();
    lines.extend(
        markers
            .iter()
            .map(|(marker, start, end)| Line::Marked(*marker, *start, *end)),
    );
    lines.sort_by_key(|line| match line {
        Line::Spoken(segment) => segment.start_ms,
        Line::Marked(_, start, _) => *start,
    });

    let mut out = String::new();
    for line in lines {
        match line {
            Line::Spoken(segment) => {
                let speaker = if segment.channel == "mic" { speaker_label } else { "Them" };
                out.push_str(&format!(
                    "[{}] {speaker}: {}\n",
                    clock(segment.start_ms),
                    segment.text.trim()
                ));
            }
            Line::Marked(marker, start, end) => out.push_str(&marker.render(start, end)),
        }
    }
    out
}

/// A short stretch as a person would say it: "0.6s", "1.2s".
fn brief(ms: i64) -> String {
    format!("{:.1}s", ms.max(0) as f64 / 1000.0)
}

fn clock(ms: i64) -> String {
    let total = (ms.max(0) / 1000) as u64;
    format!("{:02}:{:02}", total / 60, total % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// 503 is what Gemini returns when it is out of capacity, and 529 is Anthropic's.
    /// Neither is a verdict on the request, so neither should read as a rejection.
    #[test]
    fn a_busy_provider_is_told_apart_from_a_rejected_request() {
        for status in [429, 500, 502, 503, 504, 529] {
            assert!(is_transient(status), "{status} should be worth trying again");
        }
        for status in [400, 401, 403, 404, 413, 422] {
            assert!(
                !is_transient(status),
                "{status} is a verdict on the request and must not be resent"
            );
        }
    }

    #[test]
    fn a_provider_that_is_busy_once_is_tried_again() {
        let sent = Cell::new(0u32);
        let outcome = retrying(Duration::ZERO, ignore_retries, || {
            sent.set(sent.get() + 1);
            if sent.get() == 1 {
                return Err(Busy {
                    provider: "the Gemini API",
                    status: 503,
                    message: "high demand".to_string(),
                    retry_after: None,
                }
                .into());
            }
            Ok("summary")
        });

        assert_eq!(outcome.expect("the second attempt should have landed"), "summary");
        assert_eq!(sent.get(), 2, "it should have been sent exactly twice");
    }

    /// A rejection resent is the same rejection, and the request carries the transcript —
    /// so sending it again would upload it a second time for an answer already known.
    #[test]
    fn a_rejected_request_is_not_sent_again() {
        let sent = Cell::new(0u32);
        let outcome: Result<()> = retrying(Duration::ZERO, ignore_retries, || {
            sent.set(sent.get() + 1);
            Err(anyhow!("the Gemini API rejected the request (401): bad key"))
        });

        assert!(outcome.is_err());
        assert_eq!(sent.get(), 1, "a rejection must be reported, not retried");
    }

    #[test]
    fn a_provider_that_stays_busy_is_reported_with_what_it_said() {
        let sent = Cell::new(0u32);
        let outcome: Result<()> = retrying(Duration::ZERO, ignore_retries, || {
            sent.set(sent.get() + 1);
            Err(Busy {
                provider: "the Gemini API",
                status: 503,
                message: "high demand".to_string(),
                retry_after: None,
            }
            .into())
        });

        let error = format!("{:#}", outcome.expect_err("it never succeeded"));
        assert_eq!(sent.get(), SEND_ATTEMPTS, "it should stop after SEND_ATTEMPTS");
        assert!(error.contains("503"), "the status should survive: {error}");
        assert!(error.contains("high demand"), "so should the provider's words: {error}");
        assert!(
            error.contains("untouched"),
            "and it should say nothing was lost: {error}"
        );
        assert!(
            !error.contains("  "),
            "and it should read as a sentence, not a wrapped literal: {error}"
        );
    }

    /// A provider that names its own delay knows something this code does not. Ignoring
    /// it is what had three attempts span six seconds against a spike the provider had
    /// said to come back to later.
    #[test]
    fn a_provider_that_names_a_delay_is_taken_at_its_word() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "25".parse().expect("a valid header value"));
        assert_eq!(
            asked_wait_header(&headers),
            Some(Duration::from_secs(25)),
            "the header should be read"
        );

        let body = serde_json::json!({
            "error": {
                "code": 429,
                "details": [
                    { "@type": "type.googleapis.com/google.rpc.QuotaFailure" },
                    {
                        "@type": "type.googleapis.com/google.rpc.RetryInfo",
                        "retryDelay": "17s",
                    },
                ],
            }
        });
        assert_eq!(
            asked_wait_body(&body),
            Some(Duration::from_secs(17)),
            "Google's RetryInfo should be read too"
        );
    }

    /// A provider that says nothing, or says something unusable, must leave the doubling
    /// wait in place rather than collapse it to no wait at all.
    #[test]
    fn a_provider_that_names_nothing_usable_falls_back() {
        let empty = reqwest::header::HeaderMap::new();
        assert_eq!(asked_wait_header(&empty), None);

        let mut headers = reqwest::header::HeaderMap::new();
        // The HTTP-date form, deliberately not read: it is on the provider's clock.
        headers.insert(
            "retry-after",
            "Wed, 21 Oct 2026 07:28:00 GMT".parse().expect("a valid header value"),
        );
        assert_eq!(asked_wait_header(&headers), None);

        headers.insert("retry-after", "0".parse().expect("a valid header value"));
        assert_eq!(asked_wait_header(&headers), None, "zero says nothing");

        headers.insert("retry-after", "-5".parse().expect("a valid header value"));
        assert_eq!(asked_wait_header(&headers), None, "and neither does a negative");

        assert_eq!(asked_wait_body(&serde_json::json!({ "error": {} })), None);
    }

    /// The failure that started this: Gemini sheds load on a newly released model as a
    /// 503, which no amount of retrying one name gets past. The next candidate does.
    #[test]
    fn a_model_out_of_capacity_gives_way_to_the_next() {
        let asked = std::cell::RefCell::new(Vec::new());
        let outcome = walking(
            &["gemini-3.5-flash", "gemini-3.6-flash"],
            Duration::ZERO,
            ignore_retries,
            |model| {
                asked.borrow_mut().push(model.to_string());
                if model == "gemini-3.5-flash" {
                    return Err(Busy {
                        provider: "the Gemini API",
                        status: 503,
                        message: "high demand".to_string(),
                        retry_after: None,
                    }
                    .into());
                }
                Ok("summary")
            },
        );

        let (value, model) = outcome.expect("the second model should have answered");
        assert_eq!(value, "summary");
        assert_eq!(model, "gemini-3.6-flash", "the model that answered is returned");
        assert_eq!(
            asked.borrow().len(),
            SEND_ATTEMPTS as usize + 1,
            "the first is retried to exhaustion before the second is tried at all"
        );
    }

    /// A retired name is not a capacity problem, but it is equally not a verdict on the
    /// request — so it moves on too, which is what keeps a pinned id from rotting.
    #[test]
    fn a_retired_model_gives_way_to_the_next() {
        let outcome = walking(&["gone", "here"], Duration::ZERO, ignore_retries, |model| {
            if model == "gone" {
                return Err(ModelGone {
                    provider: "the Gemini API",
                    model: model.to_string(),
                    message: "not found".to_string(),
                }
                .into());
            }
            Ok(())
        });

        assert_eq!(outcome.expect("the second model should have answered").1, "here");
    }

    /// A rejection is the provider's verdict on what was sent. Walking the whole list
    /// would upload the transcript once per model to collect the same answer.
    #[test]
    fn a_rejected_request_stops_at_the_first_model() {
        let asked = Cell::new(0u32);
        let outcome: Result<((), &str)> =
            walking(&["one", "two"], Duration::ZERO, ignore_retries, |_| {
            asked.set(asked.get() + 1);
            Err(anyhow!("the Gemini API rejected the request (401): bad key"))
        });

        assert!(outcome.is_err());
        assert_eq!(asked.get(), 1, "a bad key must not be tried against every model");
    }

    /// Told only about the last model, an out-of-capacity provider reads as one broken
    /// model — and the user goes looking for a setting to change.
    #[test]
    fn every_model_tried_is_named_when_none_answer() {
        let outcome: Result<((), &str)> = walking(
            &["gemini-3.5-flash", "gemini-flash-latest"],
            Duration::ZERO,
            ignore_retries,
            |_| {
                Err(Busy {
                    provider: "the Gemini API",
                    status: 503,
                    message: "high demand".to_string(),
                    retry_after: None,
                }
                .into())
            },
        );

        let error = format!("{:#}", outcome.expect_err("nothing answered"));
        assert!(error.contains("gemini-3.5-flash"), "{error}");
        assert!(error.contains("gemini-flash-latest"), "{error}");
    }

    /// The alias is the backstop and must stay last: it is hot-swapped to whatever Google
    /// shipped most recently, which is the model most likely to be shedding load.
    #[test]
    fn the_alias_is_the_last_resort_not_the_first_choice() {
        assert_eq!(
            GEMINI_MODELS.last(),
            Some(&"gemini-flash-latest"),
            "the alias belongs at the end of the list"
        );
        assert_eq!(
            DEFAULT_GEMINI_MODEL, GEMINI_MODELS[0],
            "the name shown should be the one tried first"
        );
        assert!(
            !GEMINI_MODELS.iter().any(|model| model.contains("preview")
                || model.contains("exp")),
            "a preview model is capacity-starved by definition and must not be a default"
        );
    }

    /// The cap exists so a provider asking for minutes cannot leave the user in front of
    /// a spinner indistinguishable from a hang.
    #[test]
    fn the_asked_wait_wins_over_the_fallback_but_not_over_the_cap() {
        let busy = |retry_after| Busy {
            provider: "the Gemini API",
            status: 503,
            message: "high demand".to_string(),
            retry_after,
        };

        assert_eq!(
            next_wait(&busy(None), Duration::from_secs(5)),
            Duration::from_secs(5),
            "a provider that said nothing leaves the doubling wait alone"
        );
        assert_eq!(
            next_wait(&busy(Some(Duration::from_secs(20))), Duration::from_secs(5)),
            Duration::from_secs(20),
            "a provider that named a delay is waited out"
        );
        assert_eq!(
            next_wait(&busy(Some(Duration::from_secs(1))), Duration::from_secs(5)),
            Duration::from_secs(1),
            "even when its figure is the shorter one"
        );
        assert_eq!(
            next_wait(&busy(Some(Duration::from_secs(600))), Duration::from_secs(5)),
            MAX_ASKED_WAIT,
            "ten minutes asked for must not become ten minutes waited"
        );
    }

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

    /// The name is not decoration: it goes into the prompt, so a summary that says "You"
    /// when a name is set means the plumbing broke.
    #[test]
    fn the_prompt_addresses_the_recorder_by_name() {
        let prompt = system_prompt("Grace");
        assert!(prompt.contains("summarise recordings for Grace"));
        assert!(!prompt.contains("{name}"), "a placeholder survived: {prompt}");
        assert!(system_prompt(DEFAULT_SPEAKER).contains("recordings for You"));
    }

    #[test]
    fn an_unset_or_blank_name_falls_back_to_you() {
        assert_eq!(speaker_or_default(None), "You");
        assert_eq!(speaker_or_default(Some("   ")), "You");
        assert_eq!(speaker_or_default(Some("  Grace ")), "Grace");
    }

    #[test]
    fn action_items_are_attributed_to_the_name() {
        let summary = Summary {
            title: "T".into(),
            summary_md: "## Notes\n\n- A thing".into(),
            action_items: vec![ActionItem {
                text: "Send the deck".into(),
                owner: "you".into(),
            }],
        };
        assert!(summary.to_markdown(Some("Grace")).contains("**Grace** — Send the deck"));
        assert!(summary.to_markdown(None).contains("**You** — Send the deck"));
    }

    #[test]
    fn the_transcript_labels_who_was_speaking() {
        let rendered = render_transcript(
            &[
                segment(1, "system", 0, "Welcome everyone"),
                segment(2, "mic", 5_000, "Thanks for having me"),
            ],
            &[],
            Some("Grace"),
        );
        assert!(rendered.contains("[00:00] Them: Welcome everyone"));
        assert!(rendered.contains("[00:05] Grace: Thanks for having me"));
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
            &[(Marker::Muted, 10_000, 65_000)],
            None,
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

    /// The three absences must not read as each other. A stretch nobody was recording is
    /// not a stretch someone chose to go quiet in, and the summary must not describe it
    /// as one.
    #[test]
    fn each_kind_of_absence_says_which_one_it_is() {
        let rendered = render_transcript(
            &[segment(1, "system", 0, "Thanks for coming")],
            &[
                (Marker::NoMicrophone, 0, 60_000),
                (Marker::SystemGap, 60_000, 60_600),
                (Marker::Muted, 120_000, 150_000),
            ],
            Some("Grace"),
        );
        assert!(rendered.contains("[00:00] [no microphone until 01:00]"), "got: {rendered}");
        assert!(
            rendered.contains("[01:00] [system audio not captured for 0.6s]"),
            "got: {rendered}"
        );
        assert!(rendered.contains("[02:00] [mic muted until 02:30]"), "got: {rendered}");
    }

    /// Markers from the two tables have to interleave by time, not sit in table order:
    /// a recording that switched mode twice has them alternating.
    #[test]
    fn markers_from_both_tables_are_ordered_by_when_they_happened() {
        let muted = vec![crate::db::MuteSpan {
            id: 1,
            event_id: 1,
            start_ms: 30_000,
            end_ms: 40_000,
        }];
        let capture = vec![
            crate::db::CaptureSpan {
                id: 1,
                event_id: 1,
                kind: crate::db::NO_MICROPHONE.into(),
                start_ms: 0,
                end_ms: 20_000,
            },
            crate::db::CaptureSpan {
                id: 2,
                event_id: 1,
                kind: crate::db::NO_MICROPHONE.into(),
                start_ms: 90_000,
                end_ms: 120_000,
            },
        ];

        let all = markers(&muted, &capture);
        assert_eq!(
            all,
            vec![
                (Marker::NoMicrophone, 0, 20_000),
                (Marker::Muted, 30_000, 40_000),
                (Marker::NoMicrophone, 90_000, 120_000),
            ]
        );
    }

    /// A kind this build does not know about is left out rather than rendered as a
    /// marker with no meaning. Nothing writes one today; a later migration could.
    #[test]
    fn an_unknown_kind_is_left_out_rather_than_guessed_at() {
        let capture = vec![crate::db::CaptureSpan {
            id: 1,
            event_id: 1,
            kind: "something_later".into(),
            start_ms: 0,
            end_ms: 10,
        }];
        assert!(markers(&[], &capture).is_empty());
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

        let markdown = summary.to_markdown(Some("Grace"));
        assert!(markdown.contains("## Action items"));
        assert!(markdown.contains("- **Grace** — Draft the rollout plan"));
        assert!(markdown.contains("- **Unassigned** — Confirm the freeze window"));
    }

    #[test]
    fn a_summary_with_no_commitments_has_no_action_items_section() {
        let summary = Summary {
            title: "Weekly sync".into(),
            summary_md: "Nothing was decided.".into(),
            action_items: vec![],
        };
        assert!(!summary.to_markdown(Some("Grace")).contains("Action items"));
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
