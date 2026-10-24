// Linked only into the opt-in host callback measurement executable.
// Static dyld interposing preserves calls to the original functions from this
// image. See apple-oss-distributions/dyld, DyldRuntimeState.h InterposeTupleSpecific.
#include "TestAllocationObserver.h"
#include <malloc/malloc.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdlib.h>
#include <time.h>

static _Thread_local bool observing;
static _Thread_local unsigned depth;
static _Thread_local SotFTestAllocationCounts counts;

void sotf_test_escape_pointer(const void *pointer) {
    __asm__ volatile("" : : "r"(pointer) : "memory");
}

uint64_t sotf_test_thread_cpu_ns(void) {
    struct timespec value;
    if (clock_gettime(CLOCK_THREAD_CPUTIME_ID, &value) != 0) return UINT64_MAX;
    return (uint64_t)value.tv_sec * 1000000000ULL + (uint64_t)value.tv_nsec;
}

void sotf_test_allocation_begin(void) {
    // Initialize TLS outside the observed callback window.
    observing = false;
    depth = 0;
    counts = (SotFTestAllocationCounts){0, 0};
    observing = true;
}

SotFTestAllocationCounts sotf_test_allocation_end(void) {
    observing = false;
    return counts;
}

static void enter(bool allocation) {
    if (observing && depth == 0) {
        if (allocation) ++counts.allocations;
        else ++counts.deallocations;
    }
    ++depth;
}
static void leave(void) { --depth; }

static void *observed_malloc(size_t size) {
    enter(true); void *result = malloc(size); leave(); return result;
}
static void *observed_calloc(size_t count, size_t size) {
    enter(true); void *result = calloc(count, size); leave(); return result;
}
static void *observed_realloc(void *pointer, size_t size) {
    enter(true); void *result = realloc(pointer, size); leave(); return result;
}
static void observed_free(void *pointer) {
    if (pointer) { enter(false); free(pointer); leave(); }
}
static int observed_posix_memalign(void **pointer, size_t alignment, size_t size) {
    enter(true); int result = posix_memalign(pointer, alignment, size); leave(); return result;
}
static void *observed_aligned_alloc(size_t alignment, size_t size) {
    enter(true); void *result = aligned_alloc(alignment, size); leave(); return result;
}
static void *observed_valloc(size_t size) {
    enter(true); void *result = valloc(size); leave(); return result;
}
static void *observed_zone_malloc(malloc_zone_t *zone, size_t size) {
    enter(true); void *result = malloc_zone_malloc(zone, size); leave(); return result;
}
static void *observed_zone_calloc(malloc_zone_t *zone, size_t count, size_t size) {
    enter(true); void *result = malloc_zone_calloc(zone, count, size); leave(); return result;
}
static void *observed_zone_realloc(malloc_zone_t *zone, void *pointer, size_t size) {
    enter(true); void *result = malloc_zone_realloc(zone, pointer, size); leave(); return result;
}
static void *observed_zone_memalign(malloc_zone_t *zone, size_t alignment, size_t size) {
    enter(true); void *result = malloc_zone_memalign(zone, alignment, size); leave(); return result;
}
static void observed_zone_free(malloc_zone_t *zone, void *pointer) {
    if (pointer) { enter(false); malloc_zone_free(zone, pointer); leave(); }
}

// The two addresses are the ABI of dyld's static __interpose section.
#define OBSERVE(replacement, original) \
    __attribute__((used)) static const struct { const void *new_function; const void *old_function; } \
    interpose_##original __attribute__((section("__DATA,__interpose"))) = { \
        (const void *)(replacement), (const void *)(original) \
    }

OBSERVE(observed_malloc, malloc);
OBSERVE(observed_calloc, calloc);
OBSERVE(observed_realloc, realloc);
OBSERVE(observed_free, free);
OBSERVE(observed_posix_memalign, posix_memalign);
OBSERVE(observed_aligned_alloc, aligned_alloc);
OBSERVE(observed_valloc, valloc);
OBSERVE(observed_zone_malloc, malloc_zone_malloc);
OBSERVE(observed_zone_calloc, malloc_zone_calloc);
OBSERVE(observed_zone_realloc, malloc_zone_realloc);
OBSERVE(observed_zone_memalign, malloc_zone_memalign);
OBSERVE(observed_zone_free, malloc_zone_free);
