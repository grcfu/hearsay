//! Summary tests that make a real API call with the user's own key.
//!
//! `#[ignore]`d for the reasons `live_chat.rs` gives: they need a key in the Keychain and
//! cost requests against the user's account. The chat tests do not cover this path — a
//! summary sends a response schema and a thinking hint that a question does not, so a
//! model can answer every question and still refuse every summary.
//!
//! The transcript is fabricated, so nothing from a real recording is sent anywhere.
//!
//! ```sh
//! cargo test -p hearsay-core --test live_summary -- --ignored --nocapture --test-threads=1
//! ```

use hearsay_core::db::Segment;
use hearsay_core::summary::{self, Provider};
use std::time::Instant;

fn segment(id: i64, channel: &str, start_ms: i64, text: &str) -> Segment {
    Segment {
        id,
        event_id: 1,
        channel: channel.to_string(),
        start_ms,
        end_ms: start_ms + 2_000,
        text: text.to_string(),
    }
}

fn transcript() -> Vec<Segment> {
    vec![
        segment(1, "system", 0, "Hi, I'm Dana Whitfield, I lead the platform team."),
        segment(2, "mic", 4_000, "Thanks for making the time. How big is the team now?"),
        segment(
            3,
            "system",
            8_000,
            "We're fourteen engineers, hoping to be twenty by the end of the year.",
        ),
        segment(
            4,
            "system",
            14_000,
            "Applications close on the third of March, and we interview in April.",
        ),
        segment(5, "mic", 22_000, "Good to know. I'll send my application this week."),
        segment(6, "system", 28_000, "Great. I'll forward you the job description tonight."),
    ]
}

/// Every candidate, asked on its own. A walk that succeeds says nothing about the models
/// before the one that answered, and a walk that fails reports only the last of them.
///
/// `HEARSAY_PROBE_MODELS=a,b,c` probes those names instead of the shipped list, which is
/// how a replacement is checked before it goes into it.
#[test]
#[ignore = "needs an API key in the Keychain and spends one request per model"]
fn each_candidate_summarizes_on_its_own() {
    let probe: Vec<&'static str> = match std::env::var("HEARSAY_PROBE_MODELS") {
        Ok(names) => names
            .split(',')
            .map(|name| &*Box::leak(name.trim().to_string().into_boxed_str()))
            .collect(),
        Err(_) => Provider::current().models().to_vec(),
    };
    let mut failed = Vec::new();
    for model in &probe {
        let started = Instant::now();
        let result =
            summary::summarize(&transcript(), &[], &[*model], None, |notice| {
                println!(
                    "  {model}: attempt {} of {} busy, waiting {}s: {}",
                    notice.attempt,
                    notice.of,
                    notice.waiting.as_secs(),
                    notice.busy
                )
            });
        let elapsed = started.elapsed().as_secs_f32();
        match result {
            Ok(done) => println!(
                "{model}: answered in {elapsed:.1}s, {} action items",
                done.summary.action_items.len()
            ),
            Err(e) => {
                println!("{model}: FAILED after {elapsed:.1}s: {e:#}");
                failed.push(*model);
            }
        }
    }
    assert!(failed.is_empty(), "these candidates did not summarize: {failed:?}");
}

/// The walk the app actually runs.
#[test]
#[ignore = "needs an API key in the Keychain and spends a real request"]
fn a_transcript_is_summarized() {
    let provider = Provider::current();
    let started = Instant::now();
    let done = summary::summarize(
        &transcript(),
        &[],
        provider.models(),
        None,
        summary::ignore_retries,
    )
    .expect("the summary should arrive");
    println!(
        "--- {} in {:.1}s ---\n{}",
        done.model,
        started.elapsed().as_secs_f32(),
        done.summary.to_markdown(None)
    );
    assert!(done.summary.to_markdown(None).contains("##"), "the body has headings");
}

/// An hour of fabricated meeting, at the segment count a real one reaches. The short
/// transcript above proves the wire format; this is what proves a real recording's
/// length does not tip a model into a timeout or a truncated answer.
fn long_transcript() -> Vec<Segment> {
    const LINES: &[(&str, &str)] = &[
        ("system", "So the next thing on the roadmap is the migration off the old queue."),
        ("mic", "How long do you expect that to take?"),
        ("system", "Probably six weeks, if the storage team signs off on the schema."),
        ("system", "Priya owns the rollout plan and she's presenting it on Thursday."),
        ("mic", "Is there anything blocking her right now?"),
        ("system", "Mostly the load tests. We need a staging cluster that looks like prod."),
        ("mic", "I can ask infra whether the spare cluster is free next week."),
        ("system", "That would help a lot. The budget for this quarter is about forty thousand."),
        ("system", "We also said we'd revisit the on-call rotation before the holidays."),
        ("mic", "Right, I think we agreed to move to week-long shifts."),
        ("system", "Yes, and Marco wanted a written handover note at the end of each one."),
        ("mic", "Makes sense. Who's drafting the template?"),
        ("system", "Nobody yet. Let's leave that open and come back to it."),
        ("system", "On hiring, we have two offers out and one more final round on Monday."),
        ("mic", "Are those both for the backend role?"),
        ("system", "One backend, one data engineer. The data one closes on the twentieth."),
    ];
    (0..700)
        .map(|i| {
            let (channel, text) = LINES[i % LINES.len()];
            segment(i as i64 + 1, channel, i as i64 * 5_000, text)
        })
        .collect()
}

#[test]
#[ignore = "needs an API key in the Keychain and spends one request per model"]
fn each_candidate_summarizes_an_hour() {
    let probe: Vec<&'static str> = match std::env::var("HEARSAY_PROBE_MODELS") {
        Ok(names) => names
            .split(',')
            .map(|name| &*Box::leak(name.trim().to_string().into_boxed_str()))
            .collect(),
        Err(_) => Provider::current().models().to_vec(),
    };
    let mut failed = Vec::new();
    for model in &probe {
        let started = Instant::now();
        let result =
            summary::summarize(&long_transcript(), &[], &[*model], None, |notice| {
                println!(
                    "  {model}: attempt {} of {} busy, waiting {}s: {}",
                    notice.attempt,
                    notice.of,
                    notice.waiting.as_secs(),
                    notice.busy
                )
            });
        let elapsed = started.elapsed().as_secs_f32();
        match result {
            Ok(done) => println!(
                "{model}: answered in {elapsed:.1}s, {} chars, {} action items",
                done.summary.to_markdown(None).len(),
                done.summary.action_items.len()
            ),
            Err(e) => {
                println!("{model}: FAILED after {elapsed:.1}s: {e:#}");
                failed.push(*model);
            }
        }
    }
    assert!(failed.is_empty(), "these candidates did not summarize: {failed:?}");
}
