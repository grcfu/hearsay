import Foundation

/// hearsay-audio-helper
///
/// A dumb pipe. It opens a Core Audio process tap and writes raw interleaved float32
/// PCM to stdout. It does not buffer, does not write files, does not decide anything.
/// All policy lives in Rust — see CLAUDE.md §3.

let helperVersion = "0.1.0"

let usage = """
    hearsay-audio-helper \(helperVersion)

    Lists and taps audio-producing processes using Core Audio process taps.
    No virtual audio device is created and the system's audio routing is never changed.

    Usage:
      hearsay-audio-helper --list [--all]
      hearsay-audio-helper --capture (--pid N | --bundle ID)... [--duration SEC]
      hearsay-audio-helper --capture --system [--duration SEC]
      hearsay-audio-helper --help
      hearsay-audio-helper --version

    Options:
      --list         Print audio-producing processes as JSON on stdout.
      --all          With --list, include processes that are not currently making sound.
      --capture      Tap audio and write raw interleaved float32 PCM to stdout.
      --pid N        Capture this process. Repeatable.
      --bundle ID    Capture every process with this bundle identifier. Repeatable.
      --system       Capture everything the machine plays. Prefer --pid or --bundle.
      --duration SEC Stop after this many seconds. Runs until terminated if omitted.
      --help         Show this message.
      --version      Print the version.

    Output:
      stdout  machine-readable only — JSON for --list, raw PCM for --capture
      stderr  one JSON line describing the negotiated format, then diagnostics
    """

/// Parsed command line. Kept as a plain struct so `run()` stays a straight line.
struct Options {
    var command: String?
    var includeSilent = false
    var pids: [Int32] = []
    var bundles: [String] = []
    var systemWide = false
    var duration: Double?
}

func parseArguments(_ arguments: [String]) throws -> Options {
    var options = Options()
    var index = 0

    /// Reads the value that follows a flag, failing loudly rather than silently
    /// consuming the next flag as if it were a value.
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
        case "--list", "--capture":
            options.command = argument
        case "--all":
            options.includeSilent = true
        case "--system":
            options.systemWide = true
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

/// Resolves the requested target, preferring process scope over system-wide.
func resolveTarget(_ options: Options) throws -> TapTarget {
    var processes: [AudioProcess] = []
    for pid in options.pids {
        processes.append(contentsOf: try findProcesses(pid: pid, bundleID: nil))
    }
    for bundle in options.bundles {
        processes.append(contentsOf: try findProcesses(pid: nil, bundleID: bundle))
    }

    if !processes.isEmpty {
        // Deduplicate: --pid and --bundle can name the same process twice.
        var seen = Set<UInt32>()
        let unique = processes.filter { seen.insert($0.objectID).inserted }
        let described = unique.map { "\($0.name ?? "?") (pid \($0.pid))" }.joined(separator: ", ")
        log("tapping \(unique.count) process(es): \(described)")
        return .processes(unique)
    }

    guard options.systemWide else {
        throw HelperError.usage("--capture needs --pid, --bundle, or --system")
    }
    log("tapping system-wide output")
    return .systemWide
}

func capture(_ options: Options) throws {
    let target = try resolveTarget(options)
    let session = CaptureSession()
    try session.start(target: target)

    let deadline = options.duration.map { Date().addingTimeInterval($0) }
    while !session.isPipeClosed {
        if let deadline, Date() >= deadline { break }
        Thread.sleep(forTimeInterval: 0.05)
    }

    session.stop()
}

func run() throws {
    let arguments = Array(CommandLine.arguments.dropFirst())

    if arguments.isEmpty || arguments.contains("--help") || arguments.contains("-h") {
        log(usage)
        return
    }
    if arguments.contains("--version") {
        log(helperVersion)
        return
    }

    let options = try parseArguments(arguments)

    switch options.command {
    case "--list":
        try printProcessList(includeSilent: options.includeSilent)
    case "--capture":
        try capture(options)
    default:
        throw HelperError.usage("no command given\n\n\(usage)")
    }
}

do {
    try run()
} catch let error as HelperError {
    log("error: \(error.description)")
    exit(error.exitCode)
} catch {
    log("error: \(error.localizedDescription)")
    exit(70)
}
