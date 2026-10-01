/* Probe 10: hand-emitted C for the probe 06/09 workloads. bench.rs is the
 * same program as the Rust backend would emit it. Same checksums expected. */
#include "alx_rt.h"
#include <time.h>

ALX_VEC(int64_t, VecI64)
ALX_VEC(int32_t, VecI32)
ALX_VEC(uint8_t, VecU8)

enum { N = 10000000, DEPTH = 20, REPS = 5 };

static double now_ms(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1e3 + t.tv_nsec / 1e6;
}

/* #[pure] def sum(xs: [Int]) -> Int!   (fallible: overflow) */
static int sum(const int64_t *xs, size_t n, int64_t *out) {
    int64_t acc = 0;
    for (size_t i = 0; i < n; i++)
        if (!ALX_ADD(acc, xs[i], &acc)) return 0;
    *out = acc;
    return 1;
}

/* #[pure] def doubled(xs: [I32]) -> [Int], size out == size xs */
static void doubled(const int32_t *xs, size_t n, int64_t *out) {
    for (size_t i = 0; i < n; i++) out[i] = (int64_t)xs[i] * 2; /* [interval] */
}

/* xs.select { it % 3 == 0 }.map { it * 3 }, fused, size <= size xs */
static void select_map(const int64_t *xs, size_t n, VecI64 *out) {
    for (size_t i = 0; i < n; i++) {
        int64_t it = xs[i];
        if (it % 3 != 0) continue;
        out->ptr[out->len++] = it * 3; /* [structural] len <= n == cap */
    }
}

/* #[pure] def parse_digits(s: Str) -> [U8]!, size out <= size s */
static int parse_digits(const uint8_t *s, size_t n, uint8_t *out, uint8_t *bad) {
    for (size_t i = 0; i < n; i++) {
        uint8_t b = s[i];
        if (b < '0' || b > '9') { *bad = b; return 0; }
        out[i] = b - '0';
    }
    return 1;
}

/* Tree in a pool: 16 bytes, same as the Rust enum. */
typedef struct {
    uint32_t tag; /* 0 = Leaf, 1 = Node */
    union { int64_t leaf; struct { uint32_t l, r; } node; } u;
} Tree;
typedef struct { Tree *slots; uint32_t len, cap; } TreePool;

static uint32_t tp_put(TreePool *p, Tree t) {
    if (p->len == p->cap) ALX_PANIC("pool exceeded its header bound");
    p->slots[p->len] = t;
    return p->len++;
}

static uint32_t build(TreePool *p, int depth, int64_t *next) {
    if (depth == 0) return tp_put(p, (Tree){ .tag = 0, .u.leaf = ++*next });
    uint32_t l = build(p, depth - 1, next), r = build(p, depth - 1, next);
    return tp_put(p, (Tree){ .tag = 1, .u.node = { l, r } });
}

/* #[pure] def mirror(t: Tree) -> Tree, size out == size t */
static uint32_t mirror(const TreePool *src, uint32_t t, TreePool *dst) {
    Tree n = src->slots[ALX_IDX(t, src->len)];
    if (n.tag == 0) return tp_put(dst, n);
    uint32_t r2 = mirror(src, n.u.node.r, dst), l2 = mirror(src, n.u.node.l, dst);
    return tp_put(dst, (Tree){ .tag = 1, .u.node = { r2, l2 } });
}

/* Order-sensitive checksum of the leaves. */
static void leaves_hash(const TreePool *p, uint32_t t, uint64_t *h) {
    Tree n = p->slots[ALX_IDX(t, p->len)];
    if (n.tag == 0) { *h = *h * 31 + (uint64_t)n.u.leaf; return; }
    leaves_hash(p, n.u.node.l, h);
    leaves_hash(p, n.u.node.r, h);
}

/* Data-dependent gather: bounds checks the optimizer can't prove away. */
static void gather(const int64_t *xs, size_t n, int64_t *out) {
    for (size_t i = 0; i < n; i++) out[i] = xs[ALX_IDX((i * 7919) % n, n)];
}

static uint64_t hash_i64(const int64_t *xs, size_t n) {
    uint64_t h = 0;
    for (size_t i = 0; i < n; i++) h = h * 31 + (uint64_t)xs[i];
    return h;
}

#define TIME(label, body)                                                   \
    do {                                                                    \
        double best = 1e18;                                                 \
        for (int rep = 0; rep < REPS; rep++) {                              \
            double t0 = now_ms();                                           \
            body;                                                           \
            double dt = now_ms() - t0;                                      \
            if (dt < best) best = dt;                                       \
        }                                                                   \
        printf("%-12s %8.2f ms  ", label, best);                            \
    } while (0)

int main(int argc, char **argv) {
    /* Negative checks for the sanitizer run. */
    if (argc > 1 && !strcmp(argv[1], "oob")) {
        int64_t a[3] = { 1, 2, 3 };
        size_t i = (size_t)argc + 7; /* 9: not provable at compile time */
        printf("%lld\n", (long long)a[ALX_IDX(i, 3)]);
        return 0;
    }
    if (argc > 1 && !strcmp(argv[1], "overflow")) {
        int64_t big[2] = { INT64_MAX, (int64_t)argc };
        int64_t r = 0;
        printf("sum overflow -> %s\n", sum(big, 2, &r) ? "ok (WRONG)" : "Err(Overflow)");
        int32_t w = INT32_MAX;
        int64_t d = 0;
        doubled(&w, 1, &d);
        printf("doubled(INT32_MAX) -> %lld (widened, no overflow)\n", (long long)d);
        return 0;
    }

    VecI64 xs = VecI64_with_capacity(N);
    VecI32 x32 = VecI32_with_capacity(N);
    VecU8 digits = VecU8_with_capacity(N);
    for (size_t i = 0; i < N; i++) {
        xs.ptr[xs.len++] = (int64_t)(i * 7 % 1000);
        x32.ptr[x32.len++] = (int32_t)(i * 2654435761u) ;
        digits.ptr[digits.len++] = (uint8_t)('0' + i % 10);
    }

    int64_t s = 0;
    TIME("sum", sum(xs.ptr, xs.len, &s));
    printf("checksum %lld\n", (long long)s);

    VecI64 d = VecI64_with_capacity(N);
    d.len = N;
    TIME("doubled", doubled(x32.ptr, x32.len, d.ptr));
    printf("checksum %llu\n", (unsigned long long)hash_i64(d.ptr, d.len));

    VecI64 sm = VecI64_with_capacity(N);
    TIME("select_map", (sm.len = 0, select_map(xs.ptr, xs.len, &sm)));
    printf("checksum %llu\n", (unsigned long long)hash_i64(sm.ptr, sm.len));

    VecU8 pd = VecU8_with_capacity(N);
    uint8_t bad = 0;
    int ok = 0;
    TIME("parse_digits", ok = parse_digits(digits.ptr, digits.len, pd.ptr, &bad));
    pd.len = N;
    uint64_t ph = 0;
    for (size_t i = 0; i < pd.len; i++) ph = ph * 31 + pd.ptr[i];
    printf("checksum %llu ok=%d\n", (unsigned long long)ph, ok);

    uint32_t nodes = (1u << (DEPTH + 1)) - 1;
    TreePool src = { alx_alloc(nodes * sizeof(Tree)), 0, nodes };
    int64_t next = 0;
    uint32_t root = build(&src, DEPTH, &next);
    TreePool dst = { alx_alloc(nodes * sizeof(Tree)), 0, nodes };
    uint32_t m = 0;
    TIME("mirror", (dst.len = 0, m = mirror(&src, root, &dst)));
    uint64_t th = 0;
    leaves_hash(&dst, m, &th);
    printf("checksum %llu\n", (unsigned long long)th);

    VecI64 g = VecI64_with_capacity(N);
    g.len = N;
    TIME("gather", gather(xs.ptr, xs.len, g.ptr));
    printf("checksum %llu\n", (unsigned long long)hash_i64(g.ptr, g.len));

    return 0;
}
