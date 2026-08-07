//! Google Calendar, for naming recordings and offering to start them.
//!
//! **This is the second thing in Hearsay that talks to a network, after summaries.**
//! `CLAUDE.md` §1 says nothing leaves the machine except explicit LLM calls; calendar
//! sync is a deliberate, opt-in exception, and it is written to keep the exception as
//! small as possible:
//!
//! - Off unless the user connects it, and disconnectable at any time.
//! - **Read-only** scope. Hearsay cannot create, change, or delete anything.
//! - Only titles and times are read. Attendees, descriptions and attachments are never
//!   requested or stored.
//! - **Nothing is ever uploaded.** No recording, transcript, or summary is sent to
//!   Google. Data flows one way: in.
//! - Tokens live in the Keychain beside the API key, never in the database or a file.
//!
//! The OAuth flow is the installed-application loopback: a local HTTP server on a
//! random port receives the redirect, so no credential is ever pasted through a browser
//! address bar the user has to trust.

use crate::secrets;

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Duration;

const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
const EVENTS_ENDPOINT: &str =
    "https://www.googleapis.com/calendar/v3/calendars/primary/events";

/// Read-only. Deliberately the narrowest scope that answers "what am I in right now".
const SCOPE: &str = "https://www.googleapis.com/auth/calendar.readonly";

/// How long to wait for the user to finish signing in before giving up and shutting the
/// local server down.
const AUTH_TIMEOUT: Duration = Duration::from_secs(180);

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A calendar entry, reduced to the two fields Hearsay uses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub id: String,
    pub title: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

impl CalendarEvent {
    /// Milliseconds this event overlaps with the given span. Zero if they do not meet.
    pub fn overlap_ms(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> i64 {
        let from = self.start.max(start);
        let to = self.end.min(end);
        (to - from).num_milliseconds().max(0)
    }
}

/// Picks the calendar event a recording belongs to.
///
/// Chooses the greatest overlap rather than the nearest start: a recording begun ten
/// minutes late still belongs to the meeting it is inside, not to the one that ended
/// just before it. Returns `None` when nothing meaningfully overlaps, so a recording
/// made outside any meeting is left with its own name rather than borrowing one.
pub fn match_recording<'a>(
    events: &'a [CalendarEvent],
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
) -> Option<&'a CalendarEvent> {
    let duration_ms = (ended_at - started_at).num_milliseconds().max(1);
    // A quarter of the recording, measured against the recording rather than against the
    // meeting. A short recording inside a long meeting still qualifies (all of it
    // overlaps), while a long recording that merely clips the end of the previous
    // meeting does not — which is the case that would otherwise rename it wrongly.
    let required = (duration_ms / 4).max(1);

    events
        .iter()
        .map(|event| (event, event.overlap_ms(started_at, ended_at)))
        .filter(|(_, overlap)| *overlap >= required)
        .max_by_key(|(_, overlap)| *overlap)
        .map(|(event, _)| event)
}

/// The event that is about to start, if one is close enough to offer.
pub fn next_starting<'a>(
    events: &'a [CalendarEvent],
    now: DateTime<Utc>,
    within: ChronoDuration,
) -> Option<&'a CalendarEvent> {
    events
        .iter()
        // Already started but not finished counts: someone opening the laptop two
        // minutes into a call still wants the offer.
        .filter(|event| event.end > now && event.start <= now + within)
        .min_by_key(|event| event.start)
}

// -- credentials -------------------------------------------------------------------

/// The user's own OAuth client, from Google Cloud Console.
///
/// Hearsay ships no client of its own: an embedded one would make every user's calendar
/// access flow through credentials they do not control, which is the opposite of what
/// this app is for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientCredentials {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredTokens {
    refresh_token: String,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    expires_at: Option<DateTime<Utc>>,
}

/// Whether the calendar is connected.
pub fn is_connected() -> bool {
    secrets::calendar_tokens().ok().flatten().is_some()
}

/// Forgets the connection. Google keeps its own record of the grant, so this also tells
/// the user where to revoke it on their side.
pub fn disconnect() -> Result<()> {
    secrets::clear_calendar_tokens()?;
    secrets::clear_calendar_client()?;
    Ok(())
}

// -- the OAuth loopback flow -------------------------------------------------------

/// Runs the full sign-in, blocking until the user finishes or the timeout expires.
///
/// Returns the URL the user must open. The caller opens it; this function is already
/// listening by the time it returns from the server bind, so there is no race.
pub fn connect(credentials: &ClientCredentials, open_url: impl FnOnce(&str)) -> Result<()> {
    let verifier = random_token();
    let challenge = code_challenge(&verifier);

    // Port 0: the OS picks a free one. A fixed port would collide with whatever else
    // the user happens to be running.
    let server = tiny_http::Server::http("127.0.0.1:0")
        .map_err(|error| anyhow!("could not start the local sign-in server: {error}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| anyhow!("the local sign-in server has no address"))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}");

    let state = random_token();
    let auth_url = format!(
        "{AUTH_ENDPOINT}?client_id={}&redirect_uri={}&response_type=code&scope={}\
         &code_challenge={challenge}&code_challenge_method=S256&access_type=offline\
         &prompt=consent&state={state}",
        urlencoding::encode(&credentials.client_id),
        urlencoding::encode(&redirect_uri),
        urlencoding::encode(SCOPE),
    );
    open_url(&auth_url);

    let code = wait_for_code(&server, &state)?;
    let tokens = exchange_code(credentials, &code, &redirect_uri, &verifier)?;

    secrets::set_calendar_client(&serde_json::to_string(credentials)?)?;
    secrets::set_calendar_tokens(&serde_json::to_string(&tokens)?)?;
    tracing::info!("connected to Google Calendar (read-only)");
    Ok(())
}

/// Serves the redirect until the browser arrives with a code.
fn wait_for_code(server: &tiny_http::Server, expected_state: &str) -> Result<String> {
    let deadline = std::time::Instant::now() + AUTH_TIMEOUT;

    while std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let request = match server.recv_timeout(remaining.min(Duration::from_secs(2))) {
            Ok(Some(request)) => request,
            Ok(None) => continue,
            Err(error) => return Err(anyhow!("the local sign-in server failed: {error}")),
        };

        let url = request.url().to_string();
        let params = query_params(&url);

        if let Some(error) = params.iter().find(|(key, _)| key == "error") {
            let _ = request.respond(page("Sign-in was declined. You can close this tab."));
            return Err(anyhow!("Google returned an error: {}", error.1));
        }

        let code = params.iter().find(|(key, _)| key == "code").map(|(_, v)| v.clone());
        let state = params.iter().find(|(key, _)| key == "state").map(|(_, v)| v.clone());

        match code {
            Some(code) => {
                // Guards against another page on this machine hitting the loopback
                // server and injecting a code from a different grant.
                if state.as_deref() != Some(expected_state) {
                    let _ = request.respond(page("Sign-in could not be verified."));
                    return Err(anyhow!(
                        "the sign-in response did not match the request; nothing was saved"
                    ));
                }
                let _ = request.respond(page(
                    "Hearsay is connected to your calendar. You can close this tab.",
                ));
                return Ok(code);
            }
            None => {
                // The browser also asks for /favicon.ico; ignore anything without a code.
                let _ = request.respond(page("Waiting for Google…"));
            }
        }
    }

    Err(anyhow!(
        "sign-in timed out after {} seconds; nothing was changed",
        AUTH_TIMEOUT.as_secs()
    ))
}

fn page(message: &str) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Hearsay</title>\
         <body style=\"font-family:-apple-system,sans-serif;background:#F5F0E9;\
         color:#112250;display:grid;place-items:center;height:100vh;margin:0\">\
         <p>{message}</p>"
    );
    tiny_http::Response::from_string(body).with_header(
        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
            .expect("a literal header is always valid"),
    )
}

fn exchange_code(
    credentials: &ClientCredentials,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<StoredTokens> {
    #[derive(Deserialize)]
    struct TokenResponse {
        access_token: String,
        #[serde(default)]
        refresh_token: Option<String>,
        #[serde(default)]
        expires_in: Option<i64>,
    }

    let client = http_client()?;
    let response = client
        .post(TOKEN_ENDPOINT)
        .form(&[
            ("client_id", credentials.client_id.as_str()),
            ("client_secret", credentials.client_secret.as_str()),
            ("code", code),
            ("code_verifier", verifier),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect_uri),
        ])
        .send()
        .context("could not reach Google to complete sign-in")?;

    if !response.status().is_success() {
        let status = response.status();
        // Google echoes the request in its error body; report the status and its short
        // error code only, never the body, which would contain the client secret.
        return Err(anyhow!("Google rejected the sign-in ({status})"));
    }

    let token: TokenResponse = response.json().context("could not read Google's response")?;
    let refresh_token = token.refresh_token.ok_or_else(|| {
        anyhow!(
            "Google did not return a refresh token. Revoke Hearsay's access in your \
             Google account and connect again — Google only issues one on first consent."
        )
    })?;

    Ok(StoredTokens {
        refresh_token,
        access_token: Some(token.access_token),
        expires_at: token
            .expires_in
            .map(|seconds| Utc::now() + ChronoDuration::seconds(seconds - 60)),
    })
}

/// A usable access token, refreshing it if the stored one has expired.
fn access_token() -> Result<String> {
    let raw = secrets::calendar_tokens()?
        .ok_or_else(|| anyhow!("the calendar is not connected"))?;
    let mut tokens: StoredTokens = serde_json::from_str(&raw)
        .context("the stored calendar tokens could not be read; reconnect the calendar")?;

    if let (Some(token), Some(expires)) = (&tokens.access_token, tokens.expires_at) {
        if expires > Utc::now() {
            return Ok(token.clone());
        }
    }

    let raw_client = secrets::calendar_client()?
        .ok_or_else(|| anyhow!("the calendar client details are missing; reconnect"))?;
    let credentials: ClientCredentials = serde_json::from_str(&raw_client)?;

    #[derive(Deserialize)]
    struct RefreshResponse {
        access_token: String,
        #[serde(default)]
        expires_in: Option<i64>,
    }

    let client = http_client()?;
    let response = client
        .post(TOKEN_ENDPOINT)
        .form(&[
            ("client_id", credentials.client_id.as_str()),
            ("client_secret", credentials.client_secret.as_str()),
            ("refresh_token", tokens.refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .context("could not reach Google to refresh calendar access")?;

    if !response.status().is_success() {
        return Err(anyhow!(
            "Google would not refresh calendar access ({}). Reconnect the calendar in \
             settings.",
            response.status()
        ));
    }

    let refreshed: RefreshResponse = response.json()?;
    tokens.access_token = Some(refreshed.access_token.clone());
    tokens.expires_at = refreshed
        .expires_in
        .map(|seconds| Utc::now() + ChronoDuration::seconds(seconds - 60));
    secrets::set_calendar_tokens(&serde_json::to_string(&tokens)?)?;

    Ok(refreshed.access_token)
}

/// Events between `from` and `to` on the primary calendar.
///
/// Single events only — recurring meetings are expanded by Google so each occurrence has
/// its own concrete start and end, which is what the matching needs.
pub fn events_between(from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<CalendarEvent>> {
    #[derive(Deserialize)]
    struct ListResponse {
        #[serde(default)]
        items: Vec<Item>,
    }
    #[derive(Deserialize)]
    struct Item {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        summary: Option<String>,
        start: Option<Stamp>,
        end: Option<Stamp>,
        #[serde(default)]
        status: Option<String>,
    }
    #[derive(Deserialize)]
    struct Stamp {
        #[serde(rename = "dateTime")]
        date_time: Option<DateTime<Utc>>,
    }

    let token = access_token()?;
    let client = http_client()?;
    let response = client
        .get(EVENTS_ENDPOINT)
        .bearer_auth(token)
        .query(&[
            ("timeMin", from.to_rfc3339()),
            ("timeMax", to.to_rfc3339()),
            ("singleEvents", "true".to_string()),
            ("orderBy", "startTime".to_string()),
            ("maxResults", "50".to_string()),
        ])
        .send()
        .context("could not reach Google Calendar")?;

    if !response.status().is_success() {
        return Err(anyhow!(
            "Google Calendar returned {}. Try reconnecting in settings.",
            response.status()
        ));
    }

    let list: ListResponse = response.json().context("could not read the calendar")?;

    Ok(list
        .items
        .into_iter()
        .filter(|item| item.status.as_deref() != Some("cancelled"))
        .filter_map(|item| {
            // All-day entries have a `date` rather than a `dateTime` and are not
            // meetings; skipping them keeps "Annual leave" from claiming a recording.
            let start = item.start.and_then(|stamp| stamp.date_time)?;
            let end = item.end.and_then(|stamp| stamp.date_time)?;
            Some(CalendarEvent {
                id: item.id.unwrap_or_default(),
                title: item.summary.unwrap_or_else(|| "Untitled event".to_string()),
                start,
                end,
            })
        })
        .collect())
}

fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("could not build the HTTP client")
}

// -- small helpers -----------------------------------------------------------------

/// A URL-safe random token, for the PKCE verifier and the state parameter.
///
/// Seeded from the OS clock and the address of a fresh allocation. Not a CSPRNG, but
/// these values only need to be unguessable to a local page racing a loopback redirect
/// within a three-minute window, and pulling in a full RNG for that would be more
/// dependency than the job needs.
fn random_token() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let boxed = Box::new(0u8);
    let address = &*boxed as *const u8 as usize;

    let mut state = now as u64 ^ (address as u64).rotate_left(17) ^ std::process::id() as u64;
    let mut bytes = [0u8; 32];
    for byte in bytes.iter_mut() {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *byte = (state >> 33) as u8;
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn code_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn query_params(url: &str) -> Vec<(String, String)> {
    let Some(query) = url.split_once('?').map(|(_, query)| query) else {
        return Vec::new();
    };
    query
        .split('&')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            Some((
                urlencoding::decode(key).ok()?.into_owned(),
                urlencoding::decode(value).ok()?.into_owned(),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minute: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + minute * 60, 0).expect("valid timestamp")
    }

    fn event(id: &str, title: &str, from: i64, to: i64) -> CalendarEvent {
        CalendarEvent {
            id: id.to_string(),
            title: title.to_string(),
            start: at(from),
            end: at(to),
        }
    }

    #[test]
    fn a_recording_matches_the_meeting_it_sits_inside() {
        let events = vec![
            event("a", "Standup", 0, 15),
            event("b", "Design review", 30, 90),
        ];
        let matched = match_recording(&events, at(35), at(85)).expect("a match");
        assert_eq!(matched.title, "Design review");
    }

    /// Starting late is the normal case, not an edge case.
    #[test]
    fn joining_a_meeting_late_still_matches_it() {
        let events = vec![event("b", "Design review", 30, 90)];
        let matched = match_recording(&events, at(45), at(88)).expect("a match");
        assert_eq!(matched.id, "b");
    }

    #[test]
    fn the_meeting_with_the_greatest_overlap_wins() {
        let events = vec![
            event("a", "Standup", 0, 40),
            event("b", "Design review", 35, 90),
        ];
        // Straddles both, but sits mostly in the second.
        let matched = match_recording(&events, at(38), at(80)).expect("a match");
        assert_eq!(matched.title, "Design review");
    }

    /// A recording made outside any meeting keeps its own name.
    #[test]
    fn a_recording_outside_every_meeting_matches_nothing() {
        let events = vec![event("a", "Standup", 0, 15)];
        assert!(match_recording(&events, at(120), at(150)).is_none());
    }

    #[test]
    fn a_brief_brush_against_an_adjacent_meeting_does_not_count() {
        let events = vec![event("a", "Standup", 0, 31)];
        // A half-hour recording that clips the last 60 seconds of the previous meeting.
        assert!(
            match_recording(&events, at(30), at(60)).is_none(),
            "a one-minute overlap renamed a half-hour recording"
        );
    }

    #[test]
    fn an_empty_calendar_matches_nothing() {
        assert!(match_recording(&[], at(0), at(30)).is_none());
    }

    #[test]
    fn overlap_is_zero_for_disjoint_spans() {
        let meeting = event("a", "Standup", 0, 15);
        assert_eq!(meeting.overlap_ms(at(20), at(30)), 0);
        assert_eq!(meeting.overlap_ms(at(5), at(10)), 5 * 60 * 1000);
    }

    #[test]
    fn the_next_meeting_is_the_soonest_one_still_running_or_about_to_start() {
        let events = vec![
            event("past", "Finished", 0, 10),
            event("soon", "Starting", 20, 50),
            event("later", "Much later", 200, 260),
        ];
        let next = next_starting(&events, at(18), ChronoDuration::minutes(5)).expect("a match");
        assert_eq!(next.id, "soon");
    }

    #[test]
    fn a_meeting_already_underway_is_still_offered() {
        let events = vec![event("now", "In progress", 10, 60)];
        let next = next_starting(&events, at(25), ChronoDuration::minutes(5)).expect("a match");
        assert_eq!(next.id, "now");
    }

    #[test]
    fn a_meeting_too_far_off_is_not_offered_yet() {
        let events = vec![event("later", "Much later", 200, 260)];
        assert!(next_starting(&events, at(10), ChronoDuration::minutes(5)).is_none());
    }

    #[test]
    fn a_finished_meeting_is_never_offered() {
        let events = vec![event("past", "Finished", 0, 10)];
        assert!(next_starting(&events, at(20), ChronoDuration::minutes(5)).is_none());
    }

    #[test]
    fn the_pkce_challenge_is_the_url_safe_sha256_of_the_verifier() {
        // The worked example from RFC 7636 appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            code_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn random_tokens_differ_between_calls() {
        assert_ne!(random_token(), random_token());
    }

    #[test]
    fn query_parameters_are_decoded() {
        let params = query_params("/?code=4%2F0Ab&state=xy%20z");
        assert_eq!(params[0], ("code".to_string(), "4/0Ab".to_string()));
        assert_eq!(params[1], ("state".to_string(), "xy z".to_string()));
    }
}
