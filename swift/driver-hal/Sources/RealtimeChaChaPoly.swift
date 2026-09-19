/// Allocation-free ChaCha20-Poly1305 AEAD (RFC 8439) for the realtime IO path.
///
/// CryptoKit's `ChaChaPoly.seal/open` allocate internally, which is forbidden
/// on the CoreAudio IO thread. This implementation keeps every byte of working
/// state on the stack or in caller-provided buffers: no `Array`, `Data`,
/// `String`, closures, logging, or CryptoKit on this path.
///
/// Wire format (must match `driver-hal/src/encryption` on the Rust side):
/// - ChaCha20-Poly1305 with a 256-bit key.
/// - 96-bit nonce = 32 zero bits followed by the 64-bit frame counter,
///   big-endian (`nonce[4..<12] = counter.to_be_bytes()`).
/// - Empty AAD. Ciphertext || 16-byte tag appended.
/// - Plaintext is f32 samples as native-endian bytes (little-endian on all
///   Apple-silicon and Intel targets).
///
/// The caller owns all buffers: key (32 bytes), plaintext/ciphertext, and the
/// 16-byte tag region. Key expansion runs per call from the 32 key bytes and
/// touches only stack temporaries.
enum RealtimeChaChaPoly {
    // MARK: - ChaCha20 block function

    @inline(__always)
    private static func quarterRound(
        _ a: inout UInt32,
        _ b: inout UInt32,
        _ c: inout UInt32,
        _ d: inout UInt32
    ) {
        a = a &+ b; d ^= a; d = (d << 16) | (d >> 16)
        c = c &+ d; b ^= c; b = (b << 12) | (b >> 20)
        a = a &+ b; d ^= a; d = (d << 8) | (d >> 24)
        c = c &+ d; b ^= c; b = (b << 7) | (b >> 25)
    }

    /// Serialize one 64-byte ChaCha20 block into 16 little-endian words.
    /// `counter` is the RFC 8439 32-bit block counter; our records are far
    /// below 2^32 blocks, and the counter wraps rather than trapping.
    private static func block(
        key: (UInt32, UInt32, UInt32, UInt32, UInt32, UInt32, UInt32, UInt32),
        counter: UInt32,
        nonce: (UInt32, UInt32, UInt32),
        out: UnsafeMutablePointer<UInt32>
    ) {
        var s0: UInt32 = 0x61707865
        var s1: UInt32 = 0x3320646E
        var s2: UInt32 = 0x79622D32
        var s3: UInt32 = 0x6B206574
        var s4 = key.0
        var s5 = key.1
        var s6 = key.2
        var s7 = key.3
        var s8 = key.4
        var s9 = key.5
        var s10 = key.6
        var s11 = key.7
        var s12 = counter
        var s13 = nonce.0
        var s14 = nonce.1
        var s15 = nonce.2

        for _ in 0..<10 {
            quarterRound(&s0, &s4, &s8, &s12)
            quarterRound(&s1, &s5, &s9, &s13)
            quarterRound(&s2, &s6, &s10, &s14)
            quarterRound(&s3, &s7, &s11, &s15)
            quarterRound(&s0, &s5, &s10, &s15)
            quarterRound(&s1, &s6, &s11, &s12)
            quarterRound(&s2, &s7, &s8, &s13)
            quarterRound(&s3, &s4, &s9, &s14)
        }

        out[0] = s0 &+ 0x61707865
        out[1] = s1 &+ 0x3320646E
        out[2] = s2 &+ 0x79622D32
        out[3] = s3 &+ 0x6B206574
        out[4] = s4 &+ key.0
        out[5] = s5 &+ key.1
        out[6] = s6 &+ key.2
        out[7] = s7 &+ key.3
        out[8] = s8 &+ key.4
        out[9] = s9 &+ key.5
        out[10] = s10 &+ key.6
        out[11] = s11 &+ key.7
        out[12] = s12 &+ counter
        out[13] = s13 &+ nonce.0
        out[14] = s14 &+ nonce.1
        out[15] = s15 &+ nonce.2
    }

    private static func loadKeyWords(_ key: UnsafePointer<UInt8>) -> (
        UInt32, UInt32, UInt32, UInt32, UInt32, UInt32, UInt32, UInt32
    ) {
        func w(_ i: Int) -> UInt32 {
            UInt32(key[i]) | (UInt32(key[i + 1]) << 8) |
                (UInt32(key[i + 2]) << 16) | (UInt32(key[i + 3]) << 24)
        }
        return (w(0), w(4), w(8), w(12), w(16), w(20), w(24), w(28))
    }

    private static func loadNonceWords(frameCounter: UInt64) -> (UInt32, UInt32, UInt32) {
        // Nonce bytes: [0,0,0,0] ++ counter.to_be_bytes(). As little-endian
        // words over those bytes: word0 covers bytes 0..4.
        let hi = UInt32((frameCounter >> 32) & 0xFFFF_FFFF)
        let lo = UInt32(frameCounter & 0xFFFF_FFFF)
        let n0: UInt32 = 0
        // Bytes 4..8 hold hi big-endian; bytes 8..12 hold lo big-endian.
        let n1: UInt32 = ((hi & 0xFF) << 24) | ((hi >> 8 & 0xFF) << 16) |
            ((hi >> 16 & 0xFF) << 8) | (hi >> 24)
        let n2: UInt32 = ((lo & 0xFF) << 24) | ((lo >> 8 & 0xFF) << 16) |
            ((lo >> 16 & 0xFF) << 8) | (lo >> 24)
        return (n0, n1, n2)
    }

    /// XOR `length` bytes of ChaCha20 keystream (starting at `blockCounter`)
    /// between `input` and `output`. Buffers may alias (in-place operation).
    private static func xorKeystream(
        key: (UInt32, UInt32, UInt32, UInt32, UInt32, UInt32, UInt32, UInt32),
        nonce: (UInt32, UInt32, UInt32),
        blockCounter: UInt32,
        input: UnsafeRawPointer,
        output: UnsafeMutableRawPointer,
        length: Int
    ) {
        var keystream: (UInt32, UInt32, UInt32, UInt32, UInt32, UInt32, UInt32,
                        UInt32, UInt32, UInt32, UInt32, UInt32, UInt32, UInt32,
                        UInt32, UInt32) = (0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0)
        var counter = blockCounter
        var offset = 0
        var remaining = length
        while remaining > 0 {
            withUnsafeMutablePointer(to: &keystream) {
                $0.withMemoryRebound(to: UInt32.self, capacity: 16) { words in
                    block(key: key, counter: counter, nonce: nonce, out: words)
                }
            }
            let chunk = remaining < 64 ? remaining : 64
            withUnsafePointer(to: keystream) {
                $0.withMemoryRebound(to: UInt8.self, capacity: 64) { ks in
                    let src = input.advanced(by: offset).assumingMemoryBound(to: UInt8.self)
                    let dst = output.advanced(by: offset).assumingMemoryBound(to: UInt8.self)
                    var i = 0
                    while i < chunk {
                        dst[i] = src[i] ^ ks[i]
                        i += 1
                    }
                }
            }
            counter = counter &+ 1
            offset += chunk
            remaining -= chunk
        }
    }

    // MARK: - Poly1305 (donna-32 style, 5 x 26-bit limbs in UInt64)

    private struct Poly1305 {
        // h and r as 5 limbs of ≤26 bits held in UInt64 lanes; s (pad key)
        // as four little-endian words.
        var h0: UInt64 = 0
        var h1: UInt64 = 0
        var h2: UInt64 = 0
        var h3: UInt64 = 0
        var h4: UInt64 = 0
        var r0: UInt64 = 0
        var r1: UInt64 = 0
        var r2: UInt64 = 0
        var r3: UInt64 = 0
        var r4: UInt64 = 0
        var s1: UInt64 = 0
        var s2: UInt64 = 0
        var s3: UInt64 = 0
        var s4: UInt64 = 0

        mutating func setKey(_ k: UnsafePointer<UInt8>) {
            // RFC 8439 clamp on the byte string first, then the same
            // 0/26/52/78/104 limb split as message blocks.
            var c: (UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8,
                    UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8) =
                (0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0)
            withUnsafeMutablePointer(to: &c) {
                $0.withMemoryRebound(to: UInt8.self, capacity: 16) { cb in
                    var i = 0
                    while i < 16 {
                        cb[i] = k[i]
                        i += 1
                    }
                    cb[3] = cb[3] & 0x0F; cb[7] = cb[7] & 0x0F
                    cb[11] = cb[11] & 0x0F; cb[15] = cb[15] & 0x0F
                    cb[4] = cb[4] & 0xFC; cb[8] = cb[8] & 0xFC
                    cb[12] = cb[12] & 0xFC
                }
            }
            withUnsafePointer(to: c) {
                $0.withMemoryRebound(to: UInt8.self, capacity: 16) { cb in
                    let t0 = UInt64(cb[0]) | (UInt64(cb[1]) << 8) | (UInt64(cb[2]) << 16) | (UInt64(cb[3]) << 24)
                    let t1 = UInt64(cb[3]) | (UInt64(cb[4]) << 8) | (UInt64(cb[5]) << 16) | (UInt64(cb[6]) << 24)
                    let t2 = UInt64(cb[6]) | (UInt64(cb[7]) << 8) | (UInt64(cb[8]) << 16) | (UInt64(cb[9]) << 24)
                    let t3 = UInt64(cb[9]) | (UInt64(cb[10]) << 8) | (UInt64(cb[11]) << 16) | (UInt64(cb[12]) << 24)
                    let t4 = UInt64(cb[12]) | (UInt64(cb[13]) << 8) | (UInt64(cb[14]) << 16) | (UInt64(cb[15]) << 24)
                    // Disjoint bit ranges per limb: value-bits 0-25 / 26-51 /
                    // 52-77 / 78-103 / 104-127. Summing overlapping shifted
                    // loads here would double-count bits; each piece below
                    // covers exactly its limb's range.
                    r0 = t0 & 0x3FF_FFFF
                    r1 = (t0 >> 26) &+ (((t1 >> 8) & 0xF_FFFF) << 6)
                    r2 = (t1 >> 28) &+ (((t2 >> 8) & 0x3F_FFFF) << 4)
                    r3 = (t2 >> 30) &+ (((t3 >> 8) & 0xFF_FFFF) << 2)
                    r4 = (t4 >> 8) & 0x3FF_FFFF
                }
            }
            s1 = UInt64(k[16]) | (UInt64(k[17]) << 8) | (UInt64(k[18]) << 16) |
                (UInt64(k[19]) << 24)
            s2 = UInt64(k[20]) | (UInt64(k[21]) << 8) | (UInt64(k[22]) << 16) |
                (UInt64(k[23]) << 24)
            s3 = UInt64(k[24]) | (UInt64(k[25]) << 8) | (UInt64(k[26]) << 16) |
                (UInt64(k[27]) << 24)
            s4 = UInt64(k[28]) | (UInt64(k[29]) << 8) | (UInt64(k[30]) << 16) |
                (UInt64(k[31]) << 24)
        }

        /// Ingest one 16-byte block (message bytes with the RFC 8439 0x01
        /// terminator already placed at index `len` for partial blocks).
        /// Byte `i` contributes at value-bit `8*i`, split across the 26-bit
        /// limbs at boundaries 0/26/52/78/104.
        mutating func block16(_ m: UnsafePointer<UInt8>, fullBlock: Bool) {
            // Overlapping little-endian loads; byte i covers value-bits 8i.
            let t0 = UInt64(m[0]) | (UInt64(m[1]) << 8) | (UInt64(m[2]) << 16) | (UInt64(m[3]) << 24)
            let t1 = UInt64(m[3]) | (UInt64(m[4]) << 8) | (UInt64(m[5]) << 16) | (UInt64(m[6]) << 24)
            let t2 = UInt64(m[6]) | (UInt64(m[7]) << 8) | (UInt64(m[8]) << 16) | (UInt64(m[9]) << 24)
            let t3 = UInt64(m[9]) | (UInt64(m[10]) << 8) | (UInt64(m[11]) << 16) | (UInt64(m[12]) << 24)
            let t4 = UInt64(m[12]) | (UInt64(m[13]) << 8) | (UInt64(m[14]) << 16) | (UInt64(m[15]) << 24)
            // Split value-bits across 26-bit limbs at 0/26/52/78/104.
            // Each limb sums disjoint bit ranges only; every term stays
            // below 2^26 so plain UInt64 arithmetic cannot overflow, and
            // multiply() carries the accumulated sums.
            h0 = h0 &+ (t0 & 0x3FF_FFFF)
            h1 = h1 &+ ((t0 >> 26) &+ (((t1 >> 8) & 0xF_FFFF) << 6))
            h2 = h2 &+ ((t1 >> 28) &+ (((t2 >> 8) & 0x3F_FFFF) << 4))
            h3 = h3 &+ ((t2 >> 30) &+ (((t3 >> 8) & 0xFF_FFFF) << 2))
            h4 = h4 &+ (t4 >> 8)
            if fullBlock {
                h4 = h4 &+ (UInt64(1) << 24)
            }
            multiply()
        }

        mutating func blocks(_ message: UnsafeRawPointer, length: Int) {
            var offset = 0
            var remaining = length
            while remaining >= 16 {
                let m = message.advanced(by: offset).assumingMemoryBound(to: UInt8.self)
                block16(m, fullBlock: true)
                offset += 16
                remaining -= 16
            }
            if remaining > 0 {
                // Zero-padded block with the 0x01 terminator at `remaining`;
                // the bit-split loads place it at value-bit 8*remaining.
                var padded: (UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8,
                             UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8) =
                    (0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0)
                withUnsafeMutablePointer(to: &padded) {
                    $0.withMemoryRebound(to: UInt8.self, capacity: 16) { pad in
                        let m = message.advanced(by: offset).assumingMemoryBound(to: UInt8.self)
                        var i = 0
                        while i < remaining {
                            pad[i] = m[i]
                            i += 1
                        }
                        pad[remaining] = 1
                    }
                }
                withUnsafePointer(to: padded) {
                    $0.withMemoryRebound(to: UInt8.self, capacity: 16) { pad in
                        block16(pad, fullBlock: false)
                    }
                }
            }
        }

        /// Ingest `length` bytes with RFC 8439 section 2.8 `pad16` framing:
        /// full 16-byte chunks, then — when the length is not a multiple of
        /// 16 — the zero-padded tail as a final FULL block (2^128 added, not
        /// 2^(8*remainder)). The AEAD lengths block that follows always lands
        /// on a 16-byte boundary this way. `blocks()` instead treats a short
        /// tail as a trailing partial block, which is only correct at the
        /// very end of the MAC input.
        mutating func blocksPadded16(_ message: UnsafeRawPointer, length: Int) {
            var offset = 0
            var remaining = length
            while remaining >= 16 {
                let m = message.advanced(by: offset).assumingMemoryBound(to: UInt8.self)
                block16(m, fullBlock: true)
                offset += 16
                remaining -= 16
            }
            if remaining > 0 {
                var padded: (UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8,
                             UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8) =
                    (0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0)
                withUnsafeMutablePointer(to: &padded) {
                    $0.withMemoryRebound(to: UInt8.self, capacity: 16) { pad in
                        let m = message.advanced(by: offset).assumingMemoryBound(to: UInt8.self)
                        var i = 0
                        while i < remaining {
                            pad[i] = m[i]
                            i += 1
                        }
                    }
                }
                withUnsafePointer(to: padded) {
                    $0.withMemoryRebound(to: UInt8.self, capacity: 16) { pad in
                        block16(pad, fullBlock: true)
                    }
                }
            }
        }

        mutating func multiply() {
            // h *= r, fully carried. Products stay within UInt64: each factor
            // is < 2^27 after carrying (h) or < 2^27 (r limbs with top bits).
            let h0r = h0
            let h1r = h1
            let h2r = h2
            let h3r = h3
            let h4r = h4
            // r[1..4] have a high bit (2^24/2^25 positions); s1 = r1*5 etc.
            let s1 = r1 &* 5
            let s2 = r2 &* 5
            let s3 = r3 &* 5
            let s4 = r4 &* 5
            var d0 = (h0r &* r0) &+ (h1r &* s4) &+ (h2r &* s3) &+ (h3r &* s2) &+ (h4r &* s1)
            var d1 = (h0r &* r1) &+ (h1r &* r0) &+ (h2r &* s4) &+ (h3r &* s3) &+ (h4r &* s2)
            var d2 = (h0r &* r2) &+ (h1r &* r1) &+ (h2r &* r0) &+ (h3r &* s4) &+ (h4r &* s3)
            var d3 = (h0r &* r3) &+ (h1r &* r2) &+ (h2r &* r1) &+ (h3r &* r0) &+ (h4r &* s4)
            var d4 = (h0r &* r4) &+ (h1r &* r3) &+ (h2r &* r2) &+ (h3r &* r1) &+ (h4r &* r0)

            var c: UInt64
            c = d0 >> 26; d0 = d0 & 0x3FFFF_FF
            d1 = d1 &+ c; c = d1 >> 26; d1 = d1 & 0x3FFFF_FF
            d2 = d2 &+ c; c = d2 >> 26; d2 = d2 & 0x3FFFF_FF
            d3 = d3 &+ c; c = d3 >> 26; d3 = d3 & 0x3FFFF_FF
            d4 = d4 &+ c; c = d4 >> 26; d4 = d4 & 0x3FFFF_FF
            d0 = d0 &+ (c &* 5); c = d0 >> 26; d0 = d0 & 0x3FFFF_FF
            d1 = d1 &+ c

            h0 = d0; h1 = d1; h2 = d2; h3 = d3; h4 = d4
        }

        mutating func finish(tagOut: UnsafeMutableRawPointer) {
            // Full carry.
            var c = h1 >> 26; h1 = h1 & 0x3FF_FFFF
            h2 = h2 &+ c; c = h2 >> 26; h2 = h2 & 0x3FF_FFFF
            h3 = h3 &+ c; c = h3 >> 26; h3 = h3 & 0x3FF_FFFF
            h4 = h4 &+ c; c = h4 >> 26; h4 = h4 & 0x3FF_FFFF
            h0 = h0 &+ (c &* 5); c = h0 >> 26; h0 = h0 & 0x3FF_FFFF
            h1 = h1 &+ c

            // Freeze: g = h + -p; select h if h < p else g.
            var g0 = h0 &+ 5; c = g0 >> 26; g0 = g0 & 0x3FF_FFFF
            var g1 = h1 &+ c; c = g1 >> 26; g1 = g1 & 0x3FF_FFFF
            var g2 = h2 &+ c; c = g2 >> 26; g2 = g2 & 0x3FF_FFFF
            var g3 = h3 &+ c; c = g3 >> 26; g3 = g3 & 0x3FF_FFFF
            var g4 = h4 &+ c &- (UInt64(1) << 26)
            let mask: UInt64 = (g4 >> 63) &- 1
            g0 &= mask; g1 &= mask; g2 &= mask; g3 &= mask; g4 &= mask
            let nmask = ~mask
            h0 = (h0 & nmask) | g0
            h1 = (h1 & nmask) | g1
            h2 = (h2 & nmask) | g2
            h3 = (h3 & nmask) | g3
            h4 = (h4 & nmask) | g4

            // h is now fully reduced (all limbs < 2^26). Serialize the
            // 130-bit value little-endian: byte i covers value-bits 8i,
            // gathered from the 26-bit limbs. All shifts stay below 2^32.
            let b0 = h0 & 0xFF
            let b1 = (h0 >> 8) & 0xFF
            let b2 = (h0 >> 16) & 0xFF
            let b3 = ((h0 >> 24) | (h1 << 2)) & 0xFF
            let b4 = (h1 >> 6) & 0xFF
            let b5 = (h1 >> 14) & 0xFF
            let b6 = ((h1 >> 22) | (h2 << 4)) & 0xFF
            let b7 = (h2 >> 4) & 0xFF
            let b8 = (h2 >> 12) & 0xFF
            let b9 = ((h2 >> 20) | (h3 << 6)) & 0xFF
            let b10 = (h3 >> 2) & 0xFF
            let b11 = (h3 >> 10) & 0xFF
            let b12 = (h3 >> 18) & 0xFF
            let b13 = (h4 >> 0) & 0xFF
            let b14 = (h4 >> 8) & 0xFF
            let b15 = (h4 >> 16) & 0xFF

            // Add the s pad (four little-endian words) with carry, then emit.
            var t0 = b0 | (b1 << 8) | (b2 << 16) | (b3 << 24)
            var t1 = b4 | (b5 << 8) | (b6 << 16) | (b7 << 24)
            var t2 = b8 | (b9 << 8) | (b10 << 16) | (b11 << 24)
            var t3 = b12 | (b13 << 8) | (b14 << 16) | (b15 << 24)
            t0 = t0 &+ UInt64(s1); var cy = t0 >> 32; t0 &= 0xFFFF_FFFF
            t1 = t1 &+ UInt64(s2) &+ cy; cy = t1 >> 32; t1 &= 0xFFFF_FFFF
            t2 = t2 &+ UInt64(s3) &+ cy; cy = t2 >> 32; t2 &= 0xFFFF_FFFF
            t3 = t3 &+ UInt64(s4) &+ cy

            let out = tagOut.assumingMemoryBound(to: UInt8.self)
            out[0] = UInt8(t0 & 0xFF); out[1] = UInt8((t0 >> 8) & 0xFF)
            out[2] = UInt8((t0 >> 16) & 0xFF); out[3] = UInt8((t0 >> 24) & 0xFF)
            out[4] = UInt8(t1 & 0xFF); out[5] = UInt8((t1 >> 8) & 0xFF)
            out[6] = UInt8((t1 >> 16) & 0xFF); out[7] = UInt8((t1 >> 24) & 0xFF)
            out[8] = UInt8(t2 & 0xFF); out[9] = UInt8((t2 >> 8) & 0xFF)
            out[10] = UInt8((t2 >> 16) & 0xFF); out[11] = UInt8((t2 >> 24) & 0xFF)
            out[12] = UInt8(t3 & 0xFF); out[13] = UInt8((t3 >> 8) & 0xFF)
            out[14] = UInt8((t3 >> 16) & 0xFF); out[15] = UInt8((t3 >> 24) & 0xFF)
        }
    }

    // MARK: - AEAD seal/open (RFC 8439 section 2.8, empty AAD)

    /// Compute the authentication tag over `ciphertextLen` bytes of
    /// ciphertext with RFC 8439 pad16 framing and the lengths block
    /// (empty AAD).
    private static func tagFor(
        key: (UInt32, UInt32, UInt32, UInt32, UInt32, UInt32, UInt32, UInt32),
        nonce: (UInt32, UInt32, UInt32),
        ciphertext: UnsafeRawPointer,
        ciphertextLen: Int,
        tagOut: UnsafeMutableRawPointer
    ) {
        var polyKey: (UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8,
                      UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8,
                      UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8,
                      UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8) =
            (0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
             0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0)
        withUnsafeMutablePointer(to: &polyKey) {
            $0.withMemoryRebound(to: UInt8.self, capacity: 32) { pk in
                var words: (UInt32, UInt32, UInt32, UInt32, UInt32, UInt32, UInt32, UInt32,
                            UInt32, UInt32, UInt32, UInt32, UInt32, UInt32, UInt32, UInt32) =
                    (0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0)
                withUnsafeMutablePointer(to: &words) {
                    $0.withMemoryRebound(to: UInt32.self, capacity: 16) { w in
                        block(key: key, counter: 0, nonce: nonce, out: w)
                    }
                }
                withUnsafePointer(to: words) {
                    $0.withMemoryRebound(to: UInt8.self, capacity: 64) { wb in
                        var i = 0
                        while i < 32 {
                            pk[i] = wb[i]
                            i += 1
                        }
                    }
                }
            }
        }
        var mac = Poly1305()
        withUnsafePointer(to: polyKey) {
            $0.withMemoryRebound(to: UInt8.self, capacity: 32) { pk in
                mac.setKey(pk)
            }
        }
        mac.blocksPadded16(ciphertext, length: ciphertextLen)
        // Lengths block: aad_len (0) || ciphertext_len, little-endian.
        var lengths: (UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8,
                      UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8) =
            (0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0)
        let ctLen = UInt64(ciphertextLen)
        withUnsafeMutablePointer(to: &lengths) {
            $0.withMemoryRebound(to: UInt8.self, capacity: 16) { lb in
                var i = 0
                while i < 8 {
                    lb[8 + i] = UInt8((ctLen >> (8 * i)) & 0xFF)
                    i += 1
                }
            }
        }
        withUnsafePointer(to: lengths) {
            $0.withMemoryRebound(to: UInt8.self, capacity: 16) { lb in
                mac.blocks(UnsafeRawPointer(lb), length: 16)
            }
        }
        mac.finish(tagOut: tagOut)
    }

    /// Seal `byteCount` bytes in place: XOR the keystream over `bytes`, then
    /// write the 16-byte tag to `tagOut`. `tagOut` must not overlap `bytes`.
    static func sealInPlace(
        keyBytes: UnsafePointer<UInt8>,
        bytes: UnsafeMutableRawPointer,
        byteCount: Int,
        frameCounter: UInt64,
        tagOut: UnsafeMutableRawPointer
    ) {
        let key = loadKeyWords(keyBytes)
        let nonce = loadNonceWords(frameCounter: frameCounter)
        xorKeystream(
            key: key, nonce: nonce, blockCounter: 1,
            input: UnsafeRawPointer(bytes), output: bytes, length: byteCount
        )
        tagFor(
            key: key, nonce: nonce,
            ciphertext: UnsafeRawPointer(bytes), ciphertextLen: byteCount,
            tagOut: tagOut
        )
    }

    /// Seal `plaintextLen` bytes. `keyBytes` holds 32 key bytes.
    /// `ciphertextOut` must hold `plaintextLen` bytes, `tagOut` 16 bytes.
    /// Buffers may alias `plaintext` (in-place operation).
    static func seal(
        keyBytes: UnsafePointer<UInt8>,
        plaintext: UnsafeRawPointer,
        plaintextLen: Int,
        frameCounter: UInt64,
        ciphertextOut: UnsafeMutableRawPointer,
        tagOut: UnsafeMutableRawPointer
    ) {
        let key = loadKeyWords(keyBytes)
        let nonce = loadNonceWords(frameCounter: frameCounter)
        // Encrypt starting at block counter 1.
        xorKeystream(
            key: key, nonce: nonce, blockCounter: 1,
            input: plaintext, output: ciphertextOut, length: plaintextLen
        )
        tagFor(
            key: key, nonce: nonce,
            ciphertext: UnsafeRawPointer(ciphertextOut), ciphertextLen: plaintextLen,
            tagOut: tagOut
        )
    }

    /// Open `ciphertextLen` bytes with the appended 16-byte tag in `tag`.
    /// Writes `ciphertextLen` plaintext bytes to `plaintextOut` (may alias
    /// `ciphertext`). Returns true only if the tag verifies (constant-time
    /// compare); on failure `plaintextOut` is left untouched.
    static func open(
        keyBytes: UnsafePointer<UInt8>,
        ciphertext: UnsafeRawPointer,
        ciphertextLen: Int,
        frameCounter: UInt64,
        tag: UnsafeRawPointer,
        plaintextOut: UnsafeMutableRawPointer
    ) -> Bool {
        let key = loadKeyWords(keyBytes)
        let nonce = loadNonceWords(frameCounter: frameCounter)
        var expected: (UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8,
                       UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8) =
            (0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0)
        withUnsafeMutablePointer(to: &expected) {
            $0.withMemoryRebound(to: UInt8.self, capacity: 16) { eb in
                tagFor(
                    key: key, nonce: nonce,
                    ciphertext: ciphertext, ciphertextLen: ciphertextLen,
                    tagOut: UnsafeMutableRawPointer(eb)
                )
            }
        }
        // Constant-time tag compare.
        var difference: UInt8 = 0
        withUnsafePointer(to: expected) {
            $0.withMemoryRebound(to: UInt8.self, capacity: 16) { eb in
                let tb = tag.assumingMemoryBound(to: UInt8.self)
                var i = 0
                while i < 16 {
                    difference |= eb[i] ^ tb[i]
                    i += 1
                }
            }
        }
        guard difference == 0 else { return false }
        xorKeystream(
            key: key, nonce: nonce, blockCounter: 1,
            input: ciphertext, output: plaintextOut, length: ciphertextLen
        )
        return true
    }
}
