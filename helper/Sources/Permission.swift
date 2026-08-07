import CoreGraphics
import Foundation

/// Audio-capture permission.
///
/// A process tap that lacks permission does not fail. It is created successfully, the
/// aggregate device runs, the IO callback fires at the right rate — and every sample is
/// zero. That is the single most dangerous failure mode in this project, because it
/// looks exactly like a working recording of a quiet room.
///
/// So permission is checked in two independent ways, and neither is trusted alone:
///
///  1. **Before** capture, `preflight()` asks the system. Cheap, deterministic, and
///     catches the common case without recording anything.
///  2. **During** capture, `CaptureSession` counts non-zero samples. If audio is
///     provably playing and the tap is producing pure zeros, that is reported loudly.
///
/// Process taps are gated by the same TCC service as screen recording — on macOS 15 and
/// later it is presented as "Screen & System Audio Recording". There is no separate
/// public preflight for the audio-only half, so the screen-capture preflight is the
/// authoritative signal available to us.
enum AudioCapturePermission {

    /// True if this binary may capture system audio. Does not prompt.
    static func preflight() -> Bool {
        CGPreflightScreenCaptureAccess()
    }

    /// Asks the system to show the permission prompt. Returns the state afterwards.
    ///
    /// macOS only shows this prompt once per code identity. If the user has already
    /// answered — in either direction — this returns immediately with the stored answer
    /// and the user must change it in System Settings by hand.
    static func request() -> Bool {
        CGRequestScreenCaptureAccess()
    }

    /// What to tell a human who does not have the permission. The helper is a bare
    /// command-line binary, so the grant is attributed to whatever launched it.
    static var instructions: String {
        """
        Hearsay could not capture system audio because macOS has not granted permission.

        Open  System Settings → Privacy & Security → Screen & System Audio Recording
        and enable the app that launched this helper (Hearsay, or your terminal if you
        are running it by hand). You may need to quit and reopen that app afterwards.

        No virtual audio device or output-routing change will fix this — the permission
        is the only thing standing in the way.
        """
    }
}
