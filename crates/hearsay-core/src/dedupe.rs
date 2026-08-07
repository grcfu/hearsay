//! Removing echoed speech from the transcript.
//!
//! The second half of the echo strategy. Detection warns the user; this cleans up what
//! already leaked. When the far end's voice bleeds into the microphone loudly enough,
//! the recogniser transcribes it on the mic channel too, and the transcript ends up
//! attributing the other party's words to the user.
//!
//! The signal is unambiguous: the same sentence, on both channels, within a few seconds.
//! Genuine agreement between two people does not reproduce a sentence word for word, so
//! a high similarity threshold removes echoes without removing conversation.
//!
//! The **microphone** copy is always the one dropped. The system channel is the direct
//! recording; the mic copy is the one that travelled through the air.

use crate::transcribe::TranscriptSegment;

/// How far apart an echo and its original can be and still be the same utterance.
const MAX_GAP_MS: i64 = 3_000;

/// Similarity above which two segments are the same sentence.
///
/// High on purpose. Both channels went through the same recogniser on different-quality
/// audio, so an echo is close but rarely identical — while two people saying "yeah,
/// exactly" score far below this. Dropping a line the user really said is much worse
/// than leaving an echo in, so the threshold errs toward keeping.
const SIMILARITY_THRESHOLD: f64 = 0.82;

/// Text too short to judge. "Yeah" matches "yeah" perfectly and means nothing.
const MIN_COMPARABLE_CHARS: usize = 12;

/// Drops microphone segments that are echoes of system-channel segments.
///
/// Returns the surviving segments and how many were removed.
pub fn drop_echoed_segments(
    segments: Vec<TranscriptSegment>,
) -> (Vec<TranscriptSegment>, usize) {
    let system: Vec<(i64, String)> = segments
        .iter()
        .filter(|segment| segment.channel == "system")
        .map(|segment| (segment.start_ms, normalise(&segment.text)))
        .collect();

    if system.is_empty() {
        return (segments, 0);
    }

    let mut dropped = 0;
    let kept: Vec<TranscriptSegment> = segments
        .into_iter()
        .filter(|segment| {
            if segment.channel != "mic" {
                return true;
            }
            let candidate = normalise(&segment.text);
            if candidate.chars().count() < MIN_COMPARABLE_CHARS {
                return true;
            }

            let echoed = system.iter().any(|(start_ms, text)| {
                (segment.start_ms - start_ms).abs() <= MAX_GAP_MS
                    && similarity(&candidate, text) >= SIMILARITY_THRESHOLD
            });

            if echoed {
                dropped += 1;
                tracing::debug!("dropped echoed mic segment: {:?}", segment.text);
            }
            !echoed
        })
        .collect();

    (kept, dropped)
}

/// Lowercases, strips punctuation, and collapses whitespace.
///
/// The two channels are transcribed separately, so the same sentence routinely comes
/// back with different punctuation and casing. Comparing raw text would miss most real
/// echoes.
fn normalise(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_was_space = true;

    for character in text.chars() {
        if character.is_alphanumeric() {
            out.extend(character.to_lowercase());
            last_was_space = false;
        } else if !last_was_space {
            out.push(' ');
            last_was_space = true;
        }
    }
    out.trim_end().to_string()
}

/// Normalised edit-distance similarity, 1.0 for identical and 0.0 for nothing in common.
pub fn similarity(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    let longest = a.chars().count().max(b.chars().count());
    if longest == 0 {
        return 1.0;
    }
    1.0 - (levenshtein(a, b) as f64 / longest as f64)
}

/// Edit distance, two rows at a time.
///
/// Segments are single utterances — tens of characters — so the quadratic cost is
/// irrelevant, and the full matrix would be the only thing worth avoiding.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }

    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];

    for (i, ca) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            current[j + 1] = (current[j] + 1)
                .min(previous[j + 1] + 1)
                .min(previous[j] + cost);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(channel: &str, start_ms: i64, text: &str) -> TranscriptSegment {
        TranscriptSegment {
            start_ms,
            end_ms: start_ms + 2_000,
            text: text.to_string(),
            channel: channel.to_string(),
        }
    }

    #[test]
    fn an_echo_of_the_other_party_is_dropped_from_the_mic_channel() {
        let segments = vec![
            segment("system", 1_000, "We need to finalise the migration timeline."),
            // The same sentence, through the speakers and back into the mic.
            segment("mic", 1_400, "we need to finalize the migration timeline"),
        ];

        let (kept, dropped) = drop_echoed_segments(segments);
        assert_eq!(dropped, 1);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].channel, "system", "the wrong copy was dropped");
    }

    /// The failure that would matter: removing something the user actually said.
    #[test]
    fn the_users_own_words_are_never_dropped() {
        let segments = vec![
            segment("system", 1_000, "We need to finalise the migration timeline."),
            segment("mic", 1_500, "I can have the rollout plan ready by Thursday."),
        ];

        let (kept, dropped) = drop_echoed_segments(segments);
        assert_eq!(dropped, 0);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn a_matching_line_far_apart_in_time_is_a_repetition_not_an_echo() {
        let segments = vec![
            segment("system", 1_000, "We need to finalise the migration timeline."),
            // Ten seconds later — the user genuinely repeating the point back.
            segment("mic", 11_000, "We need to finalise the migration timeline."),
        ];

        let (kept, dropped) = drop_echoed_segments(segments);
        assert_eq!(dropped, 0, "a repetition ten seconds later is not an echo");
        assert_eq!(kept.len(), 2);
    }

    /// Short interjections match each other trivially; dropping them would silently
    /// erase half of a normal conversation.
    #[test]
    fn short_agreements_are_kept_even_when_identical() {
        let segments = vec![
            segment("system", 1_000, "Yeah."),
            segment("mic", 1_200, "Yeah."),
        ];

        let (kept, dropped) = drop_echoed_segments(segments);
        assert_eq!(dropped, 0);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn recognition_differences_between_channels_still_count_as_an_echo() {
        let segments = vec![
            segment("system", 0, "Let's push the launch to the seventeenth of November."),
            // Same sentence, worse audio: a couple of words come back mangled.
            segment("mic", 900, "lets push the launch to the seventeenth of november"),
        ];

        let (_, dropped) = drop_echoed_segments(segments);
        assert_eq!(dropped, 1);
    }

    #[test]
    fn a_listen_only_transcript_is_returned_untouched() {
        let segments = vec![
            segment("system", 0, "Welcome everyone, thanks for joining today."),
            segment("system", 3_000, "Let us start with the roadmap review."),
        ];
        let (kept, dropped) = drop_echoed_segments(segments.clone());
        assert_eq!(dropped, 0);
        assert_eq!(kept.len(), segments.len());
    }

    #[test]
    fn a_mic_only_transcript_is_returned_untouched() {
        let segments = vec![segment("mic", 0, "Testing this recording on my own.")];
        let (kept, dropped) = drop_echoed_segments(segments);
        assert_eq!(dropped, 0);
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn normalisation_ignores_case_and_punctuation() {
        assert_eq!(normalise("Hello, World!"), "hello world");
        assert_eq!(normalise("  spaced   out  "), "spaced out");
    }

    #[test]
    fn similarity_is_one_for_identical_and_low_for_unrelated() {
        assert_eq!(similarity("hello world", "hello world"), 1.0);
        assert!(similarity("hello world", "hello word") > 0.8);
        assert!(similarity("hello world", "completely different") < 0.5);
    }
}
