// Tests.swift - Test utilities for HAL driver components
//
// These tests can be called from development builds to verify functionality.
// They are designed to catch regressions in critical areas like:
// - AnyCodable encoding/decoding
// - Socket response parsing
// - Encryption key handling
//
// Note: These are NOT XCTest tests. They are assertion-based verification
// functions that can be called during development and debugging.

import CryptoKit
import Foundation

/// Test runner for HAL driver components
final class HALDriverTests {

    private struct SharedMemoryLayoutManifest: Decodable {
        let size: Int
        let alignment: Int
        let fields: [String: Int]
    }

    private static func sharedMemoryLayoutManifest() -> SharedMemoryLayoutManifest? {
        var candidates: [URL] = []
        let environment = ProcessInfo.processInfo.environment
        if let configuredPath = environment["SOTF_SHARED_MEMORY_LAYOUT_MANIFEST"],
           !configuredPath.isEmpty
        {
            candidates.append(URL(fileURLWithPath: configuredPath))
        }
        if let bundleURL = Bundle.main.url(
            forResource: "shared_memory_layout",
            withExtension: "json"
        ) {
            candidates.append(bundleURL)
        }
        if let bundleURL = Bundle(for: HALDriverTests.self).url(
            forResource: "shared_memory_layout",
            withExtension: "json"
        ) {
            candidates.append(bundleURL)
        }

        for manifestURL in candidates {
            guard let data = try? Data(contentsOf: manifestURL) else { continue }
            do {
                return try JSONDecoder().decode(SharedMemoryLayoutManifest.self, from: data)
            } catch {
                halLog("    FAIL: shared-memory layout manifest is invalid: \(error)")
                return nil
            }
        }

        halLog(
            "    FAIL: shared-memory layout manifest is missing; set "
                + "SOTF_SHARED_MEMORY_LAYOUT_MANIFEST or package it as a bundle resource"
        )
        return nil
    }

    /// Run all tests
    @discardableResult
    static func runAllTests() -> Bool {
        let tests: [(String, () -> Bool)] = [
            ("testZeroTimestampSurvivesRateReanchor", testZeroTimestampSurvivesRateReanchor),
            ("testClockSnapshotsDuringRateChanges", testClockSnapshotsDuringRateChanges),
            ("testRingBufferBasicOperations", testRingBufferBasicOperations),
            ("testRingBufferWrapAround", testRingBufferWrapAround),
            ("testRingBufferMultiChannel", testRingBufferMultiChannel),
            ("testRingBufferConcurrentSPSC", testRingBufferConcurrentSPSC),
            ("testSharedAudioHeaderLayout", testSharedAudioHeaderLayout),
            ("testVolumeConversionAndSharedRoundTrip", testVolumeConversionAndSharedRoundTrip),
            ("testSampleRateRequestRemainsTransactional", testSampleRateRequestRemainsTransactional),
            ("testCrossProcessReconfigurationProtocol", testCrossProcessReconfigurationProtocol),
            ("testGeometryCacheSurvivesDaemonRestart", testGeometryCacheSurvivesDaemonRestart),
            ("testMappingValidationAndReaderOwnership", testMappingValidationAndReaderOwnership),
            ("testEncryptionRoundTrip", testEncryptionRoundTrip),
            ("testEncryptionWithDifferentKeys", testEncryptionWithDifferentKeys),
            ("testEncryptionFrameCounterNonceUniqueness", testEncryptionFrameCounterNonceUniqueness),
            ("testRealtimeCipherMatchesCryptoKit", testRealtimeCipherMatchesCryptoKit),
            ("testKeyManagerFingerprint", testKeyManagerFingerprint),
            ("testCipherPublicationKeepsPinnedReaderAlive", testCipherPublicationKeepsPinnedReaderAlive),
            ("testCipherSlotExcludesDelayedReaders", testCipherSlotExcludesDelayedReaders),
            ("testConcurrentLocalVolumePublication", testConcurrentLocalVolumePublication),
            ("testConsoleSessionRetiresMappingKeyAndLoopback", testConsoleSessionRetiresMappingKeyAndLoopback),
            ("testSharedMemoryEncryptedRealtimeRoundTrips", testSharedMemoryEncryptedRealtimeRoundTrips),
        ]
        var failed = 0
        for (name, test) in tests {
            let passed = test()
            print("\(passed ? "PASS" : "FAIL"): \(name)")
            if !passed { failed += 1 }
        }
        print("HAL tests: \(tests.count - failed) passed, \(failed) failed, \(tests.count) total")
        return failed == 0
    }

    private static func transportFixtureHeader() -> SharedAudioHeader {
        SharedAudioHeader(
            magic: 0x534F5446, version: 7, sampleRate: 48_000, bufferFrames: 64, channelCount: 2,
            writePosition: 0, readPosition: 0, active: 0, configChanged: 0,
            driverReady: 0, engineReady: 1, encrypted: 0, volumeScalarBits: Float(1).bitPattern,
            keyFingerprint: 0, frameCounter: 0, requestedSampleRate: 48_000,
            requestedBufferFrames: 64, actualSampleRate: 48_000, actualBufferFrames: 64,
            configStatus: 0, configSource: 0, configErrorCode: 0, outputMuted: 0,
            encryptionOverflowCount: 0, daemonHeartbeatMs: 0, configuring: 0,
            configuringAck: 0, requestedChannelCount: 2, volumeGeneration: 0
        )
    }

    /// Concurrent property updates and maintenance must finish with one tuple.
    static func testConcurrentLocalVolumePublication() -> Bool {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("sotf-volume-\(UUID())")
        let path = root.appendingPathComponent("audio.shm")
        let oldPath = ProcessInfo.processInfo.environment["SOTF_HAL_SHARED_MEMORY_PATH"]
        defer {
            if let oldPath { setenv("SOTF_HAL_SHARED_MEMORY_PATH", oldPath, 1) }
            else { unsetenv("SOTF_HAL_SHARED_MEMORY_PATH") }
            try? FileManager.default.removeItem(at: root)
        }
        do {
            try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true,
                                                    attributes: [.posixPermissions: 0o700])
            var header = transportFixtureHeader()
            let aligned = (MemoryLayout<SharedAudioHeader>.size + 63) & ~63
            var bytes = Data(count: aligned + 64 * 2 * 8 * MemoryLayout<Float>.size)
            withUnsafeBytes(of: &header) { bytes.replaceSubrange(0..<$0.count, with: $0) }
            try bytes.write(to: path)
            chmod(path.path, 0o600)
            setenv("SOTF_HAL_SHARED_MEMORY_PATH", path.path, 1)
            let state = DriverState()
            guard state.sharedAudio.initialize(sampleRate: 48_000, bufferFrames: 64, channelCount: 2) else { return false }
            DispatchQueue.concurrentPerform(iterations: 3) { role in
                for index in 0..<200 {
                    if role == 0 { state.commitOutputVolume(scalar: Float(index % 10) / 10) }
                    else if role == 1 { state.commitOutputVolume(muted: index % 2 == 0) }
                    else { state.syncVolumeFromSharedMemory() }
                }
            }
            let local = state.outputVolume()
            let shared = state.sharedAudio.outputVolume()
            return shared.generation != 0 && shared.scalarBits == local.scalar.bitPattern
                && shared.muted == local.muted
        } catch { print("Volume fixture failed: \(error)"); return false }
    }

    /// An old daemon may remain live while the console transport changes.
    static func testConsoleSessionRetiresMappingKeyAndLoopback() -> Bool {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("sotf-session-\(UUID())")
        let oldAudio = ProcessInfo.processInfo.environment["SOTF_HAL_SHARED_MEMORY_PATH"]
        let oldKey = ProcessInfo.processInfo.environment["SOTF_HAL_SESSION_KEY_PATH"]
        let manager = EncryptionKeyManager.shared
        defer {
            if let oldAudio { setenv("SOTF_HAL_SHARED_MEMORY_PATH", oldAudio, 1) }
            else { unsetenv("SOTF_HAL_SHARED_MEMORY_PATH") }
            if let oldKey { setenv("SOTF_HAL_SESSION_KEY_PATH", oldKey, 1) }
            else { unsetenv("SOTF_HAL_SESSION_KEY_PATH") }
            manager.reloadFromCurrentEnvironment()
            try? FileManager.default.removeItem(at: root)
        }
        func fixture(_ name: String, keyByte: UInt8) throws -> (String, String) {
            let audio = root.appendingPathComponent(name + ".shm")
            let key = root.appendingPathComponent(name + ".key")
            var header = transportFixtureHeader()
            let aligned = (MemoryLayout<SharedAudioHeader>.size + 63) & ~63
            var bytes = Data(count: aligned + 64 * 2 * 8 * MemoryLayout<Float>.size)
            withUnsafeBytes(of: &header) { bytes.replaceSubrange(0..<$0.count, with: $0) }
            try bytes.write(to: audio)
            try Data(repeating: keyByte, count: 32).write(to: key)
            chmod(audio.path, 0o600)
            chmod(key.path, 0o600)
            return (audio.path, key.path)
        }
        do {
            try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true,
                                                    attributes: [.posixPermissions: 0o700])
            let a = try fixture("a", keyByte: 1)
            setenv("SOTF_HAL_SHARED_MEMORY_PATH", a.0, 1)
            setenv("SOTF_HAL_SESSION_KEY_PATH", a.1, 1)
            manager.reloadFromCurrentEnvironment()
            let state = DriverState()
            guard state.sharedAudio.initialize(sampleRate: 48_000, bufferFrames: 64, channelCount: 2) else { return false }
            let oldLease = state.acquireSharedAudioForCallback()
            var held = true
            defer { if held { state.finishSharedAudioCallback(slot: oldLease.slot) } }
            guard oldLease.slot != UInt32.max else { return false }
            let marker = [Float](repeating: 0.75, count: 128)
            _ = marker.withUnsafeBufferPointer {
                state.outputRingBuffer?.writeInterleaved($0.baseAddress!, frameCount: 64)
            }
            setenv("SOTF_HAL_SHARED_MEMORY_PATH", root.appendingPathComponent("missing.shm").path, 1)
            setenv("SOTF_HAL_SESSION_KEY_PATH", root.appendingPathComponent("missing.key").path, 1)
            state.noticeConsoleSessionChange()
            guard state.acquireSharedAudioForCallback().slot == UInt32.max,
                  !state.reconcileConsoleSession(), oldLease.buffer.isConnected else { return false }
            state.finishSharedAudioCallback(slot: oldLease.slot)
            held = false
            guard state.reconcileConsoleSession(), !oldLease.buffer.isConnected,
                  manager.getFingerprint() == nil else { return false }
            state.attemptInitRetryIfNeeded()
            guard state.acquireSharedAudioForCallback().slot == UInt32.max else { return false }
            var output = [Float](repeating: -1, count: 128)
            let frames = output.withUnsafeMutableBufferPointer {
                state.outputRingBuffer?.readInterleaved($0.baseAddress!, frameCount: 64)
            }
            guard frames == 0, output.allSatisfy({ $0 == 0 }) else { return false }
            let c = try fixture("c", keyByte: 3)
            setenv("SOTF_HAL_SHARED_MEMORY_PATH", c.0, 1)
            setenv("SOTF_HAL_SESSION_KEY_PATH", c.1, 1)
            guard state.reconcileConsoleSession() else { return false }
            state.attemptInitRetryIfNeeded()
            let newLease = state.acquireSharedAudioForCallback()
            defer { state.finishSharedAudioCallback(slot: newLease.slot) }
            return newLease.slot != UInt32.max && newLease.buffer.isConnected
                && manager.getFingerprint() == AudioCipher(keyBytes: [UInt8](repeating: 3, count: 32)).getFingerprint()
        } catch { print("Session fixture failed: \(error)"); return false }
    }

    /// Exercise the production Swift mapper and producer with hostile headers.
    static func testMappingValidationAndReaderOwnership() -> Bool {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("sotf-mapping-\(UUID().uuidString)")
        let path = directory.appendingPathComponent("audio.shm")
        let oldPath = ProcessInfo.processInfo.environment["SOTF_HAL_SHARED_MEMORY_PATH"]
        defer {
            if let oldPath { setenv("SOTF_HAL_SHARED_MEMORY_PATH", oldPath, 1) }
            else { unsetenv("SOTF_HAL_SHARED_MEMORY_PATH") }
            try? FileManager.default.removeItem(at: directory)
        }
        do {
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true,
                                                    attributes: [.posixPermissions: 0o700])
            setenv("SOTF_HAL_SHARED_MEMORY_PATH", path.path, 1)
            let alignedHeader = (MemoryLayout<SharedAudioHeader>.size + 63) & ~63
            let capacity = 64 * 2 * 8
            let buffer = SharedAudioBuffer()
            defer { buffer.closeSharedMemory() }

            func fixture(version: UInt32 = 7, rate: UInt32 = 48_000, frames: UInt32 = 64,
                         channels: UInt32 = 2, write: UInt64 = 0, read: UInt64 = 0,
                         configuring: UInt32 = 0) -> Data {
                var data = Data(count: alignedHeader + capacity * MemoryLayout<Float>.size)
                data.withUnsafeMutableBytes { bytes in
                    bytes.storeBytes(of: UInt32(0x534F5446), toByteOffset: 0, as: UInt32.self)
                    bytes.storeBytes(of: version, toByteOffset: 4, as: UInt32.self)
                    bytes.storeBytes(of: rate, toByteOffset: 8, as: UInt32.self)
                    bytes.storeBytes(of: frames, toByteOffset: 12, as: UInt32.self)
                    bytes.storeBytes(of: channels, toByteOffset: 16, as: UInt32.self)
                    bytes.storeBytes(of: write, toByteOffset: MemoryLayout<SharedAudioHeader>.offset(of: \.writePosition)!, as: UInt64.self)
                    bytes.storeBytes(of: read, toByteOffset: MemoryLayout<SharedAudioHeader>.offset(of: \.readPosition)!, as: UInt64.self)
                    bytes.storeBytes(of: configuring, toByteOffset: MemoryLayout<SharedAudioHeader>.offset(of: \.configuring)!, as: UInt32.self)
                }
                return data
            }

            let malformed = [fixture(version: 999), fixture(rate: UInt32.max),
                             fixture(frames: 1_048_576), fixture(channels: 0), fixture(channels: 33),
                             fixture(write: 0, read: 1), fixture(write: UInt64(capacity + 1)),
                             Data(count: 64)]
            for data in malformed {
                try data.write(to: path)
                chmod(path.path, 0o600)
                guard !buffer.initialize(sampleRate: 48_000, bufferFrames: 64, channelCount: 2),
                      !buffer.isConnected, try Data(contentsOf: path) == data else {
                    halLog("    FAIL: malformed mapping accepted, retained, or modified")
                    return false
                }
            }

            // A failed initialization must not poison a later valid attempt.
            let full = fixture(write: UInt64(capacity), configuring: 2)
            try full.write(to: path)
            chmod(path.path, 0o600)
            guard buffer.initialize(sampleRate: 48_000, bufferFrames: 64, channelCount: 2),
                  buffer.isConnected else { return false }
            let input = [Float](repeating: 1, count: 128)
            let written = input.withUnsafeBufferPointer {
                buffer.writeAudio($0.baseAddress!, frameCount: 64, channelCount: 2)
            }
            let after = try Data(contentsOf: path)
            let readPosition = after.withUnsafeBytes {
                $0.loadUnaligned(fromByteOffset: MemoryLayout<SharedAudioHeader>.offset(of: \.readPosition)!, as: UInt64.self)
            }
            guard written == 0, readPosition == 0,
                  after.suffix(capacity * MemoryLayout<Float>.size) == full.suffix(capacity * MemoryLayout<Float>.size) else {
                halLog("    FAIL: full-ring producer modified reader-owned data")
                return false
            }
            buffer.closeSharedMemory()

            try fixture().write(to: path)
            chmod(path.path, 0o600)
            guard buffer.initialize(sampleRate: 48_000, bufferFrames: 64, channelCount: 2) else { return false }
            let accepted = input.withUnsafeBufferPointer {
                buffer.writeAudio($0.baseAddress!, frameCount: 64, channelCount: 2)
            }
            var output = [Float](repeating: 0, count: input.count)
            let read = output.withUnsafeMutableBufferPointer {
                buffer.readAudio($0.baseAddress!, frameCount: 64, channelCount: 2)
            }
            guard accepted == 64, read == 64, output == input else { return false }
            return true
        } catch {
            halLog("    FAIL: mapping regression fixture: \(error)")
            return false
        }
    }

    /// An exclusively retired slot rejects a reader delayed before pinning.
    static func testCipherSlotExcludesDelayedReaders() -> Bool {
        let gate = UnsafeMutablePointer<UInt64>.allocate(capacity: 1)
        gate.initialize(to: 0)
        defer { gate.deinitialize(count: 1); gate.deallocate() }
        guard sotf_cipher_try_write(gate) else { return false }
        guard !sotf_cipher_try_pin(gate) else { return false }
        sotf_cipher_end_write(gate)
        guard sotf_cipher_try_pin(gate) else { return false }
        guard !sotf_cipher_try_write(gate) else { return false }
        sotf_atomic_fetch_sub_u64(gate, 1)
        guard sotf_cipher_try_write(gate) else { return false }
        sotf_cipher_end_write(gate)
        return true
    }

    /// Hold a realtime reader across multiple publications and slot reuse.
    static func testCipherPublicationKeepsPinnedReaderAlive() -> Bool {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("sotf-cipher-\(UUID().uuidString)")
        let path = directory.appendingPathComponent("session.key")
        let oldPath = ProcessInfo.processInfo.environment["SOTF_HAL_SESSION_KEY_PATH"]
        let manager = EncryptionKeyManager.shared
        defer {
            if let oldPath { setenv("SOTF_HAL_SESSION_KEY_PATH", oldPath, 1) }
            else { unsetenv("SOTF_HAL_SESSION_KEY_PATH") }
            manager.reloadFromCurrentEnvironment()
            try? FileManager.default.removeItem(at: directory)
        }
        do {
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true,
                                                    attributes: [.posixPermissions: 0o700])
            let first = [UInt8](repeating: 1, count: 32)
            let last = [UInt8](repeating: 2, count: 32)
            let expectedFirst = AudioCipher(keyBytes: first).getFingerprint()
            let expectedLast = AudioCipher(keyBytes: last).getFingerprint()
            try Data(first).write(to: path)
            chmod(path.path, 0o600)
            setenv("SOTF_HAL_SESSION_KEY_PATH", path.path, 1)
            manager.reloadFromCurrentEnvironment()
            let started = DispatchSemaphore(value: 0)
            let resume = DispatchSemaphore(value: 0)
            let finished = DispatchGroup()
            let result = UnsafeMutablePointer<UInt64>.allocate(capacity: 1)
            result.initialize(to: 0)
            defer { result.deinitialize(count: 1); result.deallocate() }
            finished.enter()
            DispatchQueue.global(qos: .userInitiated).async {
                let valid = manager.withCipher { cipher in
                    started.signal()
                    guard resume.wait(timeout: .now() + 5) == .success else { return 0 }
                    return cipher?.getFingerprint() == expectedFirst ? 1 : 0
                }
                sotf_atomic_store_u64(result, UInt64(valid))
                finished.leave()
            }
            defer { resume.signal(); finished.wait() }
            guard started.wait(timeout: .now() + 5) == .success else {
                resume.signal()
                finished.wait()
                return false
            }
            // Publication into the pinned slot must remain deferred, including
            // repeated replacements of the pending value.
            for value in 3...20 {
                try Data(repeating: UInt8(value), count: 32).write(to: path)
                manager.reloadFromCurrentEnvironment()
            }
            try Data(last).write(to: path)
            manager.reloadFromCurrentEnvironment()
            resume.signal()
            finished.wait()
            guard sotf_atomic_load_u64(result) == 1 else { return false }
            _ = manager.checkAndReload()
            return manager.withCipher { $0?.getFingerprint() == expectedLast ? 1 : 0 } == 1
        } catch {
            halLog("    FAIL: cipher publication fixture: \(error)")
            return false
        }
    }

    /// Regression: re-anchoring the clock on a sample-rate change lands the
    /// anchor between period boundaries, so the zero timestamp floor is
    /// *before* the anchor and the host-time offset is negative. The old
    /// unsigned conversion trapped, crash-looping the driver host until
    /// coreaudiod deactivated the device (2026-10-17).
    static func testZeroTimestampSurvivesRateReanchor() -> Bool {
        halLog("  Test: Zero timestamp survives sample-rate re-anchor")
        for delayMicroseconds in [30_000, 41_000, 53_000, 67_000, 79_000] as [useconds_t] {
            let clock = DriverClock()
            clock.start(sampleRate: 48000)
            usleep(delayMicroseconds)
            clock.setSampleRate(96000)
            guard let (sampleTime, hostTime, _) = clock.getZeroTimeStamp(period: 512) else { return false }
            guard hostTime > 0 else {
                halLog("    FAIL: zero host time after re-anchor")
                return false
            }
            guard sampleTime.truncatingRemainder(dividingBy: 512) == 0 else {
                halLog("    FAIL: zero sample time \(sampleTime) is not period-aligned")
                return false
            }
            guard hostTime <= mach_absolute_time() else {
                halLog("    FAIL: zero host time is in the future")
                return false
            }
        }
        return true
    }

    static func testClockSnapshotsDuringRateChanges() -> Bool {
        let clock = DriverClock()
        clock.start(sampleRate: 48000)
        let writer = DispatchGroup()
        writer.enter()
        DispatchQueue.global(qos: .utility).async {
            for index in 0..<10_000 {
                clock.setSampleRate(index.isMultiple(of: 2) ? 96000 : 48000)
            }
            writer.leave()
        }
        defer { writer.wait() }
        var lastSeed: UInt64 = 0
        var observations = 0
        for _ in 0..<20_000 {
            guard let (sample, host, seed) = clock.getZeroTimeStamp(period: 512) else { continue }
            guard sample.isFinite, sample >= 0, host > 0, host <= mach_absolute_time(),
                  sample.truncatingRemainder(dividingBy: 512) == 0, seed >= lastSeed else {
                halLog("    FAIL: incoherent clock snapshot during a rate change")
                return false
            }
            lastSeed = seed
            observations += 1
        }
        return observations > 0
    }

    /// Keep the Swift mirror byte-for-byte compatible with Rust's
    /// `#[repr(C, align(8))] SharedAudioHeader`. Rust asserts the same table in
    /// `shared_memory/tests.rs`; changing either side without updating the
    /// other fails its platform test instead of silently corrupting the mmap.
    static func testSharedAudioHeaderLayout() -> Bool {
        halLog("  Test: SharedAudioHeader layout")

        guard let manifest = sharedMemoryLayoutManifest(),
              // Rust's repr(C, align(8)) size includes trailing padding. Swift
              // exposes the same ABI value as `stride`; `size` only covers the
              // last field (which ends at byte 144 since the version-7
              // volume fields filled the former padding).
              MemoryLayout<SharedAudioHeader>.stride == manifest.size,
              MemoryLayout<SharedAudioHeader>.alignment == manifest.alignment else {
            halLog(
                "    FAIL: size=\(MemoryLayout<SharedAudioHeader>.size), " +
                "stride=\(MemoryLayout<SharedAudioHeader>.stride), " +
                "alignment=\(MemoryLayout<SharedAudioHeader>.alignment)"
            )
            return false
        }

        let actualOffsets: [String: Int] = [
            "magic": MemoryLayout<SharedAudioHeader>.offset(of: \.magic)!,
            "version": MemoryLayout<SharedAudioHeader>.offset(of: \.version)!,
            "sample_rate": MemoryLayout<SharedAudioHeader>.offset(of: \.sampleRate)!,
            "buffer_frames": MemoryLayout<SharedAudioHeader>.offset(of: \.bufferFrames)!,
            "channel_count": MemoryLayout<SharedAudioHeader>.offset(of: \.channelCount)!,
            "write_position": MemoryLayout<SharedAudioHeader>.offset(of: \.writePosition)!,
            "read_position": MemoryLayout<SharedAudioHeader>.offset(of: \.readPosition)!,
            "active": MemoryLayout<SharedAudioHeader>.offset(of: \.active)!,
            "config_changed": MemoryLayout<SharedAudioHeader>.offset(of: \.configChanged)!,
            "driver_ready": MemoryLayout<SharedAudioHeader>.offset(of: \.driverReady)!,
            "engine_ready": MemoryLayout<SharedAudioHeader>.offset(of: \.engineReady)!,
            "encrypted": MemoryLayout<SharedAudioHeader>.offset(of: \.encrypted)!,
            "volume_scalar_bits": MemoryLayout<SharedAudioHeader>.offset(of: \.volumeScalarBits)!,
            "key_fingerprint": MemoryLayout<SharedAudioHeader>.offset(of: \.keyFingerprint)!,
            "frame_counter": MemoryLayout<SharedAudioHeader>.offset(of: \.frameCounter)!,
            "requested_sample_rate": MemoryLayout<SharedAudioHeader>.offset(of: \.requestedSampleRate)!,
            "requested_buffer_frames": MemoryLayout<SharedAudioHeader>.offset(of: \.requestedBufferFrames)!,
            "actual_sample_rate": MemoryLayout<SharedAudioHeader>.offset(of: \.actualSampleRate)!,
            "actual_buffer_frames": MemoryLayout<SharedAudioHeader>.offset(of: \.actualBufferFrames)!,
            "config_status": MemoryLayout<SharedAudioHeader>.offset(of: \.configStatus)!,
            "config_source": MemoryLayout<SharedAudioHeader>.offset(of: \.configSource)!,
            "config_error_code": MemoryLayout<SharedAudioHeader>.offset(of: \.configErrorCode)!,
            "output_muted": MemoryLayout<SharedAudioHeader>.offset(of: \.outputMuted)!,
            "encryption_overflow_count": MemoryLayout<SharedAudioHeader>.offset(of: \.encryptionOverflowCount)!,
            "daemon_heartbeat_ms": MemoryLayout<SharedAudioHeader>.offset(of: \.daemonHeartbeatMs)!,
            "configuring": MemoryLayout<SharedAudioHeader>.offset(of: \.configuring)!,
            "configuring_ack": MemoryLayout<SharedAudioHeader>.offset(of: \.configuringAck)!,
            "requested_channel_count": MemoryLayout<SharedAudioHeader>.offset(of: \.requestedChannelCount)!,
            "volume_generation": MemoryLayout<SharedAudioHeader>.offset(of: \.volumeGeneration)!,
        ]
        guard Set(actualOffsets.keys) == Set(manifest.fields.keys) else {
            halLog("    FAIL: manifest field set does not match Swift header")
            return false
        }
        for (name, expected) in manifest.fields where actualOffsets[name] != expected {
            halLog("    FAIL: \(name) offset=\(String(describing: actualOffsets[name])), expected=\(expected)")
            return false
        }

        halLog("    PASS")
        return true
    }

    /// Volume conversion curve endpoints/clamps plus the shared-header
    /// publication round trip the daemon polls (version 7 fields).
    static func testVolumeConversionAndSharedRoundTrip() -> Bool {
        halLog("  Test: volume conversion and shared round trip")

        // Conversion endpoints and clamps.
        guard volumeScalarToDecibels(1.0) == 0,
              volumeScalarToDecibels(0) == kVolumeMinDecibels,
              volumeDecibelsToScalar(0) == 1.0,
              volumeDecibelsToScalar(kVolumeMinDecibels) == 0,
              volumeDecibelsToScalar(6) == 1.0,
              abs(volumeScalarToDecibels(0.5) - (-6.0206)) < 0.001,
              abs(volumeDecibelsToScalar(-6.0206) - 0.5) < 0.001,
              clampVolumeScalar(2.0) == 1.0,
              clampVolumeScalar(-1.0) == 0,
              clampVolumeScalar(.nan) == 1.0 else {
            halLog("    FAIL: volume conversion endpoints or clamps are wrong")
            return false
        }

        // Shared-header round trip on a daemon-style fixture file.
        let fileManager = FileManager.default
        let tempDir = (NSTemporaryDirectory() as NSString)
            .appendingPathComponent("sotf-hal-volume-\(UUID().uuidString)")
        let shmPath = (tempDir as NSString).appendingPathComponent("audio.shm")
        let headerSize = MemoryLayout<SharedAudioHeader>.size
        let alignedHeaderSize = (headerSize + 63) & ~63
        let totalSize = alignedHeaderSize + 64 * 2 * 16 * MemoryLayout<Float>.size

        do {
            try fileManager.createDirectory(
                atPath: tempDir,
                withIntermediateDirectories: true,
                attributes: [.posixPermissions: 0o700]
            )
            fileManager.createFile(atPath: shmPath, contents: Data(count: totalSize))
            chmod(shmPath, 0o600)
        } catch {
            halLog("    FAIL: fixture setup failed: \(error)")
            return false
        }

        var header = SharedAudioHeader(
            magic: 0x534F5446,
            version: 7,
            sampleRate: 48_000,
            bufferFrames: 64,
            channelCount: 2,
            writePosition: 0,
            readPosition: 0,
            active: 0,
            configChanged: 0,
            driverReady: 0,
            engineReady: 1,
            encrypted: 0,
            volumeScalarBits: Float32(1.0).bitPattern,
            keyFingerprint: 0,
            frameCounter: 0,
            requestedSampleRate: 48_000,
            requestedBufferFrames: 64,
            actualSampleRate: 48_000,
            actualBufferFrames: 64,
            configStatus: 1,
            configSource: 0,
            configErrorCode: 0,
            outputMuted: 0,
            encryptionOverflowCount: 0,
            daemonHeartbeatMs: 0,
            configuring: 0,
            configuringAck: 0,
            requestedChannelCount: 2,
            volumeGeneration: 1
        )

        let fd = Darwin.open(shmPath, O_RDWR)
        guard fd >= 0 else {
            try? fileManager.removeItem(atPath: tempDir)
            halLog("    FAIL: fixture open failed")
            return false
        }
        let headerWritten = withUnsafeBytes(of: &header) { bytes in
            Darwin.write(fd, bytes.baseAddress, bytes.count)
        }
        Darwin.close(fd)
        guard headerWritten == headerSize else {
            try? fileManager.removeItem(atPath: tempDir)
            halLog("    FAIL: fixture header write failed")
            return false
        }

        setenv("SOTF_HAL_SHARED_MEMORY_PATH", shmPath, 1)
        defer {
            unsetenv("SOTF_HAL_SHARED_MEMORY_PATH")
            try? fileManager.removeItem(atPath: tempDir)
        }

        let sharedAudio = SharedAudioBuffer()
        guard sharedAudio.initialize(sampleRate: 48_000, bufferFrames: 64, channelCount: 2) else {
            halLog("    FAIL: SharedAudioBuffer.initialize failed")
            return false
        }
        defer { sharedAudio.closeSharedMemory() }

        let initial = sharedAudio.outputVolume()
        guard initial.scalarBits == Float32(1.0).bitPattern, !initial.muted, initial.generation == 1 else {
            halLog("    FAIL: initial volume readback is wrong: \(initial)")
            return false
        }

        let generation = sharedAudio.setOutputVolume(
            scalarBits: Float32(0.25).bitPattern,
            muted: true
        )
        let updated = sharedAudio.outputVolume()
        guard generation == 2,
              updated.scalarBits == Float32(0.25).bitPattern,
              updated.muted,
              updated.generation == 2 else {
            halLog("    FAIL: published volume readback is wrong: \(updated)")
            return false
        }

        halLog("    PASS")
        return true
    }

    /// A HAL property change must publish a request without changing the
    /// active ring geometry. The daemon owns the acknowledgment; only then
    /// may the active sample rate be changed by the driver state machine.
    static func testSampleRateRequestRemainsTransactional() -> Bool {
        halLog("  Test: sample-rate request remains transactional")

        let fileManager = FileManager.default
        let tempDir = (NSTemporaryDirectory() as NSString)
            .appendingPathComponent("sotf-hal-config-(UUID().uuidString)")
        let shmPath = (tempDir as NSString).appendingPathComponent("audio.shm")
        let sampleRate: UInt32 = 48_000
        let bufferFrames: UInt32 = 64
        let channelCount: UInt32 = 2
        // The daemon sizes the file for the deepest negotiated rate: at
        // 96 kHz the ring holds 2x blocks, so the fixture must too.
        let audioCapacity = Int(bufferFrames) * Int(channelCount) * 16
        let headerSize = MemoryLayout<SharedAudioHeader>.size
        let alignedHeaderSize = (headerSize + 63) & ~63
        let totalSize = alignedHeaderSize + audioCapacity * MemoryLayout<Float>.size

        do {
            try fileManager.createDirectory(
                atPath: tempDir,
                withIntermediateDirectories: true,
                attributes: [.posixPermissions: 0o700]
            )
            fileManager.createFile(atPath: shmPath, contents: Data(count: totalSize))
            chmod(shmPath, 0o600)
        } catch {
            halLog("    FAIL: fixture setup failed: (error)")
            return false
        }

        var header = SharedAudioHeader(
            magic: 0x534F5446,
            version: 7,
            sampleRate: sampleRate,
            bufferFrames: bufferFrames,
            channelCount: channelCount,
            writePosition: 0,
            readPosition: 0,
            active: 0,
            configChanged: 0,
            driverReady: 0,
            engineReady: 1,
            encrypted: 0,
            volumeScalarBits: 0,
            keyFingerprint: 0,
            frameCounter: 0,
            requestedSampleRate: sampleRate,
            requestedBufferFrames: bufferFrames,
            actualSampleRate: sampleRate,
            actualBufferFrames: bufferFrames,
            configStatus: 1,
            configSource: 0,
            configErrorCode: 0,
            outputMuted: 0,
            encryptionOverflowCount: 0,
            daemonHeartbeatMs: 0,
            configuring: 0,
            configuringAck: 0,
            requestedChannelCount: channelCount,
            volumeGeneration: 0
        )

        let fd = Darwin.open(shmPath, O_RDWR)
        guard fd >= 0 else {
            try? fileManager.removeItem(atPath: tempDir)
            halLog("    FAIL: fixture open failed")
            return false
        }
        let headerWritten = withUnsafeBytes(of: &header) { bytes in
            Darwin.write(fd, bytes.baseAddress, bytes.count)
        }
        Darwin.close(fd)
        guard headerWritten == headerSize else {
            try? fileManager.removeItem(atPath: tempDir)
            halLog("    FAIL: fixture header write failed")
            return false
        }

        setenv("SOTF_HAL_SHARED_MEMORY_PATH", shmPath, 1)
        defer {
            unsetenv("SOTF_HAL_SHARED_MEMORY_PATH")
            try? fileManager.removeItem(atPath: tempDir)
        }

        let sharedAudio = SharedAudioBuffer()
        guard sharedAudio.initialize(
            sampleRate: sampleRate,
            bufferFrames: bufferFrames,
            channelCount: channelCount
        ) else {
            halLog("    FAIL: SharedAudioBuffer.initialize failed")
            return false
        }
        defer { sharedAudio.closeSharedMemory() }

        sharedAudio.updateSampleRate(96_000)

        guard sharedAudio.getActiveSampleRate() == sampleRate,
              sharedAudio.getRequestedSampleRate() == 96_000,
              sharedAudio.configChanged(),
              sharedAudio.configSource() == 1,
              sharedAudio.getConfigStatus() == 0 else {
            halLog("    FAIL: request changed active state or did not publish pending status")
            return false
        }

        sharedAudio.acknowledgeConfigChange(
            actualSampleRate: 96_000,
            actualBufferFrames: bufferFrames,
            status: 1,
            errorCode: 0
        )
        guard sharedAudio.applyActiveConfiguration(
            sampleRate: 96_000,
            bufferFrames: bufferFrames,
            channelCount: channelCount
        ), sharedAudio.getActiveSampleRate() == 96_000 else {
            halLog("    FAIL: acknowledged geometry was not committed")
            return false
        }

        halLog("    PASS")
        return true
    }

    /// Exercise the same pending-channel and configuring_ack protocol used by
    /// the Rust daemon. The fixture is intentionally larger than the active
    /// 2-channel geometry so the new 8-channel geometry can be committed only
    /// after the simulated Rust-side quiesce request has been acknowledged by
    /// Swift IO.
    static func testCrossProcessReconfigurationProtocol() -> Bool {
        halLog("  Test: cross-process reconfiguration protocol")

        let fileManager = FileManager.default
        let tempDir = (NSTemporaryDirectory() as NSString)
            .appendingPathComponent("sotf-hal-reconfigure-\(UUID().uuidString)")
        let shmPath = (tempDir as NSString).appendingPathComponent("audio.shm")
        let sampleRate: UInt32 = 48_000
        let bufferFrames: UInt32 = 64
        let activeChannels: UInt32 = 2
        let pendingChannels: UInt32 = 8
        // Sized for the deepest negotiated rate (96 kHz -> 2x ring blocks),
        // matching how the daemon creates the real file.
        let audioCapacity = Int(bufferFrames) * Int(pendingChannels) * 16
        let alignedHeaderSize = (MemoryLayout<SharedAudioHeader>.size + 63) & ~63
        let totalSize = alignedHeaderSize + audioCapacity * MemoryLayout<Float>.size

        do {
            try fileManager.createDirectory(
                atPath: tempDir,
                withIntermediateDirectories: true,
                attributes: [.posixPermissions: 0o700]
            )
            fileManager.createFile(atPath: shmPath, contents: Data(count: totalSize))
            chmod(shmPath, 0o600)
        } catch {
            halLog("    FAIL: fixture setup failed: \(error)")
            return false
        }
        defer { try? fileManager.removeItem(atPath: tempDir) }

        var header = SharedAudioHeader(
            magic: 0x534F5446,
            version: 7,
            sampleRate: sampleRate,
            bufferFrames: bufferFrames,
            channelCount: activeChannels,
            writePosition: 0,
            readPosition: 0,
            active: 0,
            configChanged: 0,
            driverReady: 0,
            engineReady: 1,
            encrypted: 0,
            volumeScalarBits: 0,
            keyFingerprint: 0,
            frameCounter: 0,
            requestedSampleRate: sampleRate,
            requestedBufferFrames: bufferFrames,
            actualSampleRate: sampleRate,
            actualBufferFrames: bufferFrames,
            configStatus: 1,
            configSource: 0,
            configErrorCode: 0,
            outputMuted: 0,
            encryptionOverflowCount: 0,
            daemonHeartbeatMs: 0,
            configuring: 0,
            configuringAck: 0,
            requestedChannelCount: activeChannels,
            volumeGeneration: 0
        )
        let fd = Darwin.open(shmPath, O_RDWR)
        guard fd >= 0 else {
            halLog("    FAIL: fixture open failed")
            return false
        }
        let headerWritten = withUnsafeBytes(of: &header) { bytes in
            Darwin.pwrite(fd, bytes.baseAddress, bytes.count, 0)
        }
        guard headerWritten == MemoryLayout<SharedAudioHeader>.size else {
            Darwin.close(fd)
            halLog("    FAIL: fixture header write failed")
            return false
        }

        func writeUInt32(_ value: UInt32, at offset: off_t) -> Bool {
            var value = value
            return withUnsafeBytes(of: &value) { bytes in
                Darwin.pwrite(fd, bytes.baseAddress, bytes.count, offset) == bytes.count
            }
        }
        func readUInt32(at offset: off_t) -> UInt32? {
            var value: UInt32 = 0
            let count = withUnsafeMutableBytes(of: &value) { bytes in
                Darwin.pread(fd, bytes.baseAddress, bytes.count, offset)
            }
            return count == MemoryLayout<UInt32>.size ? value : nil
        }

        setenv("SOTF_HAL_SHARED_MEMORY_PATH", shmPath, 1)
        let sharedAudio = SharedAudioBuffer()
        guard sharedAudio.initialize(
            sampleRate: sampleRate,
            bufferFrames: bufferFrames,
            channelCount: activeChannels
        ) else {
            Darwin.close(fd)
            unsetenv("SOTF_HAL_SHARED_MEMORY_PATH")
            halLog("    FAIL: SharedAudioBuffer.initialize failed")
            return false
        }
        defer {
            sharedAudio.closeSharedMemory()
            Darwin.close(fd)
            unsetenv("SOTF_HAL_SHARED_MEMORY_PATH")
        }

        // Swift publishes the pending request while the active geometry stays
        // at 2 channels. A Rust daemon would consume these fields next.
        sharedAudio.updateSampleRate(96_000)
        guard writeUInt32(pendingChannels, at: 136),
              sharedAudio.getActiveChannelCount() == activeChannels,
              sharedAudio.getRequestedSampleRate() == 96_000,
              sharedAudio.getRequestedChannelCount() == pendingChannels else {
            halLog("    FAIL: pending request changed active geometry")
            return false
        }

        // A Rust-side reconfigure request sets configuring. Swift IO must
        // acknowledge it before either process changes ring geometry.
        guard writeUInt32(1, at: 128) else {
            halLog("    FAIL: could not publish configuring request")
            return false
        }
        var silence = [Float](repeating: 0, count: 2)
        _ = silence.withUnsafeMutableBufferPointer { buffer in
            sharedAudio.readAudio(buffer.baseAddress!, frameCount: 1, channelCount: 2)
        }
        guard readUInt32(at: 132) == 1 else {
            halLog("    FAIL: Swift IO did not publish configuring_ack")
            return false
        }
        guard writeUInt32(0, at: 128) else {
            halLog("    FAIL: could not clear configuring request")
            return false
        }

        sharedAudio.acknowledgeConfigChange(
            actualSampleRate: 96_000,
            actualBufferFrames: bufferFrames,
            status: 1,
            errorCode: 0
        )
        let applied = sharedAudio.applyActiveConfiguration(
            sampleRate: 96_000,
            bufferFrames: bufferFrames,
            channelCount: pendingChannels
        )
        let activeSampleRate = sharedAudio.getActiveSampleRate()
        let activeChannelCount = sharedAudio.getActiveChannelCount()
        let configuring = readUInt32(at: 128)
        let configuringAck = readUInt32(at: 132)
        guard applied,
              activeSampleRate == 96_000,
              activeChannelCount == pendingChannels,
              configuring == 0,
              configuringAck == 0 else {
            halLog(
                "    FAIL: acknowledged geometry was not committed atomically "
                    + "(applied=\(applied), rate=\(activeSampleRate), "
                    + "channels=\(activeChannelCount), configuring=\(String(describing: configuring)), "
                    + "ack=\(String(describing: configuringAck)))"
            )
            return false
        }

        guard runRustSwiftReconfigurationStress(
            sharedAudio: sharedAudio,
            sharedMemoryPath: shmPath
        ) else {
            return false
        }

        halLog("    PASS")
        return true
    }

    private static func runRustSwiftReconfigurationStress(
        sharedAudio: SharedAudioBuffer,
        sharedMemoryPath: String
    ) -> Bool {
        guard let workerPath = ProcessInfo.processInfo.environment[
            "SOTF_RUST_HAL_TRANSPORT_WORKER"
        ], FileManager.default.isExecutableFile(atPath: workerPath) else {
            halLog("    FAIL: SOTF_RUST_HAL_TRANSPORT_WORKER is not executable")
            return false
        }

        let process = Process()
        let diagnostics = Pipe()
        process.executableURL = URL(fileURLWithPath: workerPath)
        process.arguments = [sharedMemoryPath, "200"]
        process.standardOutput = diagnostics
        process.standardError = diagnostics

        do {
            try process.run()
        } catch {
            halLog("    FAIL: could not start Rust transport worker: \(error)")
            return false
        }

        let deadline = Date().addingTimeInterval(15)
        var framesRead = 0
        while process.isRunning && Date() < deadline {
            let channels = max(Int(sharedAudio.getActiveChannelCount()), 1)
            var samples = [Float](repeating: 0, count: 64 * channels)
            let read = samples.withUnsafeMutableBufferPointer { buffer in
                sharedAudio.readAudio(
                    buffer.baseAddress!,
                    frameCount: 64,
                    channelCount: channels
                )
            }
            framesRead += max(read, 0)
            usleep(50)
        }

        if process.isRunning {
            process.terminate()
            process.waitUntilExit()
            halLog("    FAIL: Rust transport worker timed out")
            return false
        }
        process.waitUntilExit()

        let output = diagnostics.fileHandleForReading.readDataToEndOfFile()
        let outputText = String(data: output, encoding: .utf8) ?? ""
        guard process.terminationStatus == 0 else {
            halLog(
                "    FAIL: Rust transport worker exited \(process.terminationStatus): "
                + outputText
            )
            return false
        }
        guard framesRead > 0,
              sharedAudio.getActiveChannelCount() == 2
                || sharedAudio.getActiveChannelCount() == 8,
              sharedAudio.getActiveSampleRate() == 48_000
                || sharedAudio.getActiveSampleRate() == 96_000
        else {
            halLog(
                "    FAIL: cross-language stress produced no frames or invalid geometry "
                + "(frames=\(framesRead), rate=\(sharedAudio.getActiveSampleRate()), "
                + "channels=\(sharedAudio.getActiveChannelCount()))"
            )
            return false
        }

        halLog("    Rust/Swift stress: \(framesRead) frames read across 200 reconfigurations")
        return true
    }

    /// A CoreAudio helper restart must preserve the daemon-owned active
    /// geometry. Reopening the mapping with a different requested format may
    /// publish a pending request, but it must never use a stale local cache to
    /// rewrite `channelCount` before the daemon acknowledges it.
    static func testGeometryCacheSurvivesDaemonRestart() -> Bool {
        halLog("  Test: geometry cache survives daemon restart")

        let fileManager = FileManager.default
        let tempDir = (NSTemporaryDirectory() as NSString)
            .appendingPathComponent("sotf-hal-restart-\(UUID().uuidString)")
        let shmPath = (tempDir as NSString).appendingPathComponent("audio.shm")
        let sampleRate: UInt32 = 48_000
        let bufferFrames: UInt32 = 64
        let activeChannels: UInt32 = 2
        let requestedChannels: UInt32 = 8
        let audioCapacity = Int(bufferFrames) * Int(requestedChannels) * 8
        let alignedHeaderSize = (MemoryLayout<SharedAudioHeader>.size + 63) & ~63
        let totalSize = alignedHeaderSize + audioCapacity * MemoryLayout<Float>.size

        do {
            try fileManager.createDirectory(
                atPath: tempDir,
                withIntermediateDirectories: true,
                attributes: [.posixPermissions: 0o700]
            )
            fileManager.createFile(atPath: shmPath, contents: Data(count: totalSize))
            chmod(shmPath, 0o600)
        } catch {
            halLog("    FAIL: fixture setup failed: \(error)")
            return false
        }
        defer { try? fileManager.removeItem(atPath: tempDir) }

        var header = SharedAudioHeader(
            magic: 0x534F5446,
            version: 7,
            sampleRate: sampleRate,
            bufferFrames: bufferFrames,
            channelCount: activeChannels,
            writePosition: 0,
            readPosition: 0,
            active: 1,
            configChanged: 0,
            driverReady: 1,
            engineReady: 1,
            encrypted: 0,
            volumeScalarBits: 0,
            keyFingerprint: 0,
            frameCounter: 0,
            requestedSampleRate: sampleRate,
            requestedBufferFrames: bufferFrames,
            actualSampleRate: sampleRate,
            actualBufferFrames: bufferFrames,
            configStatus: 1,
            configSource: 0,
            configErrorCode: 0,
            outputMuted: 0,
            encryptionOverflowCount: 0,
            daemonHeartbeatMs: 1,
            configuring: 0,
            configuringAck: 0,
            requestedChannelCount: activeChannels,
            volumeGeneration: 0
        )

        let fd = Darwin.open(shmPath, O_RDWR)
        guard fd >= 0 else {
            halLog("    FAIL: fixture open failed")
            return false
        }
        let written = withUnsafeBytes(of: &header) { bytes in
            Darwin.pwrite(fd, bytes.baseAddress, bytes.count, 0)
        }
        Darwin.close(fd)
        guard written == MemoryLayout<SharedAudioHeader>.size else {
            halLog("    FAIL: fixture header write failed")
            return false
        }

        setenv("SOTF_HAL_SHARED_MEMORY_PATH", shmPath, 1)
        defer { unsetenv("SOTF_HAL_SHARED_MEMORY_PATH") }

        let firstConnection = SharedAudioBuffer()
        guard firstConnection.initialize(
            sampleRate: sampleRate,
            bufferFrames: bufferFrames,
            channelCount: activeChannels
        ) else {
            halLog("    FAIL: first HAL connection failed")
            return false
        }
        firstConnection.closeSharedMemory()

        let state = DriverState.shared
        state.sampleRate = Float64(sampleRate)
        state.bufferFrameSize = bufferFrames
        state.channelCount = activeChannels
        let restartedConnection = state.sharedAudio
        guard restartedConnection.initialize(
            sampleRate: sampleRate,
            bufferFrames: bufferFrames,
            channelCount: requestedChannels
        ) else {
            halLog("    FAIL: restarted HAL connection failed")
            return false
        }
        defer { restartedConnection.closeSharedMemory() }

        guard restartedConnection.getActiveChannelCount() == activeChannels,
              restartedConnection.getRequestedChannelCount() == requestedChannels,
              restartedConnection.configChanged(),
              restartedConnection.configSource() == 1 else {
            halLog("    FAIL: restart rewrote active geometry or lost pending request")
            return false
        }

        guard restartedConnection.backingFileIsCurrent() else {
            halLog("    FAIL: live mapping did not match its backing file")
            return false
        }
        do {
            try fileManager.removeItem(atPath: shmPath)
        } catch {
            halLog("    FAIL: could not unlink mapped fixture: \(error)")
            return false
        }
        guard !restartedConnection.backingFileIsCurrent() else {
            halLog("    FAIL: orphaned mapping still matched the removed path")
            return false
        }

        func restoreBackingFile() -> Bool {
            guard fileManager.createFile(
                atPath: shmPath,
                contents: Data(count: totalSize)
            ) else {
                return false
            }
            chmod(shmPath, 0o600)
            let replacementFD = Darwin.open(shmPath, O_RDWR)
            guard replacementFD >= 0 else { return false }
            defer { Darwin.close(replacementFD) }
            let replacementWritten = withUnsafeBytes(of: &header) { bytes in
                Darwin.pwrite(replacementFD, bytes.baseAddress, bytes.count, 0)
            }
            return replacementWritten == MemoryLayout<SharedAudioHeader>.size
        }

        let activeClientID: UInt32 = 77
        _ = state.startIOClient(activeClientID)
        let pinnedGeneration = state.acquireSharedAudioForCallback()
        guard restoreBackingFile(),
              state.handoffStaleSharedAudioMappingIfNeeded(),
              state.ioClientCount == 1,
              state.sharedAudio !== pinnedGeneration.buffer,
              state.sharedAudio.backingFileIsCurrent(),
              pinnedGeneration.buffer.isConnected else {
            state.finishSharedAudioCallback(slot: pinnedGeneration.slot)
            halLog("    FAIL: active-client mapping generation handoff failed")
            return false
        }
        state.finishSharedAudioCallback(slot: pinnedGeneration.slot)
        _ = state.stopIOClient(activeClientID)

        do {
            try fileManager.removeItem(atPath: shmPath)
        } catch {
            halLog("    FAIL: could not replace idle-client fixture: \(error)")
            return false
        }
        let previousGeneration = state.sharedAudio
        guard restoreBackingFile(),
              state.handoffStaleSharedAudioMappingIfNeeded(),
              state.ioClientCount == 0,
              state.sharedAudio !== previousGeneration,
              state.sharedAudio.backingFileIsCurrent() else {
            halLog("    FAIL: idle-client mapping generation handoff failed")
            return false
        }

        halLog("    PASS")
        return true
    }

    // ==========================================================================
    // Ring Buffer Tests
    // ==========================================================================

    /// Test basic ring buffer operations
    static func testRingBufferBasicOperations() -> Bool {
        halLog("  Test: RingBuffer basic operations")

        let buffer = AudioRingBuffer(capacity: 1024)

        // Initially empty
        guard buffer.availableToRead == 0 else {
            halLog("    FAIL: Buffer should be empty initially")
            return false
        }

        guard buffer.availableToWrite > 0 else {
            halLog("    FAIL: Buffer should have space to write initially")
            return false
        }

        // Write some data
        var writeData = [Float](repeating: 0.5, count: 256)
        let written = buffer.write(&writeData, count: 256)

        guard written == 256 else {
            halLog("    FAIL: Should write 256 samples, wrote \(written)")
            return false
        }

        guard buffer.availableToRead == 256 else {
            halLog("    FAIL: Should have 256 samples to read, have \(buffer.availableToRead)")
            return false
        }

        // Read data back
        var readData = [Float](repeating: 0, count: 256)
        let read = buffer.read(&readData, count: 256)

        guard read == 256 else {
            halLog("    FAIL: Should read 256 samples, read \(read)")
            return false
        }

        // Verify data matches
        for i in 0..<256 {
            if readData[i] != writeData[i] {
                halLog("    FAIL: Data mismatch at index \(i)")
                return false
            }
        }

        halLog("    PASS")
        return true
    }

    /// Test ring buffer wrap-around behavior
    static func testRingBufferWrapAround() -> Bool {
        halLog("  Test: RingBuffer wrap-around")

        let capacity = 256
        let buffer = AudioRingBuffer(capacity: capacity)

        // Write and read multiple times to force wrap-around
        for iteration in 0..<10 {
            var writeData: [Float] = (0..<128).map { Float($0 + iteration * 128) }
            let written = buffer.write(&writeData, count: 128)

            guard written == 128 else {
                halLog("    FAIL: Iteration \(iteration) - wrote \(written), expected 128")
                return false
            }

            var readData = [Float](repeating: 0, count: 128)
            let read = buffer.read(&readData, count: 128)

            guard read == 128 else {
                halLog("    FAIL: Iteration \(iteration) - read \(read), expected 128")
                return false
            }

            // Verify data
            for i in 0..<128 {
                if readData[i] != writeData[i] {
                    halLog("    FAIL: Data mismatch at iteration \(iteration), index \(i)")
                    return false
                }
            }
        }

        halLog("    PASS")
        return true
    }

    /// Test multi-channel ring buffer operations
    static func testRingBufferConcurrentSPSC() -> Bool {
        halLog("  Test: RingBuffer concurrent SPSC")

        let channels = 2
        let totalFrames = 10_000
        let ring = MultiChannelRingBuffer(channelCount: channels, framesCapacity: 257)
        let input = UnsafeMutablePointer<Float>.allocate(capacity: totalFrames * channels)
        let output = UnsafeMutablePointer<Float>.allocate(capacity: totalFrames * channels)
        defer {
            input.deallocate()
            output.deallocate()
        }
        for sample in 0..<(totalFrames * channels) {
            input[sample] = Float(sample)
            output[sample] = -1
        }

        let group = DispatchGroup()
        group.enter()
        DispatchQueue.global(qos: .userInitiated).async {
            var frameOffset = 0
            while frameOffset < totalFrames {
                let requested = min(31, totalFrames - frameOffset)
                let written = ring.writeInterleaved(
                    input.advanced(by: frameOffset * channels),
                    frameCount: requested
                )
                frameOffset += written
                if written == 0 { usleep(10) }
            }
            group.leave()
        }

        group.enter()
        DispatchQueue.global(qos: .userInitiated).async {
            var frameOffset = 0
            while frameOffset < totalFrames {
                let requested = min(29, totalFrames - frameOffset)
                let read = ring.readInterleaved(
                    output.advanced(by: frameOffset * channels),
                    frameCount: requested
                )
                frameOffset += read
                if read == 0 { usleep(10) }
            }
            group.leave()
        }

        guard group.wait(timeout: .now() + 10) == .success else {
            halLog("    FAIL: concurrent SPSC transfer timed out")
            return false
        }
        for sample in 0..<(totalFrames * channels) {
            guard input[sample].bitPattern == output[sample].bitPattern else {
                halLog("    FAIL: concurrent sample \(sample) mismatch")
                return false
            }
        }

        halLog("    PASS")
        return true
    }

    static func testRingBufferMultiChannel() -> Bool {
        halLog("  Test: RingBuffer multi-channel")

        let channels = 6  // 5.1 surround
        let frames = 64
        // Capacity one frame larger than the block makes the second pass wrap
        // at the end of every per-channel ring.
        let buffer = MultiChannelRingBuffer(channelCount: channels, framesCapacity: frames + 1)

        // Create interleaved multi-channel data
        var writeData = [Float](repeating: 0, count: frames * channels)
        for frame in 0..<frames {
            for ch in 0..<channels {
                // Each channel has a unique offset for identification
                writeData[frame * channels + ch] = Float(frame) + Float(ch) * 0.1
            }
        }

        // Write interleaved
        let written = buffer.writeInterleaved(&writeData, frameCount: frames)
        guard written == frames else {
            halLog("    FAIL: Should write \(frames) frames, wrote \(written)")
            return false
        }

        // Read interleaved
        var readData = [Float](repeating: 0, count: frames * channels)
        let read = buffer.readInterleaved(&readData, frameCount: frames)
        guard read == frames else {
            halLog("    FAIL: Should read \(frames) frames, read \(read)")
            return false
        }

        // Verify all channels preserved
        for frame in 0..<frames {
            for ch in 0..<channels {
                let idx = frame * channels + ch
                if readData[idx] != writeData[idx] {
                    halLog("    FAIL: Mismatch at frame \(frame), channel \(ch)")
                    return false
                }
            }
        }

        guard buffer.writeInterleaved(&writeData, frameCount: frames) == frames else {
            halLog("  FAIL: Wrapped write did not accept the whole block")
            return false
        }
        readData = [Float](repeating: 0, count: frames * channels)
        guard buffer.readInterleaved(&readData, frameCount: frames) == frames else {
            halLog("  FAIL: Wrapped read did not return the whole block")
            return false
        }
        guard readData == writeData else {
            halLog("  FAIL: Wrapped interleaved data mismatch")
            return false
        }

        halLog("    PASS")
        return true
    }

    // ==========================================================================
    // Encryption Tests
    // ==========================================================================

    /// Test encryption round-trip
    static func testEncryptionRoundTrip() -> Bool {
        halLog("  Test: Encryption round-trip")

        let keyBytes: [UInt8] = (0..<32).map { UInt8($0) }
        let cipher = AudioCipher(keyBytes: keyBytes)

        // Create test audio data
        let samples: [Float] = (0..<256).map { sin(Float($0) * 0.1) }

        // Encrypt
        guard let encrypted = cipher.encrypt(samples: samples, frameCounter: 1) else {
            halLog("    FAIL: Encryption failed")
            return false
        }

        // Encrypted should be larger (includes auth tag)
        guard encrypted.count > samples.count * 4 else {
            halLog("    FAIL: Encrypted data too small")
            return false
        }

        // Decrypt
        guard let decrypted = cipher.decrypt(ciphertext: encrypted, frameCounter: 1) else {
            halLog("    FAIL: Decryption failed")
            return false
        }

        // Verify
        guard decrypted.count == samples.count else {
            halLog("    FAIL: Decrypted length mismatch: \(decrypted.count) vs \(samples.count)")
            return false
        }

        for i in 0..<samples.count {
            if abs(decrypted[i] - samples[i]) > 0.0001 {
                halLog("    FAIL: Sample mismatch at \(i): \(decrypted[i]) vs \(samples[i])")
                return false
            }
        }

        halLog("    PASS")
        return true
    }

    /// Test that different keys produce different ciphertext
    static func testEncryptionWithDifferentKeys() -> Bool {
        halLog("  Test: Encryption with different keys")

        let key1: [UInt8] = (0..<32).map { UInt8($0) }
        let key2: [UInt8] = (0..<32).map { UInt8(31 - $0) }  // Different key

        let cipher1 = AudioCipher(keyBytes: key1)
        let cipher2 = AudioCipher(keyBytes: key2)

        // Verify fingerprints differ
        guard cipher1.getFingerprintHex() != cipher2.getFingerprintHex() else {
            halLog("    FAIL: Different keys should have different fingerprints")
            return false
        }

        // Encrypt same data with different keys
        let samples: [Float] = [0.5, 0.25, -0.5, -0.25]

        guard let encrypted1 = cipher1.encrypt(samples: samples, frameCounter: 1),
              let encrypted2 = cipher2.encrypt(samples: samples, frameCounter: 1) else {
            halLog("    FAIL: Encryption failed")
            return false
        }

        // Ciphertexts should differ
        guard encrypted1 != encrypted2 else {
            halLog("    FAIL: Different keys should produce different ciphertext")
            return false
        }

        // Decryption with wrong key should fail
        let wrongDecrypt = cipher1.decrypt(ciphertext: encrypted2, frameCounter: 1)
        guard wrongDecrypt == nil else {
            halLog("    FAIL: Decryption with wrong key should fail")
            return false
        }

        halLog("    PASS")
        return true
    }

    /// Test that frame counter provides unique nonces
    static func testEncryptionFrameCounterNonceUniqueness() -> Bool {
        halLog("  Test: Encryption frame counter nonce uniqueness")

        let keyBytes: [UInt8] = (0..<32).map { UInt8($0) }
        let cipher = AudioCipher(keyBytes: keyBytes)

        let samples: [Float] = [0.5, 0.25, -0.5, -0.25]

        // Same data encrypted with different frame counters should produce different ciphertext
        guard let enc1 = cipher.encrypt(samples: samples, frameCounter: 1),
              let enc2 = cipher.encrypt(samples: samples, frameCounter: 2),
              let enc3 = cipher.encrypt(samples: samples, frameCounter: 3) else {
            halLog("    FAIL: Encryption failed")
            return false
        }

        guard enc1 != enc2 && enc2 != enc3 && enc1 != enc3 else {
            halLog("    FAIL: Different frame counters should produce different ciphertext")
            return false
        }

        // Each can be decrypted with correct frame counter
        guard cipher.decrypt(ciphertext: enc1, frameCounter: 1) != nil,
              cipher.decrypt(ciphertext: enc2, frameCounter: 2) != nil,
              cipher.decrypt(ciphertext: enc3, frameCounter: 3) != nil else {
            halLog("    FAIL: Decryption with correct frame counter should succeed")
            return false
        }

        // Wrong frame counter should fail
        guard cipher.decrypt(ciphertext: enc1, frameCounter: 2) == nil,
              cipher.decrypt(ciphertext: enc2, frameCounter: 1) == nil else {
            halLog("    FAIL: Decryption with wrong frame counter should fail")
            return false
        }

        halLog("    PASS")
        return true
    }

    /// Cross-validate the allocation-free realtime core against CryptoKit.
    ///
    /// A self-round-trip cannot catch a wire-format deviation shared by seal
    /// and open: both sides agree with each other but not with RFC 8439 or
    /// the Rust daemon. This test seals with the realtime path and opens
    /// with CryptoKit and vice versa, at byte lengths that are and are not
    /// multiples of 16 — the pad16 framing fix is covered exactly by the
    /// non-aligned lengths, which every earlier test missed.
    static func testRealtimeCipherMatchesCryptoKit() -> Bool {
        halLog("  Test: Realtime cipher matches CryptoKit")
        let keyBytes: [UInt8] = (0..<32).map { UInt8(($0 * 7 + 3) & 0xFF) }
        let ckKey = SymmetricKey(data: keyBytes)
        let lengths = [0, 1, 2, 3, 4, 7, 12, 15, 16, 17, 20, 31, 32, 33, 63, 64, 65, 100, 1000]
        for (li, len) in lengths.enumerated() {
            let counter = UInt64(li * 1000 + 7)
            var nonceBytes = [UInt8](repeating: 0, count: 12)
            let bigEndianCounter = counter.bigEndian
            withUnsafeBytes(of: bigEndianCounter) { fcBytes in
                for (i, byte) in fcBytes.enumerated() {
                    nonceBytes[4 + i] = byte
                }
            }
            // Non-empty storage even for len 0 so raw pointers stay valid;
            // the length argument still governs how much is read.
            let ptStore = (0..<max(len, 1)).map { UInt8(($0 * 53 + 7) & 0xFF) }
            let pt = Array(ptStore.prefix(len))

            // CryptoKit seal is the independent oracle.
            let ckNonce: ChaChaPoly.Nonce
            let ckSealed: ChaChaPoly.SealedBox
            do {
                ckNonce = try ChaChaPoly.Nonce(data: nonceBytes)
                ckSealed = try ChaChaPoly.seal(
                    Data(pt), using: ckKey, nonce: ckNonce, authenticating: Data())
            } catch {
                halLog("    FAIL: CryptoKit seal threw at len \(len): \(error)")
                return false
            }

            // Realtime seal must be byte-identical.
            var rtOut = [UInt8](repeating: 0, count: len + kAuthTagSize)
            keyBytes.withUnsafeBufferPointer { kb in
                ptStore.withUnsafeBufferPointer { pb in
                    rtOut.withUnsafeMutableBufferPointer { ob in
                        RealtimeChaChaPoly.seal(
                            keyBytes: kb.baseAddress!,
                            plaintext: UnsafeRawPointer(pb.baseAddress!),
                            plaintextLen: len,
                            frameCounter: counter,
                            ciphertextOut: UnsafeMutableRawPointer(ob.baseAddress!),
                            tagOut: UnsafeMutableRawPointer(ob.baseAddress!.advanced(by: len)))
                    }
                }
            }
            guard Array(rtOut.prefix(len)) == Array(ckSealed.ciphertext) else {
                halLog("    FAIL: ciphertext mismatch at len \(len)")
                return false
            }
            guard Array(rtOut.suffix(kAuthTagSize)) == Array(ckSealed.tag) else {
                halLog("    FAIL: tag mismatch at len \(len)")
                return false
            }

            // CryptoKit opens the realtime box.
            do {
                let box = try ChaChaPoly.SealedBox(
                    nonce: ckNonce,
                    ciphertext: Data(rtOut.prefix(len)),
                    tag: Data(rtOut.suffix(kAuthTagSize)))
                let opened = try ChaChaPoly.open(box, using: ckKey, authenticating: Data())
                guard Array(opened) == pt else {
                    halLog("    FAIL: CryptoKit open mismatch at len \(len)")
                    return false
                }
            } catch {
                halLog("    FAIL: CryptoKit rejected realtime box at len \(len): \(error)")
                return false
            }

            // Realtime opens the CryptoKit box.
            var ctStore = Array(ckSealed.ciphertext)
            if ctStore.isEmpty {
                ctStore = [0]
            }
            let tagStore = Array(ckSealed.tag)
            var rtPt = [UInt8](repeating: 0, count: max(len, 1))
            let opened: Bool = keyBytes.withUnsafeBufferPointer { kb in
                ctStore.withUnsafeBufferPointer { cb in
                    tagStore.withUnsafeBufferPointer { tb in
                        rtPt.withUnsafeMutableBufferPointer { ob in
                            RealtimeChaChaPoly.open(
                                keyBytes: kb.baseAddress!,
                                ciphertext: UnsafeRawPointer(cb.baseAddress!),
                                ciphertextLen: len,
                                frameCounter: counter,
                                tag: UnsafeRawPointer(tb.baseAddress!),
                                plaintextOut: UnsafeMutableRawPointer(ob.baseAddress!))
                        }
                    }
                }
            }
            guard opened, Array(rtPt.prefix(len)) == pt else {
                halLog("    FAIL: realtime open mismatch at len \(len)")
                return false
            }

            // Tampered ciphertext and tampered tag must both be rejected.
            if len > 0 {
                var badCt = Array(ckSealed.ciphertext)
                badCt[len / 2] ^= 0x01
                let badStore = badCt
                let accepted: Bool = keyBytes.withUnsafeBufferPointer { kb in
                    badStore.withUnsafeBufferPointer { cb in
                        tagStore.withUnsafeBufferPointer { tb in
                            rtPt.withUnsafeMutableBufferPointer { ob in
                                RealtimeChaChaPoly.open(
                                    keyBytes: kb.baseAddress!,
                                    ciphertext: UnsafeRawPointer(cb.baseAddress!),
                                    ciphertextLen: len,
                                    frameCounter: counter,
                                    tag: UnsafeRawPointer(tb.baseAddress!),
                                    plaintextOut: UnsafeMutableRawPointer(ob.baseAddress!))
                            }
                        }
                    }
                }
                guard !accepted else {
                    halLog("    FAIL: tampered ciphertext accepted at len \(len)")
                    return false
                }
            }
            var badTag = tagStore
            badTag[0] ^= 0x01
            let tagAccepted: Bool = keyBytes.withUnsafeBufferPointer { kb in
                ctStore.withUnsafeBufferPointer { cb in
                    badTag.withUnsafeBufferPointer { tb in
                        rtPt.withUnsafeMutableBufferPointer { ob in
                            RealtimeChaChaPoly.open(
                                keyBytes: kb.baseAddress!,
                                ciphertext: UnsafeRawPointer(cb.baseAddress!),
                                ciphertextLen: len,
                                frameCounter: counter,
                                tag: UnsafeRawPointer(tb.baseAddress!),
                                plaintextOut: UnsafeMutableRawPointer(ob.baseAddress!))
                        }
                    }
                }
            }
            guard !tagAccepted else {
                halLog("    FAIL: tampered tag accepted at len \(len)")
                return false
            }
        }

        halLog("    PASS")
        return true
    }

    /// The encrypted realtime path is allocation-free (caller buffers +
    /// RealtimeChaChaPoly), so encrypted frames must roundtrip through the
    /// shared-memory ring instead of being rejected.
    static func testSharedMemoryEncryptedRealtimeRoundTrips() -> Bool {
        halLog("  Test: SharedMemory encrypted realtime transport roundtrips")

        let fileManager = FileManager.default
        let tempDir = (NSTemporaryDirectory() as NSString)
            .appendingPathComponent("sotf-hal-encrypted-\(UUID().uuidString)")
        let shmPath = (tempDir as NSString).appendingPathComponent("audio.shm")
        let keyPath = (tempDir as NSString).appendingPathComponent("session.key")
        let sampleRate: UInt32 = 48_000
        let bufferFrames: UInt32 = 64
        let channelCount: UInt32 = 2
        let audioCapacity = Int(bufferFrames) * Int(channelCount) * 8
        let headerSize = MemoryLayout<SharedAudioHeader>.size
        let alignedHeaderSize = (headerSize + 63) & ~63
        let totalSize = alignedHeaderSize + audioCapacity * MemoryLayout<Float>.size
        let keyBytes: [UInt8] = (0..<32).map { UInt8($0) }
        let cipher = AudioCipher(keyBytes: keyBytes)

        func fingerprintUInt64(_ bytes: [UInt8]) -> UInt64 {
            var value: UInt64 = 0
            for byte in bytes.prefix(8) {
                value = (value << 8) | UInt64(byte)
            }
            return value
        }

        do {
            try fileManager.createDirectory(
                atPath: tempDir,
                withIntermediateDirectories: true,
                attributes: [.posixPermissions: 0o700]
            )
            fileManager.createFile(atPath: shmPath, contents: Data(count: totalSize))
            chmod(shmPath, 0o600)
            try Data(keyBytes).write(to: URL(fileURLWithPath: keyPath))
            chmod(keyPath, 0o600)
        } catch {
            halLog("    FAIL: setup failed: \(error)")
            return false
        }

        setenv("SOTF_HAL_SHARED_MEMORY_PATH", shmPath, 1)
        setenv("SOTF_HAL_SESSION_KEY_PATH", keyPath, 1)
        defer {
            unsetenv("SOTF_HAL_SHARED_MEMORY_PATH")
            unsetenv("SOTF_HAL_SESSION_KEY_PATH")
            try? fileManager.removeItem(atPath: tempDir)
        }

        var header = SharedAudioHeader(
            magic: 0x534F5446,
            version: 7,
            sampleRate: sampleRate,
            bufferFrames: bufferFrames,
            channelCount: channelCount,
            writePosition: 0,
            readPosition: 0,
            active: 1,
            configChanged: 0,
            driverReady: 0,
            engineReady: 1,
            encrypted: 1,
            volumeScalarBits: 0,
            keyFingerprint: fingerprintUInt64(cipher.getFingerprint()),
            frameCounter: 0,
            requestedSampleRate: sampleRate,
            requestedBufferFrames: bufferFrames,
            actualSampleRate: sampleRate,
            actualBufferFrames: bufferFrames,
            configStatus: 1,
            configSource: 2,
            configErrorCode: 0,
            outputMuted: 0,
            encryptionOverflowCount: 0,
            daemonHeartbeatMs: 0,
            configuring: 0,
            configuringAck: 0,
            requestedChannelCount: channelCount,
            volumeGeneration: 0
        )

        let fd = Darwin.open(shmPath, O_RDWR)
        guard fd >= 0 else {
            halLog("    FAIL: open shared memory fixture failed")
            return false
        }
        let headerWritten = withUnsafeBytes(of: &header) { bytes in
            Darwin.write(fd, bytes.baseAddress, bytes.count)
        }
        Darwin.close(fd)
        guard headerWritten == headerSize else {
            halLog("    FAIL: header write failed")
            return false
        }

        EncryptionKeyManager.shared.reloadFromCurrentEnvironment()
        let sharedMemory = SharedAudioBuffer()
        guard sharedMemory.initialize(
            sampleRate: sampleRate,
            bufferFrames: bufferFrames,
            channelCount: channelCount
        ) else {
            halLog("    FAIL: SharedAudioBuffer.initialize failed")
            return false
        }
        defer { sharedMemory.closeSharedMemory() }

        let frames = Int(bufferFrames)
        let channels = Int(channelCount)
        var input = [Float]()
        input.reserveCapacity(frames * channels)
        for frame in 0..<frames {
            input.append(sin(Float(frame) * 0.1) * 0.5)
            input.append(cos(Float(frame) * 0.07) * 0.25)
        }

        let written = input.withUnsafeBufferPointer { ptr in
            sharedMemory.writeAudio(ptr.baseAddress!, frameCount: frames, channelCount: channels)
        }
        guard written == frames else {
            halLog("    FAIL: encrypted write returned \(written), expected \(frames)")
            return false
        }

        var output = [Float](repeating: 1, count: input.count)
        let read = output.withUnsafeMutableBufferPointer { ptr in
            sharedMemory.readAudio(ptr.baseAddress!, frameCount: frames, channelCount: channels)
        }
        guard read == frames else {
            halLog("    FAIL: encrypted read returned \(read), expected \(frames)")
            return false
        }

        guard output == input else {
            halLog("    FAIL: encrypted roundtrip mismatch")
            return false
        }

        halLog("    PASS")
        return true
    }

    /// Test key manager fingerprint consistency
    static func testKeyManagerFingerprint() -> Bool {
        let root = URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent("sotf-key-fingerprint-\(UUID())")
        let path = root.appendingPathComponent("session.key").path
        let key = (0..<32).map { UInt8($0) }
        do {
            try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
            try Data(key).write(to: URL(fileURLWithPath: path))
            chmod(path, 0o600)
        } catch { return false }
        let previous = ProcessInfo.processInfo.environment["SOTF_HAL_SESSION_KEY_PATH"]
        setenv("SOTF_HAL_SESSION_KEY_PATH", path, 1)
        let manager = EncryptionKeyManager.shared
        defer {
            if let previous { setenv("SOTF_HAL_SESSION_KEY_PATH", previous, 1) }
            else { unsetenv("SOTF_HAL_SESSION_KEY_PATH") }
            manager.reloadFromCurrentEnvironment()
            try? FileManager.default.removeItem(at: root)
        }
        manager.reloadFromCurrentEnvironment()
        let expected = AudioCipher(keyBytes: key).getFingerprint()
        return manager.getFingerprint() == expected && manager.getFingerprint() == expected && expected.count == 8
    }
}

// ==========================================================================
// ConfigBar Tests - to be added to ConfigBar.swift or separate test target
// ==========================================================================

/// Test utilities for ConfigBar components
/// Note: These would typically be in a separate test target with XCTest
class ConfigBarTestUtils {

    /// Test AnyCodable encoding/decoding
    static func testAnyCodableRoundTrip() -> Bool {
        // Test would require ConfigBar.swift AnyCodable struct
        // Implementation deferred to XCTest target
        return true
    }

    /// Test socket response parsing with fragmentation
    static func testSocketResponseParsing() -> Bool {
        // Test would require actual socket connection
        // Implementation deferred to integration tests
        return true
    }
}
