import Foundation

/// hearsay-audio-helper
///
/// A dumb pipe. It opens a Core Audio process tap and writes raw interleaved float32
/// PCM to stdout. It does not buffer, does not write files, does not decide anything.
/// All policy lives in Rust — see CLAUDE.md §3.
///
/// The one thing it will not do is fail quietly. A tap without permission runs at the
/// correct rate and produces perfect silence, so silence is treated as a result worth
/// reporting, never as a successful recording.

let helperVersion = "0.1.0"

/// Seconds of pure-zero output tolerated before saying something, while audio is
/// provably playing.
let silenceGraceSeconds = 5.0
/// How often to repeat the warning once it has started.
let silenceRepeatSeconds = 15.0

let usage = """
    hearsay-audio-helper \(helperVersion)

    Lists and taps audio-producing processes using Core Audio process taps.
    No virtual audio device is created and the system's audio routing is never changed.

    Usage:
      hearsay-audio-helper --list [--all]
      hearsay-audio-helper --capture (--pid N | --bundle ID | --system) [--duration SEC]
      hearsay-audio-helper --probe   (--pid N | --bundle ID | --system) [--duration SEC]
      hearsay-audio-helper --check-permission [--request]
      hearsay-audio-helper --help | --version

    Commands:
      --list              Print audio-producing processes as JSON on stdout.
      --capture           Tap audio and write raw interleaved float32 PCM to stdout.
      --probe             Tap audio, measure it, and print a verdict. Writes no PCM.
      --check-permission  Report whether system audio capture is permitted.

    Options:
      --all           With --list, include processes not currently making sound.
      --pid N         Target this process. Repeatable.
      --bundle ID     Target every process with this bundle identifier. Repeatable.
      --system        Target everything the machine plays. Prefer --pid or --bundle.
      --duration SEC  Stop after this many seconds. --probe defaults to 4.
      --request       With --check-permission, ask macOS to show the prompt.

    Output:
      stdout  machine-readable only — JSON for --list and --probe, raw PCM for --capture
      stderr  JSON events (one per line, each with a "type") interleaved with plain text

    Exit codes:
      0   success
      64  bad usage
      69  no such process
      70  Core Audio error
      75  the tap produced only silence while audio was playing
      77  permission to capture system audio was denied
    """

// MARK: - Signals

/// Watches for SIGTERM and SIGINT without needing a run loop on the main thread.
///
/// The default handlers would kill the process outright, leaking the tap and the
/// aggregate device and truncating whatever the reader had in flight. Here the signal
/// only sets a flag; the main loop notices it and tears everything down in order.
final class SignalWatch {
    private let lock = NSLock()
    private var terminated = false
    private var sources: [DispatchSourceSignal] = []

    init(_ signals: [Int32]) {
        let queue = DispatchQueue(label: "com.hearsay.audio-helper.signals")
        for number in signals {
            // Ignore at the POSIX level so only the dispatch source sees it.
            signal(number, SIG_IGN)
            let source = DispatchSource.makeSignalSource(signal: number, queue: queue)
            source.setEventHandler { [weak self] in
                guard let self else { return }
                self.lock.lock()
                self.terminated = true
                self.lock.unlock()
            }
            source.resume()
            sources.append(source)
        }
    }

    var shouldStop: Bool {
        lock.lock()
        defer { lock.unlock() }
        return terminated
    }
}

// MARK: - Command line

struct Options {
    var command: String?
    var includeSilent = false
    var pids: [Int32] = []
    var bundles: [String] = []
    var systemWide = false
    var duration: Double?
    var requestPermission = false
}

func parseArguments(_ arguments: [String]) throws -> Options {
    var options = Options()
    var index = 0

    /// Reads the value after a flag, failing loudly rather than silently swallowing the
    /// next flag as if it were a value.
    func value(for flag: String) throws -> String {
        index += 1
        guard index < arguments.count, !arguments[index].hasPrefix("--") else {
            throw HelperError.usage("\(flag) needs a value")
        }
        return arguments[index]
    }

    while index < arguments.count {
        let argument = arguments[index]
        switch argument {
        case "--list", "--capture", "--probe", "--check-permission":
            options.command = argument
        case "--all":
            options.includeSilent = true
        case "--system":
            options.systemWide = true
        case "--request":
            options.requestPermission = true
        case "--pid":
            let raw = try value(for: "--pid")
            guard let pid = Int32(raw) else {
                throw HelperError.usage("--pid expects a number, got \(raw)")
            }
            options.pids.append(pid)
        case "--bundle":
            options.bundles.append(try value(for: "--bundle"))
        case "--duration":
            let raw = try value(for: "--duration")
            guard let seconds = Double(raw), seconds > 0 else {
                throw HelperError.usage("--duration expects a positive number, got \(raw)")
            }
            options.duration = seconds
        default:
            throw HelperError.usage("unrecognised argument: \(argument)")
        }
        index += 1
    }
    return options
}

/// Resolves the requested target, preferring process scope over system-wide so that
/// music playing alongside a meeting stays out of the recording.
func resolveTarget(_ options: Options) throws -> TapTarget {
    var processes: [AudioProcess] = []
    for pid in options.pids {
        processes.append(contentsOf: try findProcesses(pid: pid, bundleID: nil))
    }
    for bundle in options.bundles {
        processes.append(contentsOf: try findProcesses(pid: nil, bundleID: bundle))
    }

    if !processes.isEmpty {
        // --pid and --bundle can name the same process twice.
        var seen = Set<UInt32>()
        let unique = processes.filter { seen.insert($0.objectID).inserted }
        let described = unique.map { "\($0.name ?? "?") (pid \($0.pid))" }.joined(separator: ", ")
        log("tapping \(unique.count) process(es): \(described)")
        return .processes(unique)
    }

    guard options.systemWide else {
        throw HelperError.usage("this command needs --pid, --bundle, or --system")
    }
    log("tapping system-wide output")
    return .systemWide
}

/// Whether anything we are tapping is currently playing audio. This is what turns
/// "all zeros" from ambiguous into damning: if a target is provably producing output and
/// the tap yields nothing, the tap is broken, not the room.
func anyTargetProducingOutput(_ target: TapTarget) -> Bool {
    guard let processes = try? allAudioProcesses() else { return false }
    switch target {
    case .systemWide:
        let selfPID = ProcessInfo.processInfo.processIdentifier
        return processes.contains { $0.isRunningOutput && $0.pid != selfPID }
    case let .processes(wanted):
        let pids = Set(wanted.map(\.pid))
        return processes.contains { pids.contains($0.pid) && $0.isRunningOutput }
    }
}

// MARK: - Permission

func checkPermission(_ options: Options) {
    let granted = options.requestPermission
        ? AudioCapturePermission.request()
        : AudioCapturePermission.preflight()

    emit("permission", ["granted": granted, "requested": options.requestPermission])
    if !granted {
        log(AudioCapturePermission.instructions)
    }
    print(granted ? "granted" : "denied")
}

/// Warns — loudly — when permission looks missing, but does not refuse to record.
///
/// The preflight is a signal, not proof. It answers on behalf of the *responsible*
/// process, which for a helper spawned by an app is the app, and that attribution is not
/// always what you would expect. Treating a negative as fatal meant a false negative
/// blocked recording outright on a machine where capture worked fine.
///
/// So the order of trust is: try to record, and let the thing that can actually tell —
/// whether non-zero samples arrive while audio is provably playing — be the judge. That
/// check already exists and already reports. This one only sets expectations.
func warnIfPermissionLooksMissing() {
    guard !AudioCapturePermission.preflight() else { return }
    emit(
        "permission_warning",
        [
            "kind": "permission_denied",
            "message": "macOS reports that system audio capture is not permitted. "
                + "Recording will continue; if it captures only silence, this is why.",
        ])
    log(AudioCapturePermission.instructions)
}

// MARK: - Capture

func capture(_ options: Options) throws {
    warnIfPermissionLooksMissing()

    let target = try resolveTarget(options)
    let session = CaptureSession()
    let signals = SignalWatch([SIGTERM, SIGINT, SIGHUP])

    try session.start(target: target)
    emit("started", [:])

    let started = Date()
    let deadline = options.duration.map { started.addingTimeInterval($0) }
    var nextReport = started.addingTimeInterval(1)
    var lastSilenceWarning: Date?
    var stopReason = "duration"

    while true {
        if signals.shouldStop {
            stopReason = "signal"
            break
        }
        if session.isPipeClosed {
            stopReason = "pipe_closed"
            break
        }
        if let deadline, Date() >= deadline { break }

        let now = Date()
        if now >= nextReport {
            nextReport = now.addingTimeInterval(1)
            let stats = session.snapshot(resetInterval: true)
            emit(
                "level",
                [
                    "peak": Double(stats.intervalPeak),
                    "rms": stats.rms,
                    "frames": stats.frames,
                    "nonzero_samples": stats.nonZeroSamples,
                ])

            // The guard that gives this project its name-brand failure a voice.
            let elapsed = now.timeIntervalSince(started)
            if stats.nonZeroSamples == 0, elapsed >= silenceGraceSeconds,
                anyTargetProducingOutput(target)
            {
                let due =
                    lastSilenceWarning.map { now.timeIntervalSince($0) >= silenceRepeatSeconds }
                    ?? true
                if due {
                    lastSilenceWarning = now
                    emit(
                        "silence",
                        [
                            "elapsed_seconds": elapsed,
                            "message":
                                "the tap has produced only zeros while audio is playing — "
                                + "this recording will be silent",
                        ])
                }
            }
        }

        Thread.sleep(forTimeInterval: 0.05)
    }

    session.stop()
    let stats = session.snapshot()
    emit(
        "stopped",
        [
            "reason": stopReason,
            "frames": stats.frames,
            "nonzero_samples": stats.nonZeroSamples,
            "peak": Double(stats.peak),
        ])

    // Flush before exiting so the reader sees every last sample.
    fsync(STDOUT_FILENO)
}

// MARK: - Probe

/// Answers one question and nothing else: does this machine actually capture audio?
///
/// Runs a real tap for a few seconds without writing any PCM, watches whether anything
/// is playing, and prints a verdict. This is the check to run before trusting a
/// recording — and the one to run first when a recording comes back silent.
func probe(_ options: Options) throws -> Int32 {
    let preflight = AudioCapturePermission.preflight()
    let target = try resolveTarget(options)
    let duration = options.duration ?? 4.0

    let session = CaptureSession()
    session.writesToStdout = false
    let signals = SignalWatch([SIGTERM, SIGINT])

    try session.start(target: target)

    var audioWasPlaying = false
    let started = Date()
    while Date().timeIntervalSince(started) < duration, !signals.shouldStop {
        if anyTargetProducingOutput(target) { audioWasPlaying = true }
        Thread.sleep(forTimeInterval: 0.25)
    }
    session.stop()

    let stats = session.snapshot()
    let verdict: String
    let diagnosis: String
    let code: Int32

    if stats.nonZeroSamples > 0 {
        verdict = "capturing"
        diagnosis = "the tap is producing real audio"
        code = 0
    } else if !preflight {
        verdict = "permission_denied"
        diagnosis = AudioCapturePermission.instructions
        code = 77
    } else if audioWasPlaying {
        verdict = "silent_while_audio_playing"
        diagnosis =
            "audio was playing but every captured sample was zero. macOS reports the "
            + "permission as granted, so the grant may be stale — toggle it off and on in "
            + "System Settings → Privacy & Security → Screen & System Audio Recording, "
            + "then relaunch."
        code = 75
    } else {
        verdict = "no_audio_playing"
        diagnosis =
            "nothing played during the probe, so this run proves nothing. Start audio "
            + "and probe again."
        code = 3
    }

    let report: [String: Any] = [
        "type": "probe",
        "verdict": verdict,
        "diagnosis": diagnosis,
        "permission_preflight": preflight,
        "audio_was_playing": audioWasPlaying,
        "duration_seconds": duration,
        "sample_rate": session.format.mSampleRate,
        "channels": Int(session.format.mChannelsPerFrame),
        "frames": stats.frames,
        "nonzero_samples": stats.nonZeroSamples,
        "peak": Double(stats.peak),
        "rms": stats.rms,
    ]
    if let data = try? JSONSerialization.data(
        withJSONObject: report, options: [.prettyPrinted, .sortedKeys])
    {
        FileHandle.standardOutput.write(data)
        FileHandle.standardOutput.write(Data("\n".utf8))
    }
    return code
}

// MARK: - Entry point

func run() throws -> Int32 {
    // Without this a closed stdout kills the process outright, skipping teardown.
    signal(SIGPIPE, SIG_IGN)

    let arguments = Array(CommandLine.arguments.dropFirst())

    if arguments.isEmpty || arguments.contains("--help") || arguments.contains("-h") {
        log(usage)
        return 0
    }
    if arguments.contains("--version") {
        log(helperVersion)
        return 0
    }

    let options = try parseArguments(arguments)

    switch options.command {
    case "--list":
        try printProcessList(includeSilent: options.includeSilent)
    case "--check-permission":
        checkPermission(options)
    case "--capture":
        try capture(options)
    case "--probe":
        return try probe(options)
    default:
        throw HelperError.usage("no command given\n\n\(usage)")
    }
    return 0
}

do {
    exit(try run())
} catch let error as HelperError {
    log("error: \(error.description)")
    exit(error.exitCode)
} catch {
    log("error: \(error.localizedDescription)")
    exit(70)
}
