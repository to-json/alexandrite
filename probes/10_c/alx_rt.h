/* Alexandrite C runtime, probe 10. Everything the C backend's output may
 * rely on. Rules (DESIGN.md, "Rules for faithful C"):
 *   - every runtime check is explicit (ALX_IDX, ALX_ADD, ...)
 *   - arithmetic never relies on signed-overflow UB (checked builtins)
 *   - panic = abort, with a message
 *   - no type punning except memcpy
 */
#ifndef ALX_RT_H
#define ALX_RT_H

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef int64_t Int;

_Noreturn static inline void alx_panic(const char *what, const char *file, int line) {
    fprintf(stderr, "alexandrite panic: %s at %s:%d\n", what, file, line);
    abort();
}
#define ALX_PANIC(what) alx_panic((what), __FILE__, __LINE__)

/* Bounds-checked index: evaluates to i, or aborts. */
#define ALX_IDX(i, len) \
    ((size_t)(i) < (size_t)(len) ? (size_t)(i) : (ALX_PANIC("index out of bounds"), (size_t)0))

/* Checked arithmetic for fallible (`T!`) code: returns 0 on overflow. */
#define ALX_ADD(a, b, out) (!__builtin_add_overflow((a), (b), (out)))
#define ALX_MUL(a, b, out) (!__builtin_mul_overflow((a), (b), (out)))

/* ---------- allocation, counted (mirrors rt's Counting allocator) ---------- */

static size_t alx_allocs, alx_live, alx_peak;

static inline void *alx_alloc(size_t bytes) {
    void *p = malloc(bytes ? bytes : 1);
    if (!p) ALX_PANIC("out of memory");
    alx_allocs++;
    alx_live += bytes;
    if (alx_live > alx_peak) alx_peak = alx_live;
    return p;
}

static inline void alx_free(void *p, size_t bytes) {
    alx_live -= bytes;
    free(p);
}

/* ---------- typed vectors: Vec<T> with explicit capacity ---------- */

#define ALX_VEC(T, Name)                                                        \
    typedef struct { T *ptr; size_t len, cap; } Name;                           \
    static inline Name Name##_with_capacity(size_t cap) {                       \
        return (Name){ alx_alloc(cap * sizeof(T)), 0, cap };                    \
    }                                                                           \
    static inline void Name##_push(Name *v, T x) {                              \
        if (v->len == v->cap) {                                                 \
            size_t ncap = v->cap ? 2 * v->cap : 4;                              \
            T *np = alx_alloc(ncap * sizeof(T));                                \
            memcpy(np, v->ptr, v->len * sizeof(T));                             \
            alx_free(v->ptr, v->cap * sizeof(T));                               \
            v->ptr = np; v->cap = ncap; alx_spills++;                           \
        }                                                                       \
        v->ptr[v->len++] = x;                                                   \
    }                                                                           \
    static inline void Name##_drop(Name *v) {                                   \
        alx_free(v->ptr, v->cap * sizeof(T));                                   \
        v->ptr = NULL; v->len = v->cap = 0;                                     \
    }

static size_t alx_spills;

#endif
