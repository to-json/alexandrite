/* SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception */
/* Runtime test for tasks and channels.
 * Run (from compiler/runtime):
 *   clang -O1 -g -pthread -I. -Ilibtommath -o /tmp/tasks_test tests/tasks_test.c alx.c alx_big.c -lm && /tmp/tasks_test
 * (add -fsanitize=address or -fsanitize=thread to taste). Exits 0 on success.
 * The deadlock case aborts the process by design, so it only runs with
 * `tasks_test deadlock` (expect "all tasks are asleep: deadlock"). */
#include "../alx.h"

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
