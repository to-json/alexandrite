/* SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception */
/* Alexandrite C runtime. Everything emitted C may rely on.
 * Rules (DESIGN.md, "Rules for faithful C"): every runtime check is explicit,
 * arithmetic never relies on signed-overflow UB, panic = abort. */
#ifndef ALX_H
#define ALX_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>

typedef struct { const char *ptr; int64_t len; } AlxStr;
typedef struct { int64_t lo, hi; bool excl; } AlxRange;

/* Int in promote mode: small, or a pointer to a bignum. */
typedef struct AlxBig AlxBig;
typedef struct { int64_t v; AlxBig *big; } AlxPInt;

/* Generators: every generator state begins with this. */
typedef struct AlxGen { bool (*next)(struct AlxGen *self, void *out); } AlxGen;

/* ---------- memory: one program-lifetime region (v0) ----------
 * Nothing is freed, so allocation is a per-thread bump pointer into
 * malloc'd chunks; large requests go straight to malloc. */
extern _Thread_local char *alx_bump_cur, *alx_bump_end;
extern bool alx_counting;
extern size_t alx_allocs;
void *alx_alloc_slow(size_t bytes);
static inline void *alx_alloc(size_t bytes) {
    if (__builtin_expect(alx_counting, 0)) __atomic_fetch_add(&alx_allocs, 1, __ATOMIC_RELAXED);
    size_t n = ((bytes ? bytes : 1) + 15) & ~(size_t)15;
    char *p = alx_bump_cur;
    if (__builtin_expect((size_t)(alx_bump_end - p) >= n, 1)) {
        alx_bump_cur = p + n;
        return p;
    }
    return alx_alloc_slow(n);
}
void alx_init(void);

/* ---------- panics ---------- */
_Noreturn void alx_panic(const char *what, const char *loc);
_Noreturn void alx_overflow(const char *loc);

static inline int64_t alx_idx(int64_t i, int64_t n, const char *loc) {
    if ((uint64_t)i >= (uint64_t)n) alx_panic("index out of bounds", loc);
    return i;
}
/* Checked element read and element place. The harness counts these. */
#define ALX_IDX(a, i, loc) ({ __typeof__(a) a_ = (a); a_.ptr[alx_idx((i), a_.len, (loc))]; })
#define ALX_IDX_SET(a, i, loc) ((a).ptr[alx_idx((i), (a).len, (loc))])

/* ---------- Int arithmetic ---------- */
static inline int64_t alx_add(int64_t a, int64_t b, const char *loc) { int64_t r; if (__builtin_add_overflow(a, b, &r)) alx_overflow(loc); return r; }
static inline int64_t alx_sub(int64_t a, int64_t b, const char *loc) { int64_t r; if (__builtin_sub_overflow(a, b, &r)) alx_overflow(loc); return r; }
static inline int64_t alx_mul(int64_t a, int64_t b, const char *loc) { int64_t r; if (__builtin_mul_overflow(a, b, &r)) alx_overflow(loc); return r; }
static inline int64_t alx_div(int64_t a, int64_t b, const char *loc) {
    if (b == 0) alx_panic("division by zero", loc);
    if (a == INT64_MIN && b == -1) alx_overflow(loc);
    return a / b; /* truncates, as in Go */
}
static inline int64_t alx_rem(int64_t a, int64_t b, const char *loc) {
    if (b == 0) alx_panic("division by zero", loc);
    if (b == -1) return 0;
    return a % b; /* sign of the dividend, as in Go */
}
static inline int64_t alx_neg(int64_t a, const char *loc) { if (a == INT64_MIN) alx_overflow(loc); return -a; }
int64_t alx_pow(int64_t a, int64_t b, const char *loc);
bool alx_try_pow(int64_t a, int64_t b, int64_t *r);
/* Wrapping (#![overflow(wrap)]): through unsigned, no UB. */
static inline int64_t alx_wadd(int64_t a, int64_t b) { return (int64_t)((uint64_t)a + (uint64_t)b); }
static inline int64_t alx_wsub(int64_t a, int64_t b) { return (int64_t)((uint64_t)a - (uint64_t)b); }
static inline int64_t alx_wmul(int64_t a, int64_t b) { return (int64_t)((uint64_t)a * (uint64_t)b); }
static inline int64_t alx_sat_add(int64_t a, int64_t b) { int64_t r; return __builtin_add_overflow(a, b, &r) ? INT64_MAX : r; }
static inline bool alx_even(int64_t a) { return (a & 1) == 0; }
int64_t alx_isqrt(int64_t n, const char *loc);

/* ---------- arrays ---------- */
#define ALX_ARR(T, N)                                                                 \
    typedef struct { T *ptr; int64_t len; int64_t cap; } N;                           \
    static inline N N##_cap(int64_t c) {                                              \
        if (c < 0) c = 0;                                                             \
        N a = { c ? (T *)alx_alloc((size_t)c * sizeof(T)) : NULL, 0, c };             \
        return a;                                                                     \
    }                                                                                 \
    static inline void N##_push(N *a, T v) {                                          \
        if (a->len == a->cap) {                                                       \
            int64_t nc = a->cap ? 2 * a->cap : 4;                                     \
            T *np = (T *)alx_alloc((size_t)nc * sizeof(T));                           \
            if (a->len) memcpy(np, a->ptr, (size_t)a->len * sizeof(T));               \
            a->ptr = np;                                                              \
            a->cap = nc;                                                              \
        }                                                                             \
        a->ptr[a->len++] = v;                                                         \
    }                                                                                 \
    static inline N N##_new(int64_t n, T fill, const char *loc) {                     \
        if (n < 0) alx_panic("negative array size", loc);                             \
        N a = N##_cap(n);                                                             \
        if (sizeof(T) == 1) {                                                         \
            unsigned char b_;                                                         \
            memcpy(&b_, &fill, 1);                                                    \
            if (n) memset(a.ptr, b_, (size_t)n);                                      \
        } else {                                                                      \
            for (int64_t i = 0; i < n; i++) a.ptr[i] = fill;                          \
        }                                                                             \
        a.len = n;                                                                    \
        return a;                                                                     \
    }                                                                                 \
    static inline N N##_copy(N a) {                                                   \
        N b = N##_cap(a.len);                                                         \
        if (a.len) memcpy(b.ptr, a.ptr, (size_t)a.len * sizeof(T));                   \
        b.len = a.len;                                                                \
        return b;                                                                     \
    }                                                                                 \
    static inline N N##_lit(int64_t n, const T *items) {                              \
        N a = N##_cap(n);                                                             \
        if (n) memcpy(a.ptr, items, (size_t)n * sizeof(T));                           \
        a.len = n;                                                                    \
        return a;                                                                     \
    }

ALX_ARR(int64_t, Arr_I64)
ALX_ARR(AlxStr, Arr_Str)
ALX_ARR(AlxPInt, Arr_PInt)
ALX_ARR(bool, Arr_Bool)

/* ---------- strings ---------- */
static inline AlxStr alx_str_lit(const char *p, int64_t n) { AlxStr s = { p, n }; return s; }
AlxStr alx_str_rev(AlxStr s);
bool alx_str_eq(AlxStr a, AlxStr b);
static inline AlxStr alx_int_to_s(int64_t v) {
    char buf[24], *e = buf + sizeof buf, *q = e;
    uint64_t u = v < 0 ? -(uint64_t)v : (uint64_t)v;
    do { *--q = (char)('0' + u % 10); u /= 10; } while (u);
    if (v < 0) *--q = '-';
    int64_t n = e - q;
    char *p = (char *)alx_alloc((size_t)n);
    memcpy(p, q, (size_t)n);
    AlxStr s = { p, n };
    return s;
}
/* `v.to_s.size` */
static inline int64_t alx_int_ndigits(int64_t v) {
    uint64_t u = v < 0 ? -(uint64_t)v : (uint64_t)v;
    int64_t n = v < 0 ? 2 : 1;
    while (u >= 10) { u /= 10; n++; }
    return n;
}
/* `s == s.reverse` without building the reverse (reverse is by character). */
static inline bool alx_str_is_pal(AlxStr s) {
    for (int64_t i = 0, j = s.len - 1; i < j; i++, j--) {
        unsigned char a = (unsigned char)s.ptr[i], b = (unsigned char)s.ptr[j];
        if ((a | b) & 0x80) return alx_str_eq(s, alx_str_rev(s));
        if (a != b) return false;
    }
    if ((s.len & 1) && ((unsigned char)s.ptr[s.len / 2] & 0x80)) return alx_str_eq(s, alx_str_rev(s));
    return true;
}
int alx_str_cmp(AlxStr a, AlxStr b);
AlxStr alx_str_delete(AlxStr s, AlxStr chars);
Arr_Str alx_str_split(AlxStr s, AlxStr sep);
int64_t alx_str_to_i(AlxStr s);
/* Byte length of the UTF-8 character starting at byte i. */
int64_t alx_str_charlen(AlxStr s, int64_t i);
/* Bytes [i, i+n) as a string (n == 0: the single byte at i as an Int). */
AlxStr alx_str_sub(AlxStr s, int64_t i, int64_t n);
static inline int64_t alx_str_byte(AlxStr s, int64_t i) { return (unsigned char)s.ptr[i]; }
Arr_I64 alx_digits(int64_t v, const char *loc);
void alx_sort_i64(Arr_I64 *a);
void alx_sort_str(Arr_Str *a);
_Noreturn void alx_die_str(AlxStr msg);
int64_t alx_file_status(AlxStr path);
AlxStr alx_file_read_or_empty(AlxStr path);

/* ---------- output ---------- */
void alx_puts_i64(int64_t v);
void alx_puts_str(AlxStr s);
void alx_puts_bool(bool b);
void alx_puts_pint(AlxPInt v);

/* ---------- parallel map ---------- */
typedef void (*AlxWorker)(const void *in, void *out);
void alx_pmap(const void *in, int64_t n, size_t in_size, void *out, size_t out_size, AlxWorker fn);

/* ---------- bignums (promote mode) ---------- */
AlxPInt alx_p_from(int64_t v);
int64_t alx_p_to_i64(AlxPInt a, const char *loc);
AlxPInt alx_p_add(AlxPInt a, AlxPInt b);
AlxPInt alx_p_sub(AlxPInt a, AlxPInt b);
AlxPInt alx_p_mul(AlxPInt a, AlxPInt b);
AlxPInt alx_p_div(AlxPInt a, AlxPInt b, const char *loc);
AlxPInt alx_p_rem(AlxPInt a, AlxPInt b, const char *loc);
AlxPInt alx_p_pow(AlxPInt a, AlxPInt b, const char *loc);
int alx_p_cmp(AlxPInt a, AlxPInt b);
bool alx_p_even(AlxPInt a);
AlxStr alx_p_to_s(AlxPInt a);
int64_t alx_p_ndigits(AlxPInt a);

/* ---------- floats ---------- */
AlxStr alx_f_to_s(double x);
AlxStr alx_f_fmt(double x, int64_t digits);
int64_t alx_f_to_i(double x, const char *loc);
void alx_puts_f64(double x);
AlxStr alx_str_cat(int64_t n, const AlxStr *parts);

/* ---------- sized integers ---------- */
AlxStr alx_u64_to_s(int64_t bits);
AlxStr alx_int_fmt(int64_t v, int64_t base, bool upper, bool is_u64);
int64_t alx_f_to_u64(double x, const char *loc);
AlxStr alx_rune_to_s(int64_t r);
AlxStr alx_str_from_bytes(const uint8_t *p, int64_t n);
void alx_puts_u64(int64_t bits);
Arr_PInt alx_p_digits(AlxPInt a, const char *loc);

#endif
