/* SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception */
/* Runtime test for tasks (coroutines on worker threads) and channels.
 * Run (from compiler/runtime):
 *   clang -O1 -g -pthread -I. -Ilibtommath -o /tmp/tasks_test tests/tasks_test.c alx.c alx_big.c -lm && /tmp/tasks_test
 * (add -fsanitize=address or -fsanitize=thread to taste). Exits 0 on success.
 * Also run it with ALX_PROCS=1 (every task on one worker).
 * The deadlock and overflow cases abort the process by design, so they only run
 * with `tasks_test deadlock` (expect "all tasks are asleep: deadlock") and
 * `tasks_test overflow` (expect "stack overflow in a task"). */
#include "../alx.h"

#include <time.h>
#include <unistd.h>

#include <assert.h>

#define CHECK(c) do { if (!(c)) { fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, #c); exit(1); } } while (0)

typedef struct { AlxChan *ch; int64_t n; } Env;

static void producer(const void *in, void *out) {
    Env e = *(const Env *)in;
    for (int64_t i = 1; i <= e.n; i++) { int64_t v = i; alx_chan_send(e.ch, &v, "t.alx:1:1"); }
    alx_chan_close(e.ch, "t.alx:2:1");
    *(int64_t *)out = 42;
}

static void panicker(const void *in, void *out) {
    (void)in; (void)out;
    alx_panic("index out of bounds", "f.alx:3:5");
}

static void overflower(const void *in, void *out) {
    (void)in; (void)out;
    alx_overflow("g.alx:1:2");
}

static void send_closed(const void *in, void *out) {
    AlxChan *c = *(AlxChan *const *)in;
    int64_t v = 1;
    (void)out;
    alx_chan_send(c, &v, "h.alx:9:9");
}

static void self_deadlock(const void *in, void *out) {
    AlxChan *c = *(AlxChan *const *)in;
    int64_t v;
    (void)out;
    alx_chan_recv(c, &v);
}

/* Unbuffered: records the order of events. */
static volatile int64_t g_flag;
static void slow_receiver(const void *in, void *out) {
    AlxChan *c = *(AlxChan *const *)in;
    int64_t v;
    (void)out;
    usleep(100 * 1000);
    g_flag = 1; /* set before the receive that releases the sender */
    CHECK(alx_chan_recv(c, &v) && v == 7);
}

static void sel_sender(const void *in, void *out) {
    AlxChan *c = *(AlxChan *const *)in;
    int64_t v = 5;
    AlxSelCase cs[1] = {{ c, &v, 1, 0 }};
    (void)out;
    CHECK(alx_select(cs, 1, false, "select") == 0);
}

/* Daisy chain: task i receives from ch[i], adds 1, sends to ch[i+1]. */
typedef struct { AlxChan *in, *out; } Link;
static void link_task(const void *in, void *out) {
    Link l = *(const Link *)in;
    int64_t v;
    (void)out;
    CHECK(alx_chan_recv(l.in, &v));
    v++;
    alx_chan_send(l.out, &v, "chain");
}

/* Tasks that enter a region, allocate, park, and check everything on resume. */
typedef struct { AlxChan *go, *done; int64_t id; } Rg;
static void region_task(const void *in, void *out) {
    Rg g = *(const Rg *)in;
    AlxRegion *prog = alx_region_cur();   /* the worker's program region */
    char *pa = alx_alloc(100); memset(pa, (int)g.id, 100);
    AlxRegion *saved = alx_region_cur(), *r = alx_region_enter();
    char *a = alx_alloc(5000); memset(a, (int)g.id + 1, 5000);
    char *big = alx_alloc(3u << 20); memset(big, (int)g.id + 2, 3u << 20);   /* large block */
    AlxRegion *child = alx_region_new_child(r);
    int64_t v;
    (void)out;
    CHECK(alx_chan_recv(g.go, &v));                       /* parks; other tasks run here */
    CHECK(alx_region_cur() == r && alx_region_of(a) == r && alx_region_of(big) == r);
    for (int i = 0; i < 5000; i++) CHECK(a[i] == (char)(g.id + 1));
    CHECK(big[0] == (char)(g.id + 2) && big[(3u << 20) - 1] == (char)(g.id + 2));
    char *b = alx_alloc(70000); memset(b, (int)g.id + 3, 70000);  /* new chunk in r */
    AlxRegion *sv = alx_region_use(child);
    char *c = alx_alloc(64); memset(c, 9, 64);
    alx_chan_send(g.done, &v, "x");                       /* park again, child current */
    CHECK(alx_region_cur() == child && c[63] == 9);
    alx_region_set(sv);
    for (int i = 0; i < 5000; i++) CHECK(a[i] == (char)(g.id + 1));
    for (int i = 0; i < 70000; i++) CHECK(b[i] == (char)(g.id + 3));
    alx_region_exit(r, saved);
    CHECK(alx_region_cur() == prog);
    for (int i = 0; i < 100; i++) CHECK(pa[i] == (char)g.id);
}

static int64_t g_depth;
static int64_t recurse(int64_t n) {
    volatile char pad[512];
    pad[0] = (char)n;
    g_depth = n;
    return n < 0 ? 0 : recurse(n + 1) + pad[0];
}
static void overflower_stack(const void *in, void *out) { (void)in; (void)out; recurse(0); }

int main(int argc, char **argv) {
    alx_init();
    int64_t v = 0;

    /* buffered producer/consumer */
    {
        AlxChan *ch = alx_chan_new(2, sizeof(int64_t));
        Env e = { ch, 10 };
        AlxTask *t = alx_spawn(producer, &e, sizeof e, sizeof(int64_t));
        int64_t sum = 0;
        while (alx_chan_recv(ch, &v)) sum += v;
        CHECK(sum == 55);
        int64_t r = 0;
        AlxStr msg;
        CHECK(alx_task_wait(t, &r, &msg) && r == 42);
        r = 0;
        CHECK(alx_task_wait(t, &r, &msg) && r == 42); /* waiting twice */
        CHECK(!alx_chan_recv(ch, &v)); /* closed and drained */
    }
    /* unbuffered producer/consumer */
    {
        AlxChan *ch = alx_chan_new(0, sizeof(int64_t));
        Env e = { ch, 1000 };
        AlxTask *t = alx_spawn(producer, &e, sizeof e, sizeof(int64_t));
        int64_t sum = 0;
        while (alx_chan_recv(ch, &v)) sum += v;
        CHECK(sum == 500500);
        int64_t r;
        AlxStr msg;
        CHECK(alx_task_wait(t, &r, &msg));
    }
    /* rendezvous: the send returns only after the receive */
    {
        AlxChan *ch = alx_chan_new(0, sizeof(int64_t));
        g_flag = 0;
        AlxTask *t = alx_spawn(slow_receiver, &ch, sizeof ch, 1);
        v = 7;
        alx_chan_send(ch, &v, "t.alx:1:1");
        CHECK(g_flag == 1);
        AlxStr msg;
        char r;
        CHECK(alx_task_wait(t, &r, &msg));
    }
    /* len */
    {
        AlxChan *ch = alx_chan_new(3, sizeof(int64_t));
        v = 1; alx_chan_send(ch, &v, "x"); alx_chan_send(ch, &v, "x");
        CHECK(alx_chan_len(ch) == 2);
        CHECK(alx_chan_recv(ch, &v));
        CHECK(alx_chan_len(ch) == 1);
    }
    /* select with default / ready / closed */
    {
        AlxChan *a = alx_chan_new(1, sizeof(int64_t)), *b = alx_chan_new(1, sizeof(int64_t));
        int64_t ra = -1, rb = -1;
        AlxSelCase cs[2] = {{ a, &ra, 0, 0 }, { b, &rb, 0, 0 }};
        CHECK(alx_select(cs, 2, true, "select") == 2); /* nothing ready: default */
        v = 9;
        alx_chan_send(b, &v, "x");
        CHECK(alx_select(cs, 2, true, "select") == 1 && cs[1].ok == 1 && rb == 9);
        /* send case on a buffered channel with room */
        int64_t sv = 3;
        AlxSelCase ss[1] = {{ a, &sv, 1, 0 }};
        CHECK(alx_select(ss, 1, true, "select") == 0);
        CHECK(alx_chan_len(a) == 1);
        CHECK(alx_select(ss, 1, true, "select") == 1); /* full: default */
        /* fairness: both ready, both get picked */
        int hits[2] = {0, 0};
        for (int i = 0; i < 200; i++) {
            int64_t x = 1;
            AlxSelCase two[2] = {{ a, &ra, 0, 0 }, { b, &rb, 0, 0 }};
            if (alx_chan_len(b) == 0) alx_chan_send(b, &x, "x");
            if (alx_chan_len(a) == 0) alx_chan_send(a, &x, "x");
            hits[alx_select(two, 2, false, "select")]++;
        }
        CHECK(hits[0] > 20 && hits[1] > 20);
        /* closed channel: recv case is ready with ok = 0 */
        AlxChan *c = alx_chan_new(0, sizeof(int64_t));
        alx_chan_close(c, "x");
        AlxSelCase cc[1] = {{ c, &ra, 0, 1 }};
        CHECK(alx_select(cc, 1, false, "select") == 0 && cc[0].ok == 0);
    }
    /* select blocks until ready: unbuffered, sender is a select too */
    {
        AlxChan *c = alx_chan_new(0, sizeof(int64_t));
        AlxTask *t = alx_spawn(sel_sender, &c, sizeof c, 1);
        int64_t r = 0;
        AlxSelCase cs[1] = {{ c, &r, 0, 0 }};
        CHECK(alx_select(cs, 1, false, "select") == 0 && cs[0].ok == 1 && r == 5);
        AlxStr msg;
        char o;
        CHECK(alx_task_wait(t, &o, &msg));
    }
    /* panics in tasks */
    {
        AlxStr msg;
        char o;
        AlxTask *t = alx_spawn(panicker, NULL, 0, 1);
        CHECK(!alx_task_wait(t, &o, &msg));
        CHECK(msg.len == (int64_t)strlen("alexandrite: index out of bounds at f.alx:3:5"));
        CHECK(memcmp(msg.ptr, "alexandrite: index out of bounds at f.alx:3:5", (size_t)msg.len) == 0);
        CHECK(!alx_task_wait(t, &o, &msg)); /* again */
        t = alx_spawn(overflower, NULL, 0, 1);
        CHECK(!alx_task_wait(t, &o, &msg));
        CHECK(strstr(msg.ptr, "alexandrite: overflow at g.alx:1:2\nhint:") == msg.ptr);
        AlxChan *c = alx_chan_new(1, sizeof(int64_t));
        alx_chan_close(c, "x");
        t = alx_spawn(send_closed, &c, sizeof c, 1);
        CHECK(!alx_task_wait(t, &o, &msg));
        CHECK(strcmp(msg.ptr, "alexandrite: send on a closed channel at h.alx:9:9") == 0);
    }
    /* a send blocked on an unbuffered channel panics when it is closed */
    {
        AlxChan *c = alx_chan_new(0, sizeof(int64_t));
        Env e = { c, 5 };
        AlxTask *t = alx_spawn(producer, &e, sizeof e, sizeof(int64_t));
        usleep(50 * 1000);
        alx_chan_close(c, "x");
        AlxStr msg;
        int64_t r;
        CHECK(!alx_task_wait(t, &r, &msg));
        CHECK(strstr(msg.ptr, "send on a closed channel") != NULL);
    }
    /* region state follows each task across parks (tasks interleave on workers) */
    {
        enum { N = 24 };
        AlxChan *go = alx_chan_new(0, sizeof(int64_t)), *done = alx_chan_new(0, sizeof(int64_t));
        AlxTask *ts[N];
        for (int i = 0; i < N; i++) { Rg g = { go, done, i + 1 }; ts[i] = alx_spawn(region_task, &g, sizeof g, 1); }
        for (int i = 0; i < N; i++) { v = i; alx_chan_send(go, &v, "x"); }
        for (int i = 0; i < N; i++) CHECK(alx_chan_recv(done, &v));
        for (int i = 0; i < N; i++) { AlxStr msg; char o; CHECK(alx_task_wait(ts[i], &o, &msg)); }
    }
    /* daisy chain of 100000 tasks */
    {
#if defined(__SANITIZE_THREAD__) || (defined(__has_feature) && __has_feature(thread_sanitizer))
        enum { N = 5000 };   /* TSan fibers are slow to create */
#else
        enum { N = 100000 };
#endif
        struct timespec t0, t1;
        clock_gettime(CLOCK_MONOTONIC, &t0);
        AlxChan **chs = malloc((N + 1) * sizeof *chs);
        AlxTask **ts = malloc(N * sizeof *ts);
        for (int i = 0; i <= N; i++) chs[i] = alx_chan_new(0, sizeof(int64_t));
        for (int i = 0; i < N; i++) { Link l = { chs[i], chs[i + 1] }; ts[i] = alx_spawn(link_task, &l, sizeof l, 1); }
        v = 0;
        alx_chan_send(chs[0], &v, "chain");
        int64_t r = -1;
        CHECK(alx_chan_recv(chs[N], &r) && r == N);
        for (int i = 0; i < N; i++) { AlxStr msg; char o; CHECK(alx_task_wait(ts[i], &o, &msg)); }
        clock_gettime(CLOCK_MONOTONIC, &t1);
        fprintf(stderr, "chain of %d: %.0f ms\n", N, (double)(t1.tv_sec - t0.tv_sec) * 1e3 + (double)(t1.tv_nsec - t0.tv_nsec) / 1e6);
    }
    if (argc > 1 && !strcmp(argv[1], "overflow")) {
        AlxTask *t = alx_spawn(overflower_stack, NULL, 0, 1);
        AlxStr msg;
        char o;
        alx_task_wait(t, &o, &msg);
    }
    if (argc > 1 && !strcmp(argv[1], "deadlock")) {
        AlxChan *c = alx_chan_new(0, sizeof(int64_t));
        AlxTask *t = alx_spawn(self_deadlock, &c, sizeof c, 1);
        AlxStr msg;
        char o;
        alx_task_wait(t, &o, &msg); /* the task's recv and this wait: nobody can run */
        printf("deadlock task msg: %s\n", msg.ptr);
        alx_chan_recv(c, &v); /* main blocks forever: deadlock panic, aborts */
    }
    puts("ok");
    return 0;
}
