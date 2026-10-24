// Bridging header for SotF HAL Driver
// Exposes the CoreAudio AudioServerPlugIn types to Swift

#ifndef BridgingHeader_h
#define BridgingHeader_h

#include <CoreAudio/AudioServerPlugIn.h>
#include <CoreFoundation/CoreFoundation.h>
#include <mach/mach_time.h>
#include <stdatomic.h>
#include <stdint.h>
#include "SharedMemoryDescriptor.h"

static inline uint32_t sotf_atomic_load_u32(const uint32_t *ptr) {
    return atomic_load_explicit((const _Atomic uint32_t *)ptr, memory_order_acquire);
}

static inline uint64_t sotf_atomic_load_u64(const uint64_t *ptr) {
    return atomic_load_explicit((const _Atomic uint64_t *)ptr, memory_order_acquire);
}

static inline void sotf_atomic_store_u32(uint32_t *ptr, uint32_t value) {
    atomic_store_explicit((_Atomic uint32_t *)ptr, value, memory_order_release);
}

static inline void sotf_atomic_store_u64(uint64_t *ptr, uint64_t value) {
    atomic_store_explicit((_Atomic uint64_t *)ptr, value, memory_order_release);
}

static inline uint64_t sotf_atomic_fetch_add_u64_previous(uint64_t *ptr, uint64_t value) {
    return atomic_fetch_add_explicit((_Atomic uint64_t *)ptr, value, memory_order_acq_rel);
}

static inline uint64_t sotf_atomic_fetch_add_u64(uint64_t *ptr, uint64_t value) {
    return sotf_atomic_fetch_add_u64_previous(ptr, value) + value;
}

// A cipher slot is either pinned by callbacks or exclusively owned by the
// maintenance writer. This also excludes readers delayed after activeSlot loads.
static inline bool sotf_cipher_try_write(uint64_t *ptr) {
    uint64_t expected = 0;
    return atomic_compare_exchange_strong_explicit((_Atomic uint64_t *)ptr,
        &expected, UINT64_MAX, memory_order_acq_rel, memory_order_acquire);
}

static inline void sotf_cipher_end_write(uint64_t *ptr) {
    atomic_store_explicit((_Atomic uint64_t *)ptr, 0, memory_order_release);
}

static inline bool sotf_cipher_try_pin(uint64_t *ptr) {
    uint64_t expected = atomic_load_explicit((_Atomic uint64_t *)ptr, memory_order_acquire);
    for (int attempt = 0; attempt < 3; ++attempt) {
        if (expected >= UINT64_MAX - 1) return false;
        if (atomic_compare_exchange_strong_explicit((_Atomic uint64_t *)ptr,
            &expected, expected + 1, memory_order_acq_rel, memory_order_acquire)) return true;
    }
    return false;
}

static inline void sotf_atomic_fetch_sub_u64(uint64_t *ptr, uint64_t value) {
    atomic_fetch_sub_explicit((_Atomic uint64_t *)ptr, value, memory_order_acq_rel);
}

static inline void sotf_atomic_clear_u32(uint32_t *ptr, uint32_t mask) {
    atomic_fetch_and_explicit((_Atomic uint32_t *)ptr, ~mask, memory_order_acq_rel);
}

// Clock snapshots use one sequentially consistent order for the sequence and
// every payload word. Readers never access a concurrently written plain field.
static inline uint64_t sotf_clock_load(const uint64_t *ptr) {
    return atomic_load_explicit((const _Atomic uint64_t *)ptr, memory_order_seq_cst);
}

static inline void sotf_clock_store(uint64_t *ptr, uint64_t value) {
    atomic_store_explicit((_Atomic uint64_t *)ptr, value, memory_order_seq_cst);
}

static inline bool sotf_atomic_compare_exchange_u32(uint32_t *ptr, uint32_t expected, uint32_t desired) {
    return atomic_compare_exchange_strong_explicit(
        (_Atomic uint32_t *)ptr,
        &expected,
        desired,
        memory_order_acq_rel,
        memory_order_acquire
    );
}

static inline bool sotf_atomic_compare_exchange_u64(uint64_t *ptr, uint64_t expected, uint64_t desired) {
    return atomic_compare_exchange_strong_explicit(
        (_Atomic uint64_t *)ptr, &expected, desired,
        memory_order_acq_rel, memory_order_acquire
    );
}

static inline uint32_t sotf_atomic_exchange_u32(uint32_t *ptr, uint32_t value) {
    return atomic_exchange_explicit((_Atomic uint32_t *)ptr, value, memory_order_acq_rel);
}

#endif /* BridgingHeader_h */
