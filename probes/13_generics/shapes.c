/* Probe 13b: four ways to emit trait-generic code in C.
 *   mono   one copy per concrete type (release)
 *   dict   one copy, method table passed in (debug; Swift witness tables)
 *   dyn    mixed collection of {table, object}; objects in per-type pools
 *   enum   mixed collection over a CLOSED set of types: tagged union + switch
 * Homogeneous forms must agree with each other, mixed forms likewise. */
#include "../10_c/alx_rt.h"
#include <time.h>

typedef struct { double r; } Circle;
typedef struct { double s; } Square;
typedef struct { double w, h; } Rect;

static double Circle_area(const Circle *c) { return 3.141592653589793 * c->r * c->r; }
static double Square_area(const Square *s) { return s->s * s->s; }
static double Rect_area(const Rect *r) { return r->w * r->h; }

/* ---- the method table for Shape: shared by dict (debug) and dyn ---- */
typedef struct { double (*area)(const void *); size_t size; } ShapeWT;
static double Circle_area_w(const void *p) { return Circle_area(p); }
static double Square_area_w(const void *p) { return Square_area(p); }
static double Rect_area_w(const void *p) { return Rect_area(p); }
static const ShapeWT Circle_Shape = { Circle_area_w, sizeof(Circle) };
static const ShapeWT Square_Shape = { Square_area_w, sizeof(Square) };
static const ShapeWT Rect_Shape = { Rect_area_w, sizeof(Rect) };

/* def total_area(xs) { xs.sum { it.area } }   bound inferred: T: Shape */

/* mono */
static double total_area_Circle(const Circle *xs, size_t n) {
    double acc = 0;
    for (size_t i = 0; i < n; i++) acc += Circle_area(&xs[i]);
    return acc;
}

/* dict: noinline + opaque table, as across a separate-compilation boundary */
__attribute__((noinline)) static double total_area_dict(const void *xs, size_t n, const ShapeWT *wt) {
    double acc = 0;
    const char *p = xs;
    for (size_t i = 0; i < n; i++) acc += wt->area(p + i * wt->size);
    return acc;
}

/* dyn: inferred for [circle, square, rect] when the set is open */
typedef struct { const ShapeWT *wt; const void *obj; } DynShape;
static double total_area_dyn(const DynShape *xs, size_t n) {
    double acc = 0;
    for (size_t i = 0; i < n; i++) acc += xs[i].wt->area(xs[i].obj);
    return acc;
}

/* enum: the same collection when every concrete type is known */
typedef struct {
    uint8_t tag;
    union { Circle c; Square s; Rect r; } u;
} AnyShape;
static double total_area_enum(const AnyShape *xs, size_t n) {
    double acc = 0;
    for (size_t i = 0; i < n; i++) {
        switch (xs[i].tag) {
        case 0: acc += Circle_area(&xs[i].u.c); break;
        case 1: acc += Square_area(&xs[i].u.s); break;
        case 2: acc += Rect_area(&xs[i].u.r); break;
        default: ALX_PANIC("bad shape tag");
        }
    }
    return acc;
}

enum { N = 10000000, REPS = 5 };

static double now_ms(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1e3 + t.tv_nsec / 1e6;
}

#define BEST(label, expr, out) do { double best = 1e18; \
    for (int k = 0; k < REPS; k++) { double t0 = now_ms(); out = (expr); \
        double dt = now_ms() - t0; if (dt < best) best = dt; } \
    printf("  %-26s %8.2f ms  total=%.6e\n", label, best, out); } while (0)

int main(void) {
    Circle *cs = alx_alloc(N * sizeof *cs);
    for (size_t i = 0; i < N; i++) cs[i].r = (double)(i % 100) / 10.0;

    /* Mixed: objects live in per-type pools; dyn holds {table, pointer},
     * enum holds the value inline. Same sequence of shapes in both. */
    Circle *pc = alx_alloc(N * sizeof *pc);
    Square *ps = alx_alloc(N * sizeof *ps);
    Rect *pr = alx_alloc(N * sizeof *pr);
    DynShape *dyn = alx_alloc(N * sizeof *dyn);
    AnyShape *en = alx_alloc(N * sizeof *en);
    size_t nc = 0, ns = 0, nr = 0;
    uint64_t rng = 12345;
    for (size_t i = 0; i < N; i++) {
        rng = rng * 6364136223846793005u + 1442695040888963407u; /* unpredictable mix */
        double v = (double)(i % 100) / 10.0;
        switch ((rng >> 33) % 3) {
        case 0: pc[nc] = (Circle){ v }; dyn[i] = (DynShape){ &Circle_Shape, &pc[nc++] };
                en[i] = (AnyShape){ .tag = 0, .u.c = { v } }; break;
        case 1: ps[ns] = (Square){ v }; dyn[i] = (DynShape){ &Square_Shape, &ps[ns++] };
                en[i] = (AnyShape){ .tag = 1, .u.s = { v } }; break;
        default: pr[nr] = (Rect){ v, v + 1 }; dyn[i] = (DynShape){ &Rect_Shape, &pr[nr++] };
                 en[i] = (AnyShape){ .tag = 2, .u.r = { v, v + 1 } }; break;
        }
    }

    double a, b, c, d;
    printf("homogeneous [Circle] (10M):\n");
    BEST("mono  (release)", total_area_Circle(cs, N), a);
    const ShapeWT *volatile opaque = &Circle_Shape; /* defeat devirtualization */
    BEST("dict  (debug)", total_area_dict(cs, N, opaque), b);
    printf("mixed [Circle|Square|Rect], random order (10M):\n");
    BEST("dyn   (open set)", total_area_dyn(dyn, N), c);
    BEST("enum  (closed set)", total_area_enum(en, N), d);
    printf("agree: homogeneous %s, mixed %s\n", a == b ? "yes" : "NO", c == d ? "yes" : "NO");
    printf("sizeof(DynShape)=%zu sizeof(AnyShape)=%zu\n", sizeof(DynShape), sizeof(AnyShape));
    return 0;
}
