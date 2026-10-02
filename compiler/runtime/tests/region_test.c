/* SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception */
/* Runtime test for memory regions.
 * Run (from compiler/runtime):
 *   clang -O1 -g -pthread -I. -Ilibtommath -o /tmp/region_test tests/region_test.c alx.c alx_big.c -lm && /tmp/region_test
 * (add -fsanitize=address to taste). Exits 0 on success. */
#include "../alx.h"

#include <pthread.h>

#define CHECK(c) do { if (!(c)) { fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, #c); exit(1); } } while (0)

static void fill(char *p, size_t n, int v) { memset(p, v, n); }
static int all(const char *p, size_t n, int v) { for (size_t i = 0; i < n; i++) if (p[i] != (char)v) return 0; return 1; }

static void nested(void) {
    AlxRegion *prog = alx_region_program();
    CHECK(alx_region_cur() == prog);
    char *a = alx_alloc(100); fill(a, 100, 1);
    AlxRegion *s1 = alx_region_cur(), *r1 = alx_region_enter();
    CHECK(s1 == prog && alx_region_cur() == r1 && r1 != prog);
    char *b = alx_alloc(200); fill(b, 200, 2);
    AlxRegion *s2 = alx_region_cur(), *r2 = alx_region_enter();
    CHECK(s2 == r1);
    char *c = alx_alloc(300); fill(c, 300, 3);
    (void)c;
    /* allocate in the outer region while the inner one is current */
    AlxRegion *sv = alx_region_use(prog);
    CHECK(sv == r2 && alx_region_cur() == prog);
    char *d = alx_alloc(400); fill(d, 400, 4);
    alx_region_set(sv);
    CHECK(alx_region_cur() == r2);
    char *c2 = alx_alloc(50); fill(c2, 50, 5);
    alx_region_exit(r2, s2);
    CHECK(alx_region_cur() == r1);
    CHECK(all(b, 200, 2) && all(d, 400, 4) && all(a, 100, 1));
    char *b2 = alx_alloc(64); fill(b2, 64, 6);
    CHECK(all(b, 200, 2));
    alx_region_exit(r1, s1);
    CHECK(alx_region_cur() == prog);
    CHECK(all(a, 100, 1) && all(d, 400, 4));
    char *e = alx_alloc(100); fill(e, 100, 7);
    CHECK(all(a, 100, 1) && all(d, 400, 4));
}

static void bounded(void) {
    AlxRegion *prog = alx_region_program();
    size_t base = alx_mem_held();
    for (int i = 0; i < 2000; i++) {
        AlxRegion *s = alx_region_cur(), *r = alx_region_enter();
        for (int j = 0; j < 50; j++) fill(alx_alloc(40000), 40000, j);   /* ~2 MB: chunks */
        fill(alx_alloc(1 << 20), 1 << 20, 1);                            /* large */
        alx_region_exit(r, s);
        CHECK(alx_region_cur() == prog);
    }
    CHECK(alx_mem_held() <= base + (8u << 20));
    CHECK(alx_mem_peak() <= base + (16u << 20));
    /* empty regions cost nothing */
    size_t h0 = alx_mem_held();
    for (int i = 0; i < 100000; i++) { AlxRegion *s = alx_region_cur(), *r = alx_region_enter(); alx_region_exit(r, s); }
    CHECK(alx_mem_held() == h0);
}

static void large(void) {
    size_t h0 = alx_mem_held();
    AlxRegion *s = alx_region_cur(), *r = alx_region_enter();
    char *p = alx_alloc(10u << 20); fill(p, 10u << 20, 9);
    CHECK(alx_mem_held() >= h0 + (10u << 20));
    alx_region_exit(r, s);
    CHECK(alx_mem_held() <= h0);
}

static void *thr(void *arg) {
    (void)arg;
    AlxRegion *prog = alx_region_program();
    CHECK(alx_region_cur() == prog);
    for (int i = 0; i < 300; i++) {
        AlxRegion *s = alx_region_cur(), *r = alx_region_enter();
        char *p = alx_alloc(5000); fill(p, 5000, i);
        AlxRegion *sv = alx_region_use(prog);
        char *q = alx_alloc(16); fill(q, 16, 3);
        alx_region_set(sv);
        CHECK(all(p, 5000, i));
        alx_region_exit(r, s);
    }
    return NULL;
}

/* ---- alx_region_of ---- */
static char g_buf[64];
static const char *g_lit = "a string literal";

static void of_basic(void) {
    AlxRegion *prog = alx_region_program();
    char stack[64]; void *m = malloc(100);
    CHECK(alx_region_of(NULL) == prog && alx_region_of(g_lit) == prog);
    CHECK(alx_region_of(stack) == prog && alx_region_of(m) == prog && alx_region_of(g_buf) == prog);
    char *pa = alx_alloc(100);
    CHECK(alx_region_of(pa) == prog);
    AlxRegion *s = alx_region_cur(), *r = alx_region_enter();
    char *a = alx_alloc(100), *b = alx_alloc(1000);
    CHECK(alx_region_of(a) == r && alx_region_of(a + 99) == r && alx_region_of(b + 500) == r);
    /* run through several chunks, checking first and last byte of each allocation */
    for (int i = 0; i < 40000; i++) {
        char *last = alx_alloc(64);
        CHECK(alx_region_of(last) == r && alx_region_of(last + 63) == r);
    }
    /* allocate in the program region while r is current */
    AlxRegion *sv = alx_region_use(prog);
    char *q = alx_alloc(100);
    alx_region_set(sv);
    CHECK(alx_region_of(q) == prog && alx_region_of(a) == r);
    /* nested */
    AlxRegion *s2 = alx_region_cur(), *r2 = alx_region_enter();
    char *c = alx_alloc(100);
    CHECK(alx_region_of(c) == r2 && alx_region_of(a) == r && r2 != r);
    alx_region_exit(r2, s2);
    CHECK(alx_region_of(c) == prog);          /* freed: unregistered */
    CHECK(alx_region_of(a) == r);
    alx_region_exit(r, s);
    CHECK(alx_region_of(a) == prog && alx_region_of(pa) == prog && alx_region_of(q) == prog);
    free(m);
}

static void of_large(void) {
    AlxRegion *prog = alx_region_program();
    AlxRegion *s = alx_region_cur(), *r = alx_region_enter();
    size_t n = 10u << 20;
    char *p = alx_alloc(n); fill(p, n, 1);
    char *small = alx_alloc(80);
    CHECK(alx_region_of(p) == r && alx_region_of(p + n - 1) == r && alx_region_of(small) == r);
    for (size_t o = 0; o < n; o += 4099) CHECK(alx_region_of(p + o) == r);
    char *p2 = alx_alloc(100000);     /* sub-granule large block */
    CHECK(alx_region_of(p2) == r && alx_region_of(p2 + 99999) == r);
    alx_region_exit(r, s);
    for (size_t o = 0; o < n; o += 4099) CHECK(alx_region_of(p + o) == prog);
    CHECK(alx_region_of(p2 + 5) == prog);
}

static void of_reuse(void) {
    AlxRegion *prog = alx_region_program();
    for (int round = 0; round < 6; round++) {
        AlxRegion *s = alx_region_cur(), *r1 = alx_region_enter();
        char *a[8];
        for (int i = 0; i < 8; i++) { a[i] = alx_alloc(300000); CHECK(alx_region_of(a[i]) == r1); }
        alx_region_exit(r1, s);
        for (int i = 0; i < 8; i++) CHECK(alx_region_of(a[i]) == prog);
        /* a new region reuses the freed chunks (non-ASan): must report the new one */
        AlxRegion *s2 = alx_region_cur(), *r2 = alx_region_enter();
        for (int i = 0; i < 8; i++) {
            char *b = alx_alloc(300000);
            CHECK(alx_region_of(b) == r2);
            for (int j = 0; j < 8; j++) if (a[j] == b) CHECK(alx_region_of(a[j]) == r2);
        }
        alx_region_exit(r2, s2);
    }
    /* many live regions at once: forces registry growth and deletion shifts */
    enum { N = 300 };
    static AlxRegion *rs[N], *ss[N]; static char *ps[N];
    for (int i = 0; i < N; i++) { ss[i] = alx_region_cur(); rs[i] = alx_region_enter(); ps[i] = alx_alloc(2000); }
    for (int i = 0; i < N; i++) CHECK(alx_region_of(ps[i]) == rs[i]);
    for (int i = N - 1; i >= 0; i--) {
        CHECK(alx_region_of(ps[i]) == rs[i]);
        alx_region_exit(rs[i], ss[i]);
        CHECK(alx_region_of(ps[i]) == prog);
        for (int j = 0; j < i; j += 7) CHECK(alx_region_of(ps[j]) == rs[j]);
    }
}

static void *of_thr(void *arg) {
    AlxRegion *prog = alx_region_program();
    char *foreign = arg;
    CHECK(alx_region_of(foreign) == prog);    /* another thread's memory */
    for (int i = 0; i < 200; i++) {
        AlxRegion *s = alx_region_cur(), *r = alx_region_enter();
        char *p = alx_alloc(5000), *big = alx_alloc(3u << 20);
        CHECK(alx_region_of(p + 4000) == r && alx_region_of(big + (2u << 20)) == r);
        CHECK(alx_region_of(foreign) == prog);
        alx_region_exit(r, s);
        CHECK(alx_region_of(p) == prog && alx_region_of(big) == prog);
    }
    return NULL;
}

static void of_threads(void) {
    AlxRegion *s = alx_region_cur(), *r = alx_region_enter();
    char *mine = alx_alloc(1000);
    pthread_t t[8];
    for (int i = 0; i < 8; i++) pthread_create(&t[i], NULL, of_thr, mine);
    for (int i = 0; i < 8; i++) pthread_join(t[i], NULL);
    CHECK(alx_region_of(mine) == r);
    alx_region_exit(r, s);
}

static void children(void) {
    AlxRegion *prog = alx_region_program();
    size_t h0 = alx_mem_held();
    /* freed with the parent */
    AlxRegion *s = alx_region_cur(), *p = alx_region_enter();
    AlxRegion *c1 = alx_region_new_child(p), *c2 = alx_region_new_child(p);
    AlxRegion *g = alx_region_new_child(c1);
    CHECK(alx_region_cur() == p && c1 != c2 && g != c1);
    char *pm = alx_alloc(100), *m1, *m2, *mg;
    AlxRegion *sv = alx_region_use(c1);
    CHECK(sv == p && alx_region_cur() == c1);
    m1 = alx_alloc(5000); fill(m1, 5000, 1);
    for (int i = 0; i < 10; i++) fill(alx_alloc(500000), 500000, 1);   /* many chunks */
    CHECK(alx_region_of(m1) == c1);
    alx_region_set(c2);
    m2 = alx_alloc(3u << 20); fill(m2, 3u << 20, 2);                  /* large */
    CHECK(alx_region_of(m2 + 1000) == c2 && alx_region_of(m1) == c1);
    alx_region_set(g);
    mg = alx_alloc(64);
    CHECK(alx_region_of(mg) == g);
    alx_region_set(p);
    CHECK(alx_region_of(pm) == p && all(m1, 5000, 1));
    CHECK(alx_mem_held() >= h0 + (4u << 20));
    alx_region_exit(p, s);
    CHECK(alx_region_cur() == prog);
    CHECK(alx_region_of(m1) == prog && alx_region_of(m2) == prog && alx_region_of(mg) == prog);
    CHECK(alx_mem_held() <= h0 + (4u << 20));       /* at most the free list */

    /* explicit free + unlink; the parent's exit later must not double free */
    s = alx_region_cur(); p = alx_region_enter();
    c1 = alx_region_new_child(p); c2 = alx_region_new_child(p);
    AlxRegion *c3 = alx_region_new_child(p);
    alx_region_use(c2); char *x = alx_alloc(10); alx_region_set(p);
    alx_region_use(c1); alx_alloc(10); alx_region_set(p);
    alx_region_use(c3); alx_alloc(3u << 20); alx_region_set(p);
    CHECK(alx_region_of(x) == c2);
    alx_region_free(c2);
    CHECK(alx_region_of(x) == prog);
    alx_region_free(c3);
    alx_region_free(c1);
    c1 = alx_region_new_child(p);           /* struct reuse after free */
    alx_region_free(c1);
    alx_region_exit(p, s);
    CHECK(alx_mem_held() <= h0 + (4u << 20));

    /* children of the program region live until freed */
    AlxRegion *pc = alx_region_new_child(prog);
    sv = alx_region_use(pc);
    char *y = alx_alloc(2u << 20); fill(y, 2u << 20, 9);
    alx_region_set(sv);
    s = alx_region_cur(); p = alx_region_enter(); alx_region_exit(p, s);
    CHECK(all(y, 2u << 20, 9) && alx_region_of(y) == pc);
    alx_region_free(pc);
    CHECK(alx_region_of(y) == prog);

    /* bytes: current and not current */
    s = alx_region_cur(); p = alx_region_enter();
    CHECK(alx_region_bytes(p) == 0);
    c1 = alx_region_new_child(p);
    CHECK(alx_region_bytes(c1) == 0);
    alx_alloc(100);
    CHECK(alx_region_bytes(p) == 112);
    alx_region_use(c1);
    CHECK(alx_region_bytes(p) == 112 && alx_region_bytes(c1) == 0);
    alx_alloc(16); alx_alloc(1);
    CHECK(alx_region_bytes(c1) == 32);
    int64_t tot = 32;
    for (int i = 0; i < 100; i++) { alx_alloc(40000); tot += 40000; }   /* 40000 is a multiple of 16 */
    int64_t b = alx_region_bytes(c1);
    CHECK(b == tot);
    alx_alloc(2u << 20);
    int64_t b2 = alx_region_bytes(c1);
    CHECK(b2 == b + (2 << 20));
    alx_region_set(p);
    CHECK(alx_region_bytes(c1) == b2 && alx_region_bytes(p) == 112);
    alx_alloc(16);
    CHECK(alx_region_bytes(p) == 128 && alx_region_bytes(c1) == b2);
    alx_region_exit(p, s);

    /* compaction: a child replaced by a fresh one after copying a small live set */
    s = alx_region_cur(); p = alx_region_enter();
    AlxRegion *box = alx_region_new_child(p);
    size_t hb = alx_mem_held();
    int64_t *live = NULL; int n = 0;
    for (int round = 0; round < 10000; round++) {
        sv = alx_region_use(box);
        for (int i = 0; i < 50; i++) fill(alx_alloc(2000), 2000, i);   /* garbage */
        alx_region_set(sv);
        if (alx_region_bytes(box) > (64 << 10)) {
            AlxRegion *nb = alx_region_new_child(p);
            sv = alx_region_use(nb);
            int64_t *nl = alx_alloc(8 * 9);
            for (int i = 0; i < n; i++) nl[i] = live[i];
            nl[n++] = round;
            if (n > 8) { memmove(nl, nl + 1, 8 * 8); n = 8; }
            alx_region_set(sv);
            alx_region_free(box);
            box = nb; live = nl;
            CHECK(alx_region_of(live) == box);
        }
        CHECK(alx_mem_held() <= hb + (8u << 20));
    }
    CHECK(n == 8 && live[7] > 9000);
    alx_region_exit(p, s);
    CHECK(alx_mem_held() <= h0 + (4u << 20));
}

int main(void) {
    alx_init();
    nested();
    bounded();
    large();
    children();
    of_basic(); of_large(); of_reuse(); of_threads();
    pthread_t t[16];
    for (int i = 0; i < 16; i++) pthread_create(&t[i], NULL, thr, NULL);
    for (int i = 0; i < 16; i++) pthread_join(t[i], NULL);
    puts("ok");
    return 0;
}
