#ifndef SOTF_TEST_ALLOCATION_OBSERVER_H
#define SOTF_TEST_ALLOCATION_OBSERVER_H
#include <stdint.h>

typedef struct {
    uint64_t allocations;
    uint64_t deallocations;
} SotFTestAllocationCounts;

void sotf_test_allocation_begin(void);
SotFTestAllocationCounts sotf_test_allocation_end(void);
void sotf_test_escape_pointer(const void *pointer);
// UINT64_MAX reports an unsupported or failed thread clock.
uint64_t sotf_test_thread_cpu_ns(void);
#endif
