//! Question-answering tests that make a real API call with the user's own key.
//!
//! `#[ignore]`d because they need a key in the Keychain and cost a request against the
//! user's account. They exist because everything else about this path can pass while the
//! path itself does not work: the request shape, the token ceiling, and the response
//! parsing are only ever exercised by a real call.
//!
//! The transcript is fabricated on purpose. It verifies the wire format without sending a
//! real recording anywhere, and it has known answers so a wrong one is visible.
//!
//! ```sh
//! cargo test -p hearsay-core --test live_chat -- --ignored --nocapture
//! ```

use hearsay_core::chat;
use hearsay_core::db::Segment;
use hearsay_core::summary::Provider;

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

/// A short exchange with facts worth retrieving: a name, a number, and a date.
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
        segment(5, "mic", 22_000, "Good to know. I'll get mine in before then."),
    ]
}

#[test]
#[ignore = "needs an API key in the Keychain and spends a real request"]
fn a_question_is_answered_from_the_transcript() {
    let provider = Provider::current();
    let model = provider.default_model();
    println!("asking {} ({model})", provider.as_str());

    let answer = chat::ask(
        &transcript(),
        &[],
        &[],
        "How many engineers are on the team, and when do applications close?",
        model,
        Some("Anikait"),
    )
    .expect("the question should be answered");

    println!("--- answer ---\n{answer}\n--------------");

    assert!(!answer.trim().is_empty(), "the answer was empty");
    assert!(
        answer.contains("fourteen") || answer.contains("14"),
        "the answer missed the team size, which the transcript states plainly:\n{answer}"
    );
    assert!(
        answer.to_lowercase().contains("march"),
        "the answer missed the deadline, which the transcript states plainly:\n{answer}"
    );
}

/// The rule the prompt spends most of its words on. An answer invented to be helpful is
/// the failure mode that makes the whole feature untrustworthy.
#[test]
#[ignore = "needs an API key in the Keychain and spends a real request"]
fn a_question_the_transcript_does_not_answer_is_declined() {
    let model = Provider::current().default_model();

    let answer = chat::ask(
        &transcript(),
        &[],
        &[],
        "What is Dana's salary, and what city does she live in?",
        model,
        Some("Anikait"),
    )
    .expect("the question should get a response");

    println!("--- answer ---\n{answer}\n--------------");

    let lowered = answer.to_lowercase();
    let admits = ["not", "no ", "doesn't", "does not", "never", "isn't"]
        .iter()
        .any(|marker| lowered.contains(marker));
    assert!(
        admits,
        "the model should say the transcript does not cover this rather than answering:\n{answer}"
    );
}

/// Multi-turn: the second question only makes sense if the first exchange was carried.
#[test]
#[ignore = "needs an API key in the Keychain and spends a real request"]
fn earlier_turns_are_carried_into_later_questions() {
    let model = Provider::current().default_model();

    let history = vec![
        chat::Turn::user("Who was I speaking to?"),
        chat::Turn::assistant("Dana Whitfield, who leads the platform team [00:00]."),
    ];

    let answer = chat::ask(
        &transcript(),
        &[],
        &history,
        "What team did she say she leads?",
        model,
        Some("Anikait"),
    )
    .expect("the follow-up should be answered");

    println!("--- answer ---\n{answer}\n--------------");
    assert!(
        answer.to_lowercase().contains("platform"),
        "the follow-up lost the thread:\n{answer}"
    );
}
