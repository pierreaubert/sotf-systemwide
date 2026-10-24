// SharedMemory.swift - Shared memory interface for communication with Rust audio engine
//
// Security model:
// - Each user has their own shared memory region
// - Path is based on the console user's UID
// - Permissions allow only the user and _coreaudiod to access
// - The HAL driver (running as _coreaudiod) gets the console user's UID
//   to determine which shared memory region to use

import Darwin
import Foundation
import SystemConfiguration

/// Magic number for shared memory header validation: 'SOTF'
private let kSharedMemoryMagic: UInt32 = 0x534F5446

/// Current protocol version
/// Version 2: Added encryption fields (encrypted, key_fingerprint, frame_counter)
/// Version 3: Added config negotiation fields for bidirectional HAL-Daemon sync
/// Version 4: Added daemon heartbeat for stale-engine detection in the HAL driver
/// Version 5: Added configuring handshake for daemon-side reconfiguration
/// Version 6: Added a quiesce acknowledgment and pending channel geometry.
/// Version 7: Added output volume fields (volumeScalarBits, outputMuted,
/// volumeGeneration) in former padding slots so the HAL volume control
/// (OS media keys) and the daemon share one volume state.
/// Version 8: Identified configuration ACKs and atomic multiwriter volume tuples.
private let kSharedMemoryVersion: UInt32 = 8
private let kDaemonHeartbeatTimeoutMs: UInt64 = 3_000
// These bounds match driver-hal/src/shared_memory/consts.rs.
private let kMaximumSampleRate: UInt32 = 192_000
private let kMaximumBufferFrames: UInt32 = 4_096
private let kMaximumChannels: UInt32 = 32
private let kMaximumAudioBytes = 4_096 * 32 * 32 * MemoryLayout<Float>.size
private let kMaximumRecordBytes = 4_096 * 32 * MemoryLayout<Float>.size + 40

/// Encrypted audio record magic: 'SEA1' (SotF Encrypted Audio v1)
private let kEncryptedRecordMagic: UInt32 = 0x5345_4131
private let kEncryptedRecordHeaderBytes = 24
private let kEncryptedRecordHeaderFloats = kEncryptedRecordHeaderBytes / 4

// `configuring` is a cross-process state bitset shared with Rust. Bit 0
// blocks new IO commits, while bits 1 and 2 protect the reader and writer
// publication stores respectively. This prevents a reconfiguration from
// resetting ring positions and then losing a stale cursor update.
private let kConfiguringReconfigure: UInt32 = 1
private let kConfiguringReadCommit: UInt32 = 2
private let kConfiguringWriteCommit: UInt32 = 4
private let kConfiguringIOMask: UInt32 = kConfiguringReadCommit | kConfiguringWriteCommit
private let kReconfigurationTimeoutNanoseconds: UInt64 = 5_000_000

@inline(__always)
private func atomicLoad(_ pointer: UnsafeMutablePointer<UInt32>) -> UInt32 {
    sotf_atomic_load_u32(pointer)
}

@inline(__always)
private func atomicLoad(_ pointer: UnsafeMutablePointer<UInt64>) -> UInt64 {
    sotf_atomic_load_u64(pointer)
}

@inline(__always)
private func atomicStore(_ pointer: UnsafeMutablePointer<UInt32>, _ value: UInt32) {
    sotf_atomic_store_u32(pointer, value)
}

@inline(__always)
private func atomicStore(_ pointer: UnsafeMutablePointer<UInt64>, _ value: UInt64) {
    sotf_atomic_store_u64(pointer, value)
}

@inline(__always)
private func reconfigurationRequested(_ header: UnsafeMutablePointer<SharedAudioHeader>) -> Bool {
    return atomicLoad(&header.pointee.configuring) & kConfiguringReconfigure != 0
}

@inline(__always)
private func tryAcquireIOCommit(
    _ header: UnsafeMutablePointer<SharedAudioHeader>,
    bit: UInt32
) -> Bool {
    var state = atomicLoad(&header.pointee.configuring)
    for _ in 0..<8 {
        if state & kConfiguringReconfigure != 0 || state & bit != 0 {
            return false
        }
        if sotf_atomic_compare_exchange_u32(
            &header.pointee.configuring,
            state,
            state | bit
        ) {
            return true
        }
        state = atomicLoad(&header.pointee.configuring)
    }
    return false
}

@inline(__always)
private func releaseIOCommit(
    _ header: UnsafeMutablePointer<SharedAudioHeader>,
    bit: UInt32
) {
    sotf_atomic_clear_u32(&header.pointee.configuring, bit)
}

@inline(__always)
private func clearReconfiguration(
    _ header: UnsafeMutablePointer<SharedAudioHeader>
) {
    var state = atomicLoad(&header.pointee.configuring)
    while true {
        let next = state & ~kConfiguringReconfigure
        if sotf_atomic_compare_exchange_u32(
            &header.pointee.configuring,
            state,
            next
        ) {
            return
        }
        state = atomicLoad(&header.pointee.configuring)
    }
}

/// Claim the shared-memory geometry transition bit and wait for in-flight
/// cursor publications to finish. Geometry must never be changed while a
/// Rust or Swift IO endpoint can still publish a cursor based on the old
/// layout.
@inline(__always)
private func beginReconfiguration(
    _ header: UnsafeMutablePointer<SharedAudioHeader>
) -> Bool {
    let start = DispatchTime.now().uptimeNanoseconds
    var state = atomicLoad(&header.pointee.configuring)

    while true {
        if state & kConfiguringReconfigure != 0 {
            return false
        }
        if sotf_atomic_compare_exchange_u32(
            &header.pointee.configuring,
            state,
            state | kConfiguringReconfigure
        ) {
            break
        }
        state = atomicLoad(&header.pointee.configuring)
        if DispatchTime.now().uptimeNanoseconds - start >= kReconfigurationTimeoutNanoseconds {
            return false
        }
    }

    while atomicLoad(&header.pointee.configuring) & kConfiguringIOMask != 0 {
        if DispatchTime.now().uptimeNanoseconds - start >= kReconfigurationTimeoutNanoseconds {
            clearReconfiguration(header)
            return false
        }
        _ = sched_yield()
    }

    return true
}

@inline(__always)
private func commitReadPosition(
    _ header: UnsafeMutablePointer<SharedAudioHeader>,
    _ position: UInt64
) -> Bool {
    // The reader owns this bit for the entire copy, not just publication.
    guard atomicLoad(&header.pointee.configuring) & kConfiguringReadCommit != 0 else { return false }
    atomicStore(&header.pointee.readPosition, position)
    return true
}

private func currentUnixMillis() -> UInt64 {
    return clock_gettime_nsec_np(CLOCK_REALTIME) / 1_000_000
}

/// Get the shared memory path for the current console user
///
/// Security model: each user has their own shared memory region.
/// Path is based on the console user's UID.
///
/// IMPORTANT: This must match the Rust side in shared_memory.rs which uses:
/// `/tmp/sotf-{uid}/audio.shm`
func getSharedMemoryPath() -> String {
    let environment = ProcessInfo.processInfo.environment
    if let overridePath = environment["SOTF_HAL_SHARED_MEMORY_PATH"], !overridePath.isEmpty {
        return overridePath
    }
    if let runtimeDir = environment["SOTF_SYSTEMWIDE_RUNTIME_DIR"], !runtimeDir.isEmpty {
        return (runtimeDir as NSString).appendingPathComponent("audio.shm")
    }

    // Get the console user (the human logged in, not _coreaudiod)
    var uid: uid_t = 0
    var gid: gid_t = 0

    if SCDynamicStoreCopyConsoleUser(nil, &uid, &gid) != nil {
        let filePath = "/tmp/sotf-\(uid)/audio.shm"
        return filePath
    }

    // No console user - this shouldn't happen in normal operation
    halLog("ERROR: No console user found, cannot determine shared memory path")
    // Return a path that will fail to open, forcing proper error handling
    return "/tmp/sotf-unknown/audio.shm"
}

/// Whether an opened shared-memory file is safe to map.
///
/// The daemon creates the file mode 0600 owned by the console user with a
/// 0700 parent. Require all three plus a regular file: anything else is a
/// planted or stale file that must never carry system audio. When no console
/// user exists, only an explicit lab override path is accepted, matching the
/// fail-closed `/tmp/sotf-unknown/audio.shm` rule above.
private func sharedMemoryFileIsDaemonOwned(_ statBuf: stat) -> Bool {
    guard (Int(statBuf.st_mode) & Int(S_IFMT)) == Int(S_IFREG) else { return false }
    let mode = Int(statBuf.st_mode) & 0o777
    guard mode & 0o077 == 0, mode & 0o600 == 0o600 else { return false }

    var uid: uid_t = 0
    var gid: gid_t = 0
    if SCDynamicStoreCopyConsoleUser(nil, &uid, &gid) != nil, uid != 0 {
        return statBuf.st_uid == uid
    }
    let environment = ProcessInfo.processInfo.environment
    if let overridePath = environment["SOTF_HAL_SHARED_MEMORY_PATH"], !overridePath.isEmpty {
        return true
    }
    return false
}

/// Header structure for shared memory region
/// Must match the Rust side exactly (SharedAudioHeader in shared_memory.rs)
struct SharedAudioHeader {
    var magic: UInt32           // 0x534F5446 ('SOTF')
    var version: UInt32         // Protocol version
    var sampleRate: UInt32      // Current sample rate
    var bufferFrames: UInt32    // Frames per buffer
    var channelCount: UInt32    // Number of channels

    // Ring buffer state (atomic on both sides)
    var writePosition: UInt64   // Write position in samples
    var readPosition: UInt64    // Read position in samples

    // Control flags (atomic)
    var active: UInt32          // IO is running
    var configChanged: UInt32   // Config change notification
    var driverReady: UInt32     // Driver is initialized
    var engineReady: UInt32     // Rust engine is connected

    // Encryption fields (version 2+)
    var encrypted: UInt32       // 0 = disabled, 1 = enabled
    var volumeScalarBits: UInt32 // Output volume scalar bits (version 7+)
    // First 8 bytes of SHA256, stored as a UInt64 with canonical big-endian
    // conversion. This mirrors Rust's AtomicU64 field and keeps the offset
    // aligned at byte 64.
    var keyFingerprint: UInt64
    var frameCounter: UInt64    // Frame counter for nonce generation

    // Config negotiation fields (version 3+)
    var requestedSampleRate: UInt32     // Requested sample rate
    var requestedBufferFrames: UInt32   // Requested buffer frames
    var actualSampleRate: UInt32        // Actual sample rate in use
    var actualBufferFrames: UInt32      // Actual buffer frames in use
    var configStatus: UInt32            // 0=pending, 1=accepted, 2=negotiated, 3=error
    var configSource: UInt32            // 1=HAL initiated, 2=Daemon initiated
    var configErrorCode: UInt32         // Error code if configStatus=3
    var outputMuted: UInt32             // Output mute flag (version 7+)

    // Statistics
    var encryptionOverflowCount: UInt64 // Encrypted write drops due to insufficient ring space
    var daemonHeartbeatMs: UInt64       // Daemon liveness heartbeat in Unix epoch milliseconds

    // Reconfiguration handshake (version 5+)
    var configuring: UInt32             // Daemon is mutating geometry/ring positions
    var configuringAck: UInt32          // IO participant observed configuring=1
    var requestedChannelCount: UInt32   // Pending channel geometry request
    var volumeGeneration: UInt32        // Reserved version 7 publication counter
    var controlLock: UInt32 = 0
    var actualChannelCount: UInt32 = 0
    var configRequestID: UInt64 = 0
    var configAckID: UInt64 = 0
    var volumeState: UInt64 = 0
    var ringGeneration: UInt64 = 1
}

struct SharedConfigurationSnapshot {
    let requestID: UInt64
    let sampleRate: UInt32
    let bufferFrames: UInt32
    let channelCount: UInt32
    let source: UInt32
    let actualSampleRate: UInt32
    let actualBufferFrames: UInt32
    let actualChannelCount: UInt32
    let status: UInt32
    let errorCode: UInt32
    let pending: Bool
}

private struct EncryptedRecordMetadata {
    let sampleCount: Int
    let frameCounter: UInt64
    let ciphertextLen: Int
    let totalBytes: Int
    let floatCount: Int
}

/// Shared memory buffer for audio exchange with Rust engine
final class SharedAudioBuffer {
    private var sharedMemory: UnsafeMutableRawPointer?
    private var memorySize: Int = 0
    private var fileDescriptor: Int32 = -1
    private var rendezvousDescriptor: Int32 = -1
    private var backingLength: Int64 = 0
    private var brokeredMapping = false

    /// The path to the shared memory file (stored for cleanup)
    private var currentPath: String = ""
    private var backingDevice: dev_t = 0
    private var backingInode: ino_t = 0
    private var validatedMapping = false

    /// Ring depth in blocks at the 48 kHz reference rate. The depth scales
    /// with the sample rate so the ring holds the same *duration* at every
    /// rate (8 blocks of 512 stereo frames: 85 ms at 48 kHz but only 42.7 ms
    /// at 96 kHz). Must match Rust `ring_block_count` in driver-hal consts.
    private func ringBufferCount(forSampleRate sampleRate: UInt32) -> Int {
        let multiplier = max(1, (Int(sampleRate) + 47_999) / 48_000)
        return 8 * multiplier
    }

    /// Validate before converting untrusted geometry into offsets or allocation sizes.
    private func validatedCapacity(sampleRate: UInt32, bufferFrames: UInt32, channelCount: UInt32) -> Int? {
        guard sampleRate > 0, sampleRate <= kMaximumSampleRate,
              bufferFrames > 0, bufferFrames <= kMaximumBufferFrames,
              channelCount > 0, channelCount <= kMaximumChannels else { return nil }
        // The bounds above make every multiplication fit in Int on supported hosts.
        let capacity = Int(bufferFrames) * Int(channelCount) * ringBufferCount(forSampleRate: sampleRate)
        let alignedHeaderSize = (MemoryLayout<SharedAudioHeader>.size + 63) & ~63
        guard capacity * MemoryLayout<Float>.size <= memorySize - alignedHeaderSize else { return nil }
        return capacity
    }

    /// Header pointer (start of shared memory)
    private var header: UnsafeMutablePointer<SharedAudioHeader>? {
        sharedMemory?.assumingMemoryBound(to: SharedAudioHeader.self)
    }

    /// Audio data pointer (after header)
    private var audioData: UnsafeMutablePointer<Float>? {
        guard let mem = sharedMemory else { return nil }
        let headerSize = MemoryLayout<SharedAudioHeader>.size
        // Align to 64 bytes for SIMD
        let alignedOffset = (headerSize + 63) & ~63
        return mem.advanced(by: alignedOffset).assumingMemoryBound(to: Float.self)
    }

    /// Total capacity in samples
    private var audioCapacity: Int = 0
    private var encryptedPlaintextScratch: [UInt8] = []
    private var encryptedPayloadScratch: [UInt8] = []
    private var encryptedHeaderScratch: [UInt8] = []

    /// Whether we're connected to shared memory
    var isConnected: Bool {
        guard validatedMapping, let header = header else { return false }
        return atomicLoad(&header.pointee.magic) == kSharedMemoryMagic
            && atomicLoad(&header.pointee.version) == kSharedMemoryVersion
    }

    /// Whether the Rust engine is connected.
    ///
    /// Important: when `header` is nil (shared memory never initialised or
    /// init failed), we must return `false`. The previous formulation
    /// `header?.pointee.engineReady != 0` returns `true` when `header` is nil
    /// because Swift treats `nil != 0` as true for optional comparisons —
    /// callers then see `engineReady=1, isConnected=0` which is nonsense.
    var engineReady: Bool {
        guard let header = header, atomicLoad(&header.pointee.engineReady) != 0 else {
            return false
        }

        // Protocol v4 treats engineReady as valid only while the daemon keeps
        // refreshing its heartbeat. This prevents CoreAudio from filling the
        // ring forever after a daemon crash/stall leaves engineReady stuck at 1.
        if atomicLoad(&header.pointee.version) < 4 {
            return true
        }

        let heartbeat = atomicLoad(&header.pointee.daemonHeartbeatMs)
        guard heartbeat != 0 else {
            return false
        }

        let now = currentUnixMillis()
        if now < heartbeat {
            return true
        }
        return now - heartbeat <= kDaemonHeartbeatTimeoutMs
    }

    #if SOTF_HAL_TESTING
    /// Model daemon liveness without changing readiness or callback behavior.
    func testSetDaemonHeartbeat(_ unixMillis: UInt64) {
        guard validatedMapping, let header else { return }
        atomicStore(&header.pointee.daemonHeartbeatMs, unixMillis)
    }
    #endif

    /// Debug: get connection state details
    var connectionStateDebug: String {
        let hasMem = sharedMemory != nil
        let magic = header.map { atomicLoad(&$0.pointee.magic) } ?? 0
        let expectedMagic = kSharedMemoryMagic
        let magicMatch = magic == expectedMagic
        let version = header.map { atomicLoad(&$0.pointee.version) } ?? 0
        let engineFlag = header.map { atomicLoad(&$0.pointee.engineReady) } ?? 0
        let heartbeat = (version >= 4) ? (header.map { atomicLoad(&$0.pointee.daemonHeartbeatMs) } ?? 0) : 0
        let heartbeatAge: String
        if heartbeat == 0 {
            heartbeatAge = "none"
        } else {
            let now = currentUnixMillis()
            heartbeatAge = now >= heartbeat ? "\(now - heartbeat)ms" : "future"
        }
        return "mem=\(hasMem), magic=0x\(String(format: "%08X", magic)) (expected 0x\(String(format: "%08X", expectedMagic))), magicMatch=\(magicMatch), version=\(version), engineReadyFlag=\(engineFlag), engineReady=\(engineReady), daemonHeartbeatMs=\(heartbeat), daemonHeartbeatAge=\(heartbeatAge)"
    }

    /// Initialize shared memory
    /// - Parameters:
    ///   - sampleRate: Sample rate in Hz
    ///   - bufferFrames: Frames per buffer
    ///   - channelCount: Number of audio channels
    /// - Returns: true if successful
    func initialize(sampleRate: UInt32, bufferFrames: UInt32, channelCount: UInt32,
                    initialVolumeScalar: Float32 = 1, initialMuted: Bool = false) -> Bool {
        let headerSize = MemoryLayout<SharedAudioHeader>.size
        let alignedHeaderSize = (headerSize + 63) & ~63  // 64-byte aligned
        guard sampleRate > 0, sampleRate <= kMaximumSampleRate,
              bufferFrames > 0, bufferFrames <= kMaximumBufferFrames,
              channelCount > 0, channelCount <= kMaximumChannels else { return false }

        // Get the shared memory path (secure per-user path or legacy)
        currentPath = getSharedMemoryPath()
        halLog("SharedMemory: opening daemon mapping at \(currentPath)")

        if sharedMemory != nil || fileDescriptor >= 0 {
            closeSharedMemory()
        }
        var initialized = false
        defer { if !initialized { closeSharedMemory() } }

        // Open the daemon-owned file. The HAL plugin runs inside coreaudiod's
        // restricted environment, so creation/sizing/permissions are handled by
        // the daemon before IO starts. O_NOFOLLOW refuses a symlinked path
        // outright; the fstat checks below then pin the opened file to a
        // console-user-owned, owner-only regular file.
        fileDescriptor = Darwin.open(currentPath, O_RDONLY | O_NOFOLLOW)
        if fileDescriptor < 0 {
            halLog("SharedMemory: open failed: \(String(cString: strerror(errno)))")
            return false
        }

        var statBuf = stat()
        if fstat(fileDescriptor, &statBuf) != 0 {
            halLog("SharedMemory: fstat failed: \(String(cString: strerror(errno)))")
            Darwin.close(fileDescriptor)
            fileDescriptor = -1
            return false
        }
        guard sharedMemoryFileIsDaemonOwned(statBuf) else {
            halLog("SharedMemory: refusing file with unexpected owner/type/permissions")
            Darwin.close(fileDescriptor)
            fileDescriptor = -1
            return false
        }
        backingDevice = statBuf.st_dev
        backingInode = statBuf.st_ino
        backingLength = statBuf.st_size

        if statBuf.st_size == 32 {
            var record = [UInt8](repeating: 0, count: 32)
            let count = record.withUnsafeMutableBytes {
                pread(fileDescriptor, $0.baseAddress, $0.count, 0)
            }
            guard count == 32, Array(record.prefix(8)) == Array("SOTFMM08".utf8) else { return false }
            var length: UInt64 = 0
            for index in 0..<8 { length |= UInt64(record[8 + index]) << (8 * index) }
            guard length >= UInt64(alignedHeaderSize),
                  length <= UInt64(alignedHeaderSize + kMaximumAudioBytes) else { return false }
            let suffix = record[16..<24].map { String(format: "%02x", $0) }.joined()
            let endpoint = ((currentPath as NSString).deletingLastPathComponent as NSString)
                .appendingPathComponent(".fd-\(suffix)")
            let received = record.withUnsafeBufferPointer { bytes in
                endpoint.withCString { path in
                    sotf_receive_memory_descriptor(path, statBuf.st_uid, bytes.baseAddress, bytes.count)
                }
            }
            guard received >= 0 else {
                halLog("SharedMemory: immutable descriptor handoff failed: \(String(cString: strerror(errno)))")
                return false
            }
            rendezvousDescriptor = fileDescriptor
            fileDescriptor = received
            brokeredMapping = true
            memorySize = Int(length)
        } else {
            #if SOTF_HAL_TESTING
            // Explicit legacy fixtures are admitted only by the test build.
            // Production initialization always uses an immutable descriptor.
            guard statBuf.st_size >= alignedHeaderSize,
                  statBuf.st_size <= alignedHeaderSize + kMaximumAudioBytes else { return false }
            Darwin.close(fileDescriptor)
            fileDescriptor = Darwin.open(currentPath, O_RDWR | O_NOFOLLOW)
            guard fileDescriptor >= 0 else { return false }
            var fixtureStat = stat()
            guard fstat(fileDescriptor, &fixtureStat) == 0,
                  sharedMemoryFileIsDaemonOwned(fixtureStat),
                  fixtureStat.st_dev == backingDevice, fixtureStat.st_ino == backingInode,
                  fixtureStat.st_size == backingLength else { return false }
            memorySize = Int(fixtureStat.st_size)
            #else
            halLog("SharedMemory: refusing a truncatable audio file")
            return false
            #endif
        }

        // Map memory
        sharedMemory = mmap(nil, memorySize, PROT_READ | PROT_WRITE, MAP_SHARED, fileDescriptor, 0)
        if sharedMemory == MAP_FAILED {
            halLog("SharedMemory: mmap failed: \(String(cString: strerror(errno)))")
            Darwin.close(fileDescriptor)
            fileDescriptor = -1
            sharedMemory = nil
            return false
        }

        // Initialize header
        guard let header = header else { return false }

        // If shared memory was already initialized by the daemon (running before
        // coreaudiod was respawned), preserve its runtime state — engineReady,
        // ring positions, encryption session, config-negotiation state. Wiping
        // those silently flips engineReady from 1 → 0 behind the daemon's back
        // and stops the audio path. Only fresh memory should be zeroed.
        let alreadyInitialized = (atomicLoad(&header.pointee.magic) == kSharedMemoryMagic)

        if !alreadyInitialized {
            halLog("SharedMemory: existing file is not daemon-initialized")
            munmap(sharedMemory!, memorySize)
            sharedMemory = nil
            Darwin.close(fileDescriptor)
            fileDescriptor = -1
            return false
        }
        guard atomicLoad(&header.pointee.version) == kSharedMemoryVersion,
              tryAcquireIOCommit(header, bit: kConfiguringWriteCommit) else { return false }
        defer { releaseIOCommit(header, bit: kConfiguringWriteCommit) }

        let activeSampleRate = atomicLoad(&header.pointee.sampleRate)
        let activeBufferFrames = atomicLoad(&header.pointee.bufferFrames)
        let activeChannelCount = atomicLoad(&header.pointee.channelCount)
        guard let capacity = validatedCapacity(
            sampleRate: activeSampleRate, bufferFrames: activeBufferFrames, channelCount: activeChannelCount
        ) else {
            halLog("SharedMemory: daemon header has invalid active geometry")
            return false
        }

        let writePosition = atomicLoad(&header.pointee.writePosition)
        let readPosition = atomicLoad(&header.pointee.readPosition)
        guard writePosition >= readPosition, writePosition - readPosition <= UInt64(capacity) else { return false }
        audioCapacity = capacity
        encryptedPlaintextScratch = [UInt8](repeating: 0, count: kMaximumRecordBytes)
        encryptedPayloadScratch = [UInt8](repeating: 0, count: kMaximumRecordBytes)
        encryptedHeaderScratch = [UInt8](repeating: 0, count: kEncryptedRecordHeaderBytes)
        if brokeredMapping {
            let old = atomicLoad(&header.pointee.volumeState)
            if old >> 33 == 0 {
                let bits = UInt64(clampVolumeScalar(initialVolumeScalar).bitPattern)
                    | (initialMuted ? UInt64(1) << 32 : 0) | (UInt64(1) << 33)
                // Never overwrite an IPC volume writer that published while
                // this control-thread attachment was being prepared.
                _ = sotf_atomic_compare_exchange_u64(&header.pointee.volumeState, old, bits)
            }
        }
        validatedMapping = true
        atomicStore(&header.pointee.driverReady, 1)
        if activeSampleRate != sampleRate
            || activeBufferFrames != bufferFrames
            || activeChannelCount != channelCount {
            requestConfigChange(
                sampleRate: sampleRate,
                bufferFrames: bufferFrames,
                channelCount: channelCount
            )
            halLog(
                "SharedMemory: queued reconnect geometry (sampleRate)Hz, "
                    + "(bufferFrames) frames, (channelCount) channels; "
                    + "active remains (activeSampleRate)Hz, (activeBufferFrames) frames, "
                    + "(activeChannelCount) channels"
            )
        }

        let engineReady = atomicLoad(&header.pointee.engineReady)
        halLog("SharedMemory: initialized (version \(kSharedMemoryVersion), fresh=\(!alreadyInitialized), engineReady=\(engineReady))")
        initialized = true
        return true
    }

    /// Apply negotiated geometry after the daemon has acknowledged it.
    ///
    /// This is the only Swift-side path that mutates active ring geometry.
    /// The request path writes only requested fields; this method waits for
    /// both endpoint publication bits before changing the active fields and
    /// resetting cursors.
    func applyActiveConfiguration(
        sampleRate: UInt32,
        bufferFrames: UInt32,
        channelCount: UInt32
    ) -> Bool {
        guard validatedMapping, let header,
              let newCapacity = validatedCapacity(
                sampleRate: sampleRate, bufferFrames: bufferFrames, channelCount: channelCount
              ) else {
            return false
        }

        guard beginReconfiguration(header) else {
            halLog("SharedMemory: timed out waiting to apply negotiated geometry")
            return false
        }
        defer { clearReconfiguration(header) }

        let generation = atomicLoad(&header.pointee.ringGeneration)
        guard generation < UInt64.max else { return false }
        atomicStore(&header.pointee.ringGeneration, generation + 1)
        atomicStore(&header.pointee.sampleRate, sampleRate)
        atomicStore(&header.pointee.bufferFrames, bufferFrames)
        atomicStore(&header.pointee.channelCount, channelCount)
        atomicStore(&header.pointee.writePosition, 0)
        atomicStore(&header.pointee.readPosition, 0)
        audioCapacity = newCapacity
        // The IO participant acknowledged the reconfiguration while the
        // configuring bit was set. Once the new geometry and cursors are
        // published, release that acknowledgement just as the Rust side does
        // at the end of its reconfigure transaction.
        atomicStore(&header.pointee.configuringAck, 0)
        return true
    }

    /// Close shared memory
    func closeSharedMemory() {
        if validatedMapping, let header = header {
            atomicStore(&header.pointee.driverReady, 0)
        }
        validatedMapping = false

        if let mem = sharedMemory, memorySize > 0 {
            munmap(mem, memorySize)
        }
        sharedMemory = nil
        memorySize = 0
        audioCapacity = 0

        if fileDescriptor >= 0 {
            Darwin.close(fileDescriptor)
            fileDescriptor = -1
        }
        if rendezvousDescriptor >= 0 {
            Darwin.close(rendezvousDescriptor)
            rendezvousDescriptor = -1
        }
        brokeredMapping = false
        backingLength = 0

        backingDevice = 0
        backingInode = 0

        halLog("SharedMemory: closed")
    }

    /// Check from the maintenance queue that the mapped inode is still the
    /// file published at `currentPath`. This must never run on a CoreAudio IO
    /// callback because it performs filesystem metadata calls.
    func backingFileIsCurrent() -> Bool {
        guard fileDescriptor >= 0, !currentPath.isEmpty else { return false }

        var descriptorStat = stat()
        let identityDescriptor = rendezvousDescriptor >= 0 ? rendezvousDescriptor : fileDescriptor
        guard fstat(identityDescriptor, &descriptorStat) == 0 else { return false }

        var pathStat = stat()
        guard lstat(currentPath, &pathStat) == 0 else { return false }
        return descriptorStat.st_dev == pathStat.st_dev
            && descriptorStat.st_ino == pathStat.st_ino
            && descriptorStat.st_dev == backingDevice
            && descriptorStat.st_ino == backingInode
            && descriptorStat.st_size == backingLength
            && pathStat.st_size == backingLength
    }

    /// Set active state
    func setActive(_ active: Bool) {
        guard let header = header else { return }
        atomicStore(&header.pointee.active, active ? 1 : 0)
    }

    /// Signal configuration change to Rust engine
    func signalConfigChange() {
        guard let header = header else { return }
        atomicStore(&header.pointee.configChanged, 1)
    }

    /// Update sample rate (called when CoreAudio changes the device sample rate)
    /// This uses the config negotiation protocol to notify the daemon
    func updateSampleRate(_ sampleRate: UInt32) {
        guard let header = header else { return }
        // Publish a request only. `sampleRate` is active geometry and must
        // remain unchanged until the daemon acknowledges the transition.
        requestConfigChange(
            sampleRate: sampleRate,
            bufferFrames: atomicLoad(&header.pointee.bufferFrames),
            channelCount: atomicLoad(&header.pointee.channelCount)
        )
        halLog("updateSampleRate: requested \(sampleRate)Hz via config negotiation")
    }

    func getActiveSampleRate() -> UInt32 {
        guard let header else { return 0 }
        return atomicLoad(&header.pointee.sampleRate)
    }

    func getActiveBufferFrames() -> UInt32 {
        guard let header else { return 0 }
        return atomicLoad(&header.pointee.bufferFrames)
    }

    func getActiveChannelCount() -> UInt32 {
        guard let header else { return 0 }
        return atomicLoad(&header.pointee.channelCount)
    }

    private func fingerprintMatches(_ headerFingerprint: UInt64, _ cipherFingerprint: [UInt8]) -> Bool {
        guard cipherFingerprint.count == 8 else { return false }

        var difference: UInt8 = 0
        for index in 0..<8 {
            let shift = UInt64((7 - index) * 8)
            let byte = UInt8((headerFingerprint >> shift) & 0xff)
            difference |= byte ^ cipherFingerprint[index]
        }
        return difference == 0
    }

    private func withMatchingCipher(_ header: UnsafeMutablePointer<SharedAudioHeader>, body: (AudioCipher) -> Int) -> Int {
        let headerFingerprint = atomicLoad(&header.pointee.keyFingerprint)
        return EncryptionKeyManager.shared.withCipher { cipher in
            guard let cipher, fingerprintMatches(headerFingerprint, cipher.getFingerprint()) else { return 0 }
            return body(cipher)
        }
    }

    private func writeUInt32BE(_ value: UInt32, into bytes: inout [UInt8], at offset: Int) {
        bytes[offset] = UInt8((value >> 24) & 0xff)
        bytes[offset + 1] = UInt8((value >> 16) & 0xff)
        bytes[offset + 2] = UInt8((value >> 8) & 0xff)
        bytes[offset + 3] = UInt8(value & 0xff)
    }

    private func writeUInt64BE(_ value: UInt64, into bytes: inout [UInt8], at offset: Int) {
        for index in 0..<8 {
            let shift = UInt64((7 - index) * 8)
            bytes[offset + index] = UInt8((value >> shift) & 0xff)
        }
    }

    private func readUInt32BE(_ bytes: [UInt8], at offset: Int) -> UInt32 {
        return (UInt32(bytes[offset]) << 24) |
               (UInt32(bytes[offset + 1]) << 16) |
               (UInt32(bytes[offset + 2]) << 8) |
               UInt32(bytes[offset + 3])
    }

    private func readUInt64BE(_ bytes: [UInt8], at offset: Int) -> UInt64 {
        var value: UInt64 = 0
        for index in 0..<8 {
            value = (value << 8) | UInt64(bytes[offset + index])
        }
        return value
    }

    private func writeEncryptedRecordHeader(_ bytes: inout [UInt8], sampleCount: Int, frameCounter: UInt64, ciphertextLen: Int) -> Bool {
        guard bytes.count >= kEncryptedRecordHeaderBytes,
              sampleCount > 0,
              sampleCount <= Int(UInt32.max),
              ciphertextLen <= Int(UInt32.max) else {
            return false
        }

        writeUInt32BE(kEncryptedRecordMagic, into: &bytes, at: 0)
        writeUInt32BE(UInt32(sampleCount), into: &bytes, at: 4)
        writeUInt64BE(frameCounter, into: &bytes, at: 8)
        writeUInt32BE(UInt32(ciphertextLen), into: &bytes, at: 16)
        writeUInt32BE(0, into: &bytes, at: 20)
        return true
    }

    private func parseEncryptedRecordHeader(_ bytes: [UInt8]) -> EncryptedRecordMetadata? {
        guard bytes.count >= kEncryptedRecordHeaderBytes else { return nil }

        let magic = readUInt32BE(bytes, at: 0)
        guard magic == kEncryptedRecordMagic else { return nil }

        let sampleCount = Int(readUInt32BE(bytes, at: 4))
        let frameCounter = readUInt64BE(bytes, at: 8)
        let ciphertextLen = Int(readUInt32BE(bytes, at: 16))
        let reserved = readUInt32BE(bytes, at: 20)
        let expectedCiphertextLen = sampleCount * MemoryLayout<Float>.size + 16

        guard sampleCount > 0,
              reserved == 0,
              ciphertextLen == expectedCiphertextLen else {
            return nil
        }

        let totalBytes = kEncryptedRecordHeaderBytes + ciphertextLen
        return EncryptedRecordMetadata(
            sampleCount: sampleCount,
            frameCounter: frameCounter,
            ciphertextLen: ciphertextLen,
            totalBytes: totalBytes,
            floatCount: (totalBytes + 3) / 4
        )
    }

    private func clampedUsedSampleCount(writePos: UInt64, readPos: UInt64, capacity: Int) -> Int {
        guard capacity > 0, writePos >= readPos else { return 0 }

        let distance = writePos - readPos
        let capacity64 = UInt64(capacity)
        if distance >= capacity64 {
            return capacity
        }
        return Int(distance)
    }

    /// Write audio to shared memory (called from DoIOOperation for output)
    /// Uses lock-free ring buffer algorithm
    func writeAudio(_ buffer: UnsafePointer<Float>, frameCount: Int, channelCount: Int) -> Int {
        guard validatedMapping, let header = header, let audioData = audioData,
              frameCount > 0, frameCount <= Int(kMaximumBufferFrames),
              channelCount > 0, channelCount <= Int(kMaximumChannels) else { return 0 }
        guard tryAcquireIOCommit(header, bit: kConfiguringWriteCommit) else {
            if reconfigurationRequested(header) { atomicStore(&header.pointee.configuringAck, 1) }
            return 0
        }
        defer { releaseIOCommit(header, bit: kConfiguringWriteCommit) }
        guard atomicLoad(&header.pointee.version) == kSharedMemoryVersion,
              atomicLoad(&header.pointee.channelCount) == UInt32(channelCount),
              let audioCapacity = validatedCapacity(
                sampleRate: atomicLoad(&header.pointee.sampleRate),
                bufferFrames: atomicLoad(&header.pointee.bufferFrames),
                channelCount: UInt32(channelCount)
              ) else { return 0 }

        // Check for encryption. The encrypted path is allocation-free
        // (caller buffers + RealtimeChaChaPoly), so it is always compiled in.
        if atomicLoad(&header.pointee.encrypted) != 0 {
            return withMatchingCipher(header) { cipher in
                let sampleCount = frameCount * channelCount
                let ciphertextLen = sampleCount * MemoryLayout<Float>.size + 16
                let totalBytes = kEncryptedRecordHeaderBytes + ciphertextLen
                guard totalBytes <= encryptedPayloadScratch.count else {
                    _ = sotf_atomic_fetch_add_u64_previous(
                        &header.pointee.encryptionOverflowCount,
                        1
                    )
                    return 0
                }

                let frameCounter = sotf_atomic_fetch_add_u64(&header.pointee.frameCounter, 1)
                guard writeEncryptedRecordHeader(
                    &encryptedPayloadScratch,
                    sampleCount: sampleCount,
                    frameCounter: frameCounter,
                    ciphertextLen: ciphertextLen
                ) else {
                    return 0
                }

                let bytesWritten = encryptedPayloadScratch.withUnsafeMutableBytes { rawBuffer -> Int in
                    guard let baseAddress = rawBuffer.baseAddress?.assumingMemoryBound(to: UInt8.self) else {
                        return 0
                    }
                    return cipher.encryptToBuffer(
                        buffer,
                        sampleCount: sampleCount,
                        frameCounter: frameCounter,
                        output: baseAddress.advanced(by: kEncryptedRecordHeaderBytes),
                        plaintextScratch: &encryptedPlaintextScratch
                    )
                }

                if bytesWritten == ciphertextLen {
                    return writeRawBytes(
                        encryptedPayloadScratch,
                        byteCount: totalBytes,
                        originalFrameCount: frameCount
                    )
                }
                return 0
            }
        }

        // Standard unencrypted write
        let sampleCount = frameCount * channelCount
        let writableCapacity = audioCapacity - (audioCapacity % channelCount)
        guard sampleCount > 0, writableCapacity > 0 else { return 0 }

        let writePos = atomicLoad(&header.pointee.writePosition)
        let readPos = atomicLoad(&header.pointee.readPosition)
        guard writePos >= readPos, writePos - readPos <= UInt64(writableCapacity) else { return 0 }
        let available = writableCapacity - Int(writePos - readPos)
        // Only the consumer owns readPosition. Drop incoming frames when full;
        // overwriting unread payload would race a reader already copying it.
        let writableSamples = min(sampleCount, available)
        let samplesToWrite = writableSamples - writableSamples % channelCount
        guard samplesToWrite > 0, writePos <= UInt64.max - UInt64(samplesToWrite) else { return 0 }

        let writeIndex = Int(writePos % UInt64(audioCapacity))
        let firstPart = min(samplesToWrite, audioCapacity - writeIndex)
        let secondPart = samplesToWrite - firstPart

        // Copy data
        memcpy(audioData.advanced(by: writeIndex), buffer, firstPart * MemoryLayout<Float>.size)
        if secondPart > 0 {
            memcpy(audioData, buffer.advanced(by: firstPart), secondPart * MemoryLayout<Float>.size)
        }
        if atomicLoad(&header.pointee.active) == 0 {
            atomicStore(&header.pointee.active, 1)
        }
        atomicStore(&header.pointee.writePosition, writePos + UInt64(samplesToWrite))

        // TRACE: Log successful write to shared memory ring buffer
        // Note: Avoid logging every frame in production (use os_signpost for performance tracing)
        #if SOTF_AUDIO_TRACE
        if samplesToWrite > 0 {
            let newWritePos = writePos + UInt64(samplesToWrite)
            halLog("[SHM TRACE] write: \(samplesToWrite/channelCount) frames, wpos=\(newWritePos), rpos=\(readPos)")
        }
        #endif

        return samplesToWrite / channelCount
    }

    /// Helper to write raw bytes (ciphertext) to ring buffer
    private func writeRawBytes(_ bytes: [UInt8], byteCount: Int, originalFrameCount: Int) -> Int {
        guard let header = header, let audioData = audioData else { return 0 }
        guard atomicLoad(&header.pointee.configuring) & kConfiguringWriteCommit != 0,
              let audioCapacity = validatedCapacity(
                sampleRate: atomicLoad(&header.pointee.sampleRate),
                bufferFrames: atomicLoad(&header.pointee.bufferFrames),
                channelCount: atomicLoad(&header.pointee.channelCount)
              ) else { return 0 }
        if reconfigurationRequested(header) {
            atomicStore(&header.pointee.configuringAck, 1)
            return 0
        }
        
        // Calculate required space in floats (rounded up)
        let floatCount = (byteCount + 3) / 4
        guard floatCount > 0, audioCapacity > 0 else { return 0 }

        if floatCount > audioCapacity {
            _ = sotf_atomic_fetch_add_u64_previous(&header.pointee.encryptionOverflowCount, 1)
            return 0
        }
        
        let writePos = atomicLoad(&header.pointee.writePosition)
        let readPos = atomicLoad(&header.pointee.readPosition)
        guard writePos >= readPos, writePos - readPos <= UInt64(audioCapacity),
              writePos <= UInt64.max - UInt64(floatCount) else { return 0 }
        let used = clampedUsedSampleCount(
            writePos: writePos,
            readPos: readPos,
            capacity: audioCapacity
        )
        let available = audioCapacity - used

        // Overflow policy: drop ONLY the incoming record. The old behavior
        // wrote anyway and reset readPosition = writePosition, discarding
        // the entire queued backlog on every overflow — at 96 kHz records
        // arrive twice as often, so one decoder stall destroyed ~85 ms of
        // audio instead of a single block.
        if floatCount > available {
            _ = sotf_atomic_fetch_add_u64_previous(&header.pointee.encryptionOverflowCount, 1)
            return 0
        }

        let writeIndex = Int(writePos % UInt64(audioCapacity))
        let firstPartFloats = min(floatCount, audioCapacity - writeIndex)
        let firstPartBytes = firstPartFloats * 4
        
        let bytesToWriteFirst = min(byteCount, firstPartBytes)
        let bytesToWriteSecond = byteCount - bytesToWriteFirst
        
        bytes.withUnsafeBufferPointer { ptr in
            guard let base = ptr.baseAddress else { return }
            
            // First part
            let destFirst = UnsafeMutableRawPointer(audioData.advanced(by: writeIndex))
            memcpy(destFirst, base, bytesToWriteFirst)
            
            // Second part (wrap)
            if bytesToWriteSecond > 0 {
                let destSecond = UnsafeMutableRawPointer(audioData) // Start of buffer
                memcpy(destSecond, base.advanced(by: bytesToWriteFirst), bytesToWriteSecond)
            }
        }
        
        if atomicLoad(&header.pointee.active) == 0 {
            atomicStore(&header.pointee.active, 1)
        }
        atomicStore(&header.pointee.writePosition, writePos + UInt64(floatCount))
        
        return originalFrameCount
    }

    /// Read audio from shared memory (called from DoIOOperation for input)
    /// Uses lock-free ring buffer algorithm
    func readAudio(_ buffer: UnsafeMutablePointer<Float>, frameCount: Int, channelCount: Int) -> Int {
        guard frameCount > 0, frameCount <= Int(kMaximumBufferFrames),
              channelCount > 0, channelCount <= Int(kMaximumChannels) else { return 0 }
        guard validatedMapping, let header = header, let audioData = audioData else {
            // Fill with silence
            memset(buffer, 0, frameCount * channelCount * MemoryLayout<Float>.size)
            return 0
        }
        guard tryAcquireIOCommit(header, bit: kConfiguringReadCommit) else {
            if reconfigurationRequested(header) { atomicStore(&header.pointee.configuringAck, 1) }
            memset(buffer, 0, frameCount * channelCount * MemoryLayout<Float>.size)
            return 0
        }
        defer { releaseIOCommit(header, bit: kConfiguringReadCommit) }
        guard atomicLoad(&header.pointee.version) == kSharedMemoryVersion,
              atomicLoad(&header.pointee.channelCount) == UInt32(channelCount),
              let audioCapacity = validatedCapacity(
                sampleRate: atomicLoad(&header.pointee.sampleRate),
                bufferFrames: atomicLoad(&header.pointee.bufferFrames),
                channelCount: UInt32(channelCount)
              ) else {
            memset(buffer, 0, frameCount * channelCount * MemoryLayout<Float>.size)
            return 0
        }
        let currentWrite = atomicLoad(&header.pointee.writePosition)
        let currentRead = atomicLoad(&header.pointee.readPosition)
        guard currentWrite >= currentRead, currentWrite - currentRead <= UInt64(audioCapacity) else {
            memset(buffer, 0, frameCount * channelCount * MemoryLayout<Float>.size)
            return 0
        }

        // Check for encryption. The encrypted path is allocation-free
        // (caller buffers + RealtimeChaChaPoly), so it is always compiled in.
        if atomicLoad(&header.pointee.encrypted) != 0 {
            // Silence is the default even if no matching key is published.
            memset(buffer, 0, frameCount * channelCount * MemoryLayout<Float>.size)
            return withMatchingCipher(header) { cipher in
                let requestedSampleCount = frameCount * channelCount
                let writePos = currentWrite
                let readPos = currentRead
                let available = writePos >= readPos ? Int(writePos - readPos) : 0

                if available >= kEncryptedRecordHeaderFloats {
                    copyRawBytes(
                        at: readPos,
                        into: &encryptedHeaderScratch,
                        byteCount: kEncryptedRecordHeaderBytes,
                        floatCount: kEncryptedRecordHeaderFloats
                    )

                    guard let record = parseEncryptedRecordHeader(encryptedHeaderScratch) else {
                        _ = commitReadPosition(header, writePos)
                        memset(buffer, 0, requestedSampleCount * MemoryLayout<Float>.size)
                        return 0
                    }

                    if record.floatCount <= available && record.sampleCount <= requestedSampleCount {
                        guard record.totalBytes <= encryptedPayloadScratch.count else {
                            _ = commitReadPosition(header, readPos + UInt64(record.floatCount))
                            memset(buffer, 0, requestedSampleCount * MemoryLayout<Float>.size)
                            return 0
                        }
                        copyRawBytes(
                            at: readPos,
                            into: &encryptedPayloadScratch,
                            byteCount: record.totalBytes,
                            floatCount: record.floatCount
                        )

                        let decryptedCount = encryptedPayloadScratch.withUnsafeBufferPointer { ptr in
                            guard let base = ptr.baseAddress else { return 0 }
                            return cipher.decryptFromBuffer(
                                base.advanced(by: kEncryptedRecordHeaderBytes),
                                ciphertextLen: record.ciphertextLen,
                                frameCounter: record.frameCounter,
                                output: buffer
                            )
                        }

                        if reconfigurationRequested(header) {
                            atomicStore(&header.pointee.configuringAck, 1)
                            memset(buffer, 0, requestedSampleCount * MemoryLayout<Float>.size)
                            return 0
                        }
                        guard commitReadPosition(header, readPos + UInt64(record.floatCount)) else {
                            memset(buffer, 0, requestedSampleCount * MemoryLayout<Float>.size)
                            return 0
                        }

                        if decryptedCount > 0 {
                            if decryptedCount < requestedSampleCount {
                                memset(
                                    buffer.advanced(by: decryptedCount),
                                    0,
                                    (requestedSampleCount - decryptedCount) * MemoryLayout<Float>.size
                                )
                            }
                            return decryptedCount / channelCount
                        }
                    }
                }
                return 0
            }
        }

        // Standard unencrypted read
        let sampleCount = frameCount * channelCount
        let writePos = currentWrite
        let readPos = currentRead

        // Calculate available data
        let available = writePos >= readPos ? Int(writePos - readPos) : 0
        // The ring stores interleaved frames. Never publish a read cursor
        // between channels when CoreAudio asks for fewer samples than are
        // currently available.
        let readableSamples = min(sampleCount, available)
        let toRead = readableSamples - (readableSamples % channelCount)

        if toRead <= 0 {
            // Fill with silence
            memset(buffer, 0, frameCount * channelCount * MemoryLayout<Float>.size)
            return 0
        }

        let readIndex = Int(readPos % UInt64(audioCapacity))
        let firstPart = min(toRead, audioCapacity - readIndex)
        let secondPart = toRead - firstPart

        // Copy data
        memcpy(buffer, audioData.advanced(by: readIndex), firstPart * MemoryLayout<Float>.size)
        if secondPart > 0 {
            memcpy(buffer.advanced(by: firstPart), audioData, secondPart * MemoryLayout<Float>.size)
        }

        // Fill remaining with silence
        if toRead < sampleCount {
            memset(buffer.advanced(by: toRead), 0, (sampleCount - toRead) * MemoryLayout<Float>.size)
        }

        if reconfigurationRequested(header) {
            atomicStore(&header.pointee.configuringAck, 1)
            memset(buffer, 0, frameCount * channelCount * MemoryLayout<Float>.size)
            return 0
        }
        guard commitReadPosition(header, readPos + UInt64(toRead)) else {
            memset(buffer, 0, frameCount * channelCount * MemoryLayout<Float>.size)
            return 0
        }

        // TRACE: Log successful read from shared memory ring buffer
        #if SOTF_AUDIO_TRACE
        if toRead > 0 {
            let newReadPos = readPos + UInt64(toRead)
            halLog("[SHM TRACE] read: \(toRead/channelCount) frames, wpos=\(writePos), rpos=\(newReadPos)")
        }
        #endif

        return toRead / channelCount
    }

    /// Helper to read raw bytes (ciphertext) from ring buffer
    private func copyRawBytes(at position: UInt64, into buffer: inout [UInt8], byteCount: Int, floatCount: Int) {
        guard let audioData = audioData, let header,
              let audioCapacity = validatedCapacity(
                sampleRate: atomicLoad(&header.pointee.sampleRate),
                bufferFrames: atomicLoad(&header.pointee.bufferFrames),
                channelCount: atomicLoad(&header.pointee.channelCount)
              ), byteCount <= buffer.count, floatCount <= audioCapacity else { return }

        let readIndex = Int(position % UInt64(audioCapacity))
        let firstPartFloats = min(floatCount, audioCapacity - readIndex)
        let firstPartBytes = firstPartFloats * MemoryLayout<Float>.size

        let bytesToReadFirst = min(byteCount, firstPartBytes)
        let bytesToReadSecond = byteCount - bytesToReadFirst

        buffer.withUnsafeMutableBufferPointer { ptr in
            guard let base = ptr.baseAddress else { return }

            let srcFirst = UnsafeRawPointer(audioData.advanced(by: readIndex))
            memcpy(base, srcFirst, bytesToReadFirst)

            if bytesToReadSecond > 0 {
                let srcSecond = UnsafeRawPointer(audioData)
                memcpy(base.advanced(by: bytesToReadFirst), srcSecond, bytesToReadSecond)
            }
        }
    }

    // MARK: - Config Negotiation Methods (version 3+)

    /// Check if configuration change is pending
    func configChanged() -> Bool {
        guard let header = header else { return false }
        return atomicLoad(&header.pointee.configChanged) != 0
    }

    /// Get config source (1=HAL initiated, 2=Daemon initiated)
    func configSource() -> UInt32 {
        guard let header = header else { return 0 }
        return atomicLoad(&header.pointee.configSource)
    }

    /// Get actual sample rate (set by responder after negotiation)
    ///
    /// Caller should check `getConfigStatus()` first, which performs a memory barrier.
    func getActualSampleRate() -> UInt32 {
        guard let header = header else { return 0 }
        return atomicLoad(&header.pointee.actualSampleRate)
    }

    /// Get actual buffer frames (set by responder after negotiation)
    ///
    /// Caller should check `getConfigStatus()` first, which performs a memory barrier.
    func getActualBufferFrames() -> UInt32 {
        guard let header = header else { return 0 }
        return atomicLoad(&header.pointee.actualBufferFrames)
    }

    /// Get config status (0=pending, 1=accepted, 2=negotiated, 3=error)
    ///
    /// This function includes a memory barrier to ensure visibility of
    /// the status and all related config values from the responder.
    func getConfigStatus() -> UInt32 {
        guard let header = header else { return 0 }
        return atomicLoad(&header.pointee.configStatus)
    }

    /// Get config error code (only valid when configStatus=3)
    ///
    /// Caller should check `getConfigStatus()` first, which performs a memory barrier.
    func getConfigErrorCode() -> UInt32 {
        guard let header = header else { return 0 }
        return atomicLoad(&header.pointee.configErrorCode)
    }

    /// Get requested sample rate (set by the config requester)
    func getRequestedSampleRate() -> UInt32 {
        guard let header = header else { return 0 }
        return atomicLoad(&header.pointee.requestedSampleRate)
    }

    /// Get requested buffer frames (set by the config requester)
    func getRequestedBufferFrames() -> UInt32 {
        guard let header = header else { return 0 }
        return atomicLoad(&header.pointee.requestedBufferFrames)
    }

    /// Get the pending channel count without changing live ring geometry.
    func getRequestedChannelCount() -> UInt32 {
        guard let header = header else { return 0 }
        return atomicLoad(&header.pointee.requestedChannelCount)
    }

    /// Control-plane publication only: audio IO never takes this bounded gate.
    private func withControlLock<T>(_ body: (UnsafeMutablePointer<SharedAudioHeader>) -> T) -> T? {
        guard let header else { return nil }
        for _ in 0..<128 {
            if sotf_atomic_compare_exchange_u32(&header.pointee.controlLock, 0, 1) {
                defer { atomicStore(&header.pointee.controlLock, 0) }
                return body(header)
            }
        }
        return nil
    }

    func configurationSnapshot() -> SharedConfigurationSnapshot? {
        withControlLock { header in
            let id = atomicLoad(&header.pointee.configRequestID)
            let acknowledged = atomicLoad(&header.pointee.configAckID) == id
            return SharedConfigurationSnapshot(
                requestID: id,
                sampleRate: atomicLoad(&header.pointee.requestedSampleRate),
                bufferFrames: atomicLoad(&header.pointee.requestedBufferFrames),
                channelCount: atomicLoad(&header.pointee.requestedChannelCount),
                source: atomicLoad(&header.pointee.configSource),
                actualSampleRate: atomicLoad(&header.pointee.actualSampleRate),
                actualBufferFrames: atomicLoad(&header.pointee.actualBufferFrames),
                actualChannelCount: atomicLoad(&header.pointee.actualChannelCount),
                status: acknowledged ? atomicLoad(&header.pointee.configStatus) : 0,
                errorCode: acknowledged ? atomicLoad(&header.pointee.configErrorCode) : 0,
                pending: atomicLoad(&header.pointee.configChanged) != 0 && !acknowledged
            )
        }
    }

    @discardableResult
    func requestConfigChange(sampleRate: UInt32, bufferFrames: UInt32, channelCount: UInt32) -> Bool {
        requestConfigChangeWithID(sampleRate: sampleRate, bufferFrames: bufferFrames, channelCount: channelCount) != nil
    }

    func requestConfigChangeWithID(sampleRate: UInt32, bufferFrames: UInt32, channelCount: UInt32) -> UInt64? {
        guard let header, !reconfigurationRequested(header) else { return nil }
        return withControlLock { header -> UInt64? in
            let previous = atomicLoad(&header.pointee.configRequestID)
            guard previous != UInt64.max else { return nil }
            let id = previous + 1
            atomicStore(&header.pointee.requestedSampleRate, sampleRate)
            atomicStore(&header.pointee.requestedBufferFrames, bufferFrames)
            atomicStore(&header.pointee.requestedChannelCount, channelCount)
            atomicStore(&header.pointee.configStatus, 0)
            atomicStore(&header.pointee.configErrorCode, 0)
            atomicStore(&header.pointee.configSource, 1)
            atomicStore(&header.pointee.configChanged, 1)
            atomicStore(&header.pointee.configRequestID, id)
            return id
        } ?? nil
    }

    /// Publish an ACK only for the retained request identity.
    @discardableResult
    func acknowledgeConfigChange(requestID: UInt64, actualSampleRate: UInt32, actualBufferFrames: UInt32,
                                 actualChannelCount: UInt32, status: UInt32, errorCode: UInt32) -> Bool {
        withControlLock { header in
            guard requestID != 0, atomicLoad(&header.pointee.configRequestID) == requestID else { return false }
            atomicStore(&header.pointee.actualSampleRate, actualSampleRate)
            atomicStore(&header.pointee.actualBufferFrames, actualBufferFrames)
            atomicStore(&header.pointee.actualChannelCount, actualChannelCount)
            atomicStore(&header.pointee.configErrorCode, errorCode)
            atomicStore(&header.pointee.configStatus, status)
            atomicStore(&header.pointee.configAckID, requestID)
            atomicStore(&header.pointee.configChanged, 0)
            return true
        } ?? false
    }

    /// Atomically observe scalar, mute, and generation from one publication.
    func outputVolume() -> (scalarBits: UInt32, muted: Bool, generation: UInt32) {
        guard let header else { return (0, false, 0) }
        let state = atomicLoad(&header.pointee.volumeState)
        return (UInt32(truncatingIfNeeded: state), state & (1 << 32) != 0, UInt32(state >> 33))
    }

    /// Patch independent controls without overwriting another writer's mute/volume intent.
    func patchOutputVolume(scalarBits: UInt32?, muted: Bool?) -> (scalarBits: UInt32, muted: Bool, generation: UInt32)? {
        guard let header else { return nil }
        while true {
            let old = atomicLoad(&header.pointee.volumeState)
            let previousGeneration = UInt32(old >> 33)
            let oldBits = previousGeneration == 0 ? Float32(1).bitPattern : UInt32(truncatingIfNeeded: old)
            let value = Float32(bitPattern: scalarBits ?? oldBits)
            let bits = (value.isFinite ? min(max(value, 0), 1) : 1).bitPattern
            let mute = muted ?? (old & (1 << 32) != 0)
            var generation = (previousGeneration &+ 1) & 0x7fff_ffff
            if generation == 0 { generation = 1 }
            let next = UInt64(bits) | (UInt64(mute ? 1 : 0) << 32) | (UInt64(generation) << 33)
            if sotf_atomic_compare_exchange_u64(&header.pointee.volumeState, old, next) {
                return (bits, mute, generation)
            }
        }
    }

    @discardableResult
    func setOutputVolume(scalarBits: UInt32, muted: Bool) -> UInt32 {
        patchOutputVolume(scalarBits: scalarBits, muted: muted)?.generation ?? 0
    }

    deinit {
        closeSharedMemory()
    }
}
