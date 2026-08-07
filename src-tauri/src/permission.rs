//! Asking macOS about system-audio permission, from inside the app process.
//!
//! This has to run **in the app**, not in the helper subprocess. TCC answers on behalf
//! of the *responsible* process, so when Hearsay spawns the helper and the helper asks,
//! the answer describes whatever launched Hearsay rather than Hearsay itself. Shelling
//! out for the answer produced a permanent "not granted" banner on a machine where the
//! permission was, in fact, granted.
//!
//! Process taps are gated by the same TCC service as screen recording, presented on
//! macOS 15+ as "Screen & System Audio Recording". There is no public preflight for the
//! audio-only half, so this is the authoritative signal available to us — and, as the
//! banner text is careful to say, it is a signal rather than proof. The proof is whether
//! real samples arrive, which `hearsay-audio` checks independently.

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

/// Whether this process may capture system audio. Does not prompt.
pub fn granted() -> bool {
    #[cfg(target_os = "macos")]
    // Safety: both are simple C predicates from CoreGraphics taking no arguments and
    // returning a bool. They are safe to call from any thread.
    unsafe {
        CGPreflightScreenCaptureAccess()
    }
    #[cfg(not(target_os = "macos"))]
    false
}

/// Asks macOS to show the permission prompt, and reports the state afterwards.
///
/// macOS shows this once per code identity. If the user has already answered — either
/// way — this returns the stored answer immediately and the only way to change it is
/// System Settings.
pub fn request() -> bool {
    #[cfg(target_os = "macos")]
    unsafe {
        CGRequestScreenCaptureAccess()
    }
    #[cfg(not(target_os = "macos"))]
    false
}
