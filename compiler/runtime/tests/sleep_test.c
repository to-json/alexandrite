/* Sleeping tasks park instead of blocking their worker.
 *   clang -O1 -g -pthread -I. -Ilibtommath -o /tmp/sleep_test tests/sleep_test.c alx.c alx_big.c -lm && ALX_PROCS=1 /tmp/sleep_test */
#include "alx.h"
#include <stdio.h>
#include <time.h>

static void napper(const void *env, void *res) {
    int64_t ms = *(const int64_t *)env;
    alx_sleep_ns(ms * 1000000);
    *(int64_t *)res = ms;
}

int main(void) {
    alx_init();
    struct timespec a, b;
    clock_gettime(CLOCK_MONOTONIC, &a);
    /* 200 tasks sleeping 50 ms each on one worker: ~50 ms if they park. */
    AlxTask *ts[200];
    for (int64_t i = 0; i < 200; i++) {
        int64_t ms = 50;
        ts[i] = alx_spawn(napper, &ms, sizeof ms, sizeof(int64_t));
    }
    int64_t sum = 0;
    for (int i = 0; i < 200; i++) {
        int64_t r;
        AlxStr msg;
        if (!alx_task_wait(ts[i], &r, &msg)) return 1;
        sum += r;
    }
    clock_gettime(CLOCK_MONOTONIC, &b);
    double ms = (b.tv_sec - a.tv_sec) * 1e3 + (b.tv_nsec - a.tv_nsec) / 1e6;
    printf("200 sleepers: %.1f ms, sum %lld\n", ms, (long long)sum);
    if (sum != 200 * 50 || ms > 500) return 1;
    alx_sleep_ns(1000000);
    puts("ok");
    return 0;
}
