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

int main(void) {
    alx_init();
    nested();
    bounded();
    large();
    pthread_t t[16];
    for (int i = 0; i < 16; i++) pthread_create(&t[i], NULL, thr, NULL);
    for (int i = 0; i < 16; i++) pthread_join(t[i], NULL);
    puts("ok");
    return 0;
}
