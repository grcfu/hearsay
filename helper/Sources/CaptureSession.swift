import AudioToolbox
import CoreAudio
import Foundation

/// What to tap.
enum TapTarget {
    /// Only these processes. Preferred: music playing alongside the meeting stays out
    /// of the recording.
    case processes([AudioProcess])
    /// Everything the machine is playing.
    case systemWide
}

/// Opens a Core Audio process tap, wraps it in a private aggregate device, and writes
/// raw interleaved float32 PCM to stdout for as long as it runs.
///
/// This type owns two system resources — the tap and the aggregate device — and both
/// must be destroyed on the way out. `stop()` is idempotent so the signal handler and
/// the normal exit path can both call it.
final class CaptureSession {
    private var tapID: AudioObjectID = kAudioObjectUnknown
    private var aggregateID: AudioObjectID = kAudioObjectUnknown
    private var ioProcID: AudioDeviceIOProcID?
    private var running = false

    /// Scratch space for interleaving non-interleaved input. Allocated once, before the
    /// IO callback ever fires — never allocate on the audio thread.
    private var scratch: UnsafeMutablePointer<Float>?
    private var scratchCapacity = 0

    private let queue = DispatchQueue(label: "com.hearsay.audio-helper.io", qos: .userInitiated)
    private let lock = NSLock()

    /// The format the tap negotiated. Reported on stderr before any PCM is written.
    private(set) var format = AudioStreamBasicDescription()

    /// Set when the output pipe closes; the run loop notices and exits cleanly.
    private var pipeClosed = false

    // MARK: - Setup

    func start(target: TapTarget) throws {
        let description = try makeTapDescription(for: target)

        var tap = AudioObjectID(kAudioObjectUnknown)
        let tapStatus = AudioHardwareCreateProcessTap(description, &tap)
        guard tapStatus == noErr, tap != kAudioObjectUnknown else {
            throw HelperError.coreAudio(op: "AudioHardwareCreateProcessTap", status: tapStatus)
        }
        tapID = tap

        format = try readProperty(
            tapID, kAudioTapPropertyFormat, initial: AudioStreamBasicDescription(),
            op: "read tap format")
        try validate(format)

        aggregateID = try makeAggregateDevice(tapUID: description.uuid.uuidString)
        try allocateScratch()
        emitFormat()

        try installIOProc()

        let startStatus = AudioDeviceStart(aggregateID, ioProcID)
        guard startStatus == noErr else {
            throw HelperError.coreAudio(op: "AudioDeviceStart", status: startStatus)
        }
        running = true
    }

    private func makeTapDescription(for target: TapTarget) throws -> CATapDescription {
        let description: CATapDescription
        switch target {
        case let .processes(processes):
            let ids = processes.map { AudioObjectID($0.objectID) }
            description = CATapDescription(stereoMixdownOfProcesses: ids)
        case .systemWide:
            description = CATapDescription(stereoGlobalTapButExcludeProcesses: [])
        }
        description.uuid = UUID()
        description.name = "Hearsay"
        // Private: the tap does not show up as a device for other apps to find.
        description.isPrivate = true
        // Unmuted: tapping must not silence what the user is actually listening to.
        description.muteBehavior = .unmuted
        return description
    }

    /// The aggregate device is what actually pulls audio. It carries no sub-devices of
    /// its own — the default output device is named only as a clock source, and the tap
    /// is the sole audio source. Nothing about the user's routing changes.
    private func makeAggregateDevice(tapUID: String) throws -> AudioObjectID {
        let outputDevice: AudioObjectID = try readProperty(
            systemObject, kAudioHardwarePropertyDefaultOutputDevice,
            initial: AudioObjectID(kAudioObjectUnknown), op: "read default output device")

        let clockUID = readStringProperty(outputDevice, kAudioDevicePropertyDeviceUID) ?? ""

        let description: [String: Any] = [
            kAudioAggregateDeviceNameKey: "Hearsay Capture",
            kAudioAggregateDeviceUIDKey: UUID().uuidString,
            kAudioAggregateDeviceMainSubDeviceKey: clockUID,
            kAudioAggregateDeviceIsPrivateKey: true,
            kAudioAggregateDeviceIsStackedKey: false,
            kAudioAggregateDeviceTapAutoStartKey: true,
            kAudioAggregateDeviceSubDeviceListKey: [],
            kAudioAggregateDeviceTapListKey: [
                [
                    kAudioSubTapDriftCompensationKey: true,
                    kAudioSubTapUIDKey: tapUID,
                ]
            ],
        ]

        var device = AudioObjectID(kAudioObjectUnknown)
        let status = AudioHardwareCreateAggregateDevice(description as CFDictionary, &device)
        guard status == noErr, device != kAudioObjectUnknown else {
            throw HelperError.coreAudio(op: "AudioHardwareCreateAggregateDevice", status: status)
        }
        return device
    }

    private func validate(_ asbd: AudioStreamBasicDescription) throws {
        guard asbd.mFormatID == kAudioFormatLinearPCM else {
            throw HelperError.coreAudio(op: "tap format is not linear PCM", status: noErr)
        }
        guard asbd.mFormatFlags & kAudioFormatFlagIsFloat != 0, asbd.mBitsPerChannel == 32 else {
            throw HelperError.coreAudio(op: "tap format is not float32", status: noErr)
        }
        guard asbd.mChannelsPerFrame > 0, asbd.mSampleRate > 0 else {
            throw HelperError.coreAudio(op: "tap format has no channels", status: noErr)
        }
    }

    private func allocateScratch() throws {
        // Ask the aggregate device how large an IO buffer it will hand us, then take
        // a generous multiple so a format or buffer-size change mid-run cannot overrun.
        let frames: UInt32 =
            (try? readProperty(
                aggregateID, kAudioDevicePropertyBufferFrameSize, initial: UInt32(0),
                op: "read buffer frame size")) ?? 4096
        let capacity = Int(max(frames, 4096)) * Int(format.mChannelsPerFrame) * 8
        scratch = UnsafeMutablePointer<Float>.allocate(capacity: capacity)
        scratchCapacity = capacity
    }

    private func installIOProc() throws {
        var procID: AudioDeviceIOProcID?
        let status = AudioDeviceCreateIOProcIDWithBlock(&procID, aggregateID, queue) {
            [weak self] _, inInputData, _, _, _ in
            self?.handle(input: inInputData)
        }
        guard status == noErr, let procID else {
            throw HelperError.coreAudio(
                op: "AudioDeviceCreateIOProcIDWithBlock", status: status)
        }
        ioProcID = procID
    }

    // MARK: - Audio callback

    /// Called on the IO queue for every buffer the tap produces. Interleaves if needed
    /// and writes straight to stdout. No queueing, no retry, no policy — see CLAUDE.md §3.
    private func handle(input: UnsafePointer<AudioBufferList>) {
        let list = UnsafeMutableAudioBufferListPointer(
            UnsafeMutablePointer(mutating: input))
        guard list.count > 0, let scratch else { return }

        let channels = Int(format.mChannelsPerFrame)
        let interleaved = format.mFormatFlags & kAudioFormatFlagIsNonInterleaved == 0

        if interleaved {
            // One buffer already holding frames as L,R,L,R… — hand it straight over.
            let buffer = list[0]
            guard let data = buffer.mData, buffer.mDataByteSize > 0 else { return }
            writeToStdout(data, Int(buffer.mDataByteSize))
            return
        }

        // One buffer per channel. Interleave into scratch, then write once.
        let frames = Int(list[0].mDataByteSize) / MemoryLayout<Float>.size
        guard frames > 0 else { return }
        let usable = min(frames * channels, scratchCapacity)
        let framesToWrite = usable / channels

        for channel in 0..<min(channels, list.count) {
            guard let raw = list[channel].mData else { continue }
            let source = raw.assumingMemoryBound(to: Float.self)
            var frame = 0
            while frame < framesToWrite {
                scratch[frame * channels + channel] = source[frame]
                frame += 1
            }
        }

        writeToStdout(scratch, framesToWrite * channels * MemoryLayout<Float>.size)
    }

    /// A pipe write can be partial. Loop until it is all out, and treat a closed pipe as
    /// "the parent went away", not as an error worth shouting about.
    private func writeToStdout(_ base: UnsafeRawPointer, _ byteCount: Int) {
        var offset = 0
        while offset < byteCount {
            let written = write(STDOUT_FILENO, base.advanced(by: offset), byteCount - offset)
            if written > 0 {
                offset += written
                continue
            }
            if written == -1 && errno == EINTR { continue }
            if written == -1 && (errno == EPIPE || errno == EBADF) {
                lock.lock()
                pipeClosed = true
                lock.unlock()
                return
            }
            return
        }
    }

    var isPipeClosed: Bool {
        lock.lock()
        defer { lock.unlock() }
        return pipeClosed
    }

    // MARK: - Format reporting

    /// One line of JSON on stderr, before any PCM reaches stdout. Rust reads this to
    /// learn the sample rate and channel count it is about to receive.
    private func emitFormat() {
        let payload: [String: Any] = [
            "type": "format",
            "sample_rate": format.mSampleRate,
            "channels": Int(format.mChannelsPerFrame),
            "bits_per_sample": Int(format.mBitsPerChannel),
            "encoding": "f32le",
            "interleaved": true,
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: payload, options: [.sortedKeys])
        else { return }
        FileHandle.standardError.write(data)
        FileHandle.standardError.write(Data("\n".utf8))
    }

    // MARK: - Teardown

    /// Idempotent. Stops the device, then destroys the aggregate device and the tap in
    /// that order — the aggregate references the tap, so the tap goes last.
    func stop() {
        lock.lock()
        let wasRunning = running
        running = false
        lock.unlock()

        if wasRunning, let ioProcID {
            AudioDeviceStop(aggregateID, ioProcID)
            AudioDeviceDestroyIOProcID(aggregateID, ioProcID)
        }
        ioProcID = nil

        if aggregateID != kAudioObjectUnknown {
            AudioHardwareDestroyAggregateDevice(aggregateID)
            aggregateID = kAudioObjectUnknown
        }
        if tapID != kAudioObjectUnknown {
            AudioHardwareDestroyProcessTap(tapID)
            tapID = kAudioObjectUnknown
        }
        if let scratch {
            scratch.deallocate()
            self.scratch = nil
            scratchCapacity = 0
        }
    }

    deinit { stop() }
}
