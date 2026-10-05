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
#include <errno.h>

typedef struct { const char *ptr; int64_t len; } AlxStr;
typedef struct { int64_t lo, hi; bool excl; } AlxRange;

/* Int in promote mode: small, or a pointer to a bignum. */
typedef struct AlxBig AlxBig;
typedef struct { int64_t v; AlxBig *big; } AlxPInt;

/* Generators: every generator state begins with this. */
typedef struct AlxGen { bool (*next)(struct AlxGen *self, void *out); } AlxGen;

/* ---------- memory: regions ----------
 * A region is a list of chunks plus a list of large blocks. Each thread has
 * a program region (never freed) and a current region; alx_alloc allocates
 * in the current region. The current region's bump cursor lives in the
 * thread-locals below (saved into the region struct when it is switched
 * out), so the fast path is a bump. Handles are thread-local. */
typedef struct AlxBlk { struct AlxBlk *next; size_t size; } AlxBlk;
typedef struct AlxRegion AlxRegion;
struct AlxRegion {
    char *cur, *end;       /* bump position; valid only while not current */
    AlxBlk *chunks, *larges;
    AlxRegion *pool_next;
    char *cbase;           /* payload start of the newest chunk (NULL: none) */
    size_t counted;        /* bytes used in older chunks + large blocks */
    AlxRegion *parent, *child, *sib, *sib_prev;   /* child regions (R3) */
};
/* The hot thread-locals, in one struct: on macOS every thread-local costs
 * an address lookup per function that uses it, so they share one. */
struct AlxTls {
    char *bump_cur, *bump_end;  /* the current region's bump cursor */
    AlxRegion *cur;             /* the current region; NULL = the program region */
    AlxRegion *pool;            /* spare region structs (some keep a chunk) */
    int ncached;                /* how many of them kept a chunk */
    const char *last_cat;       /* end of the last alx_str_cat result */
    uintptr_t last_g;           /* alx_region_of's last granule (0: none) */
    AlxRegion *last_r;          /* ... and its region */
    AlxRegion prog;             /* this thread's program region */
};
extern _Thread_local struct AlxTls alx_tls;
#define alx_bump_cur (alx_tls.bump_cur)
#define alx_bump_end (alx_tls.bump_end)
#define alx_tl_cur (alx_tls.cur)
#define alx_tl_pool (alx_tls.pool)
#define alx_tl_ncached (alx_tls.ncached)
#define alx_last_cat (alx_tls.last_cat)
#define alx_tl_prog (alx_tls.prog)
enum { ALX_CHUNK_BYTES = 1 << 20 };
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

static inline AlxRegion *alx_region_program(void) { return &alx_tl_prog; }
/* the current region */
static inline AlxRegion *alx_region_cur(void) { return alx_tl_cur ? alx_tl_cur : &alx_tl_prog; }
AlxRegion *alx_region_enter_slow(void);
void alx_region_exit_slow(AlxRegion *r, AlxRegion *saved);
/* A mark in the current region, and rolling it back (freeing what was
 * allocated in it since): the frame of a call that allocates only for itself.
 * The mark is the bump position (or a tagged region of its own, in the
 * program region) and the newest large block. */
AlxRegion *alx_region_mark_slow(void);
void alx_region_reset_slow(AlxRegion *mark, void *larges);
/* A fresh empty child region of parent (not made current). Freed with its
 * parent (alx_region_exit / alx_region_free of it, recursively), or earlier by
 * alx_region_free. The program region may be a parent. */
AlxRegion *alx_region_new_child(AlxRegion *parent);
/* Free r now: its children, chunks and large blocks; unlink it from its
 * parent. r must not be current (nor the program region). */
void alx_region_free(AlxRegion *r);
/* Bytes allocated in r so far (chunk bytes in use + large blocks); O(1). */
/* Bytes allocated in r (what compaction compares against). */
static inline int64_t alx_region_bytes(AlxRegion *r) {
    if (!r->cbase) return (int64_t)r->counted;
    char *cur = r == alx_region_cur() ? alx_bump_cur : r->cur;
    return (int64_t)(r->counted + (size_t)(cur - r->cbase));
}
/* make r current */
static inline void alx_region_set(AlxRegion *r) {
    AlxRegion *c = alx_region_cur();
    if (c == r) return;
    c->cur = alx_bump_cur; c->end = alx_bump_end;
    alx_bump_cur = r->cur; alx_bump_end = r->end;
    alx_tl_cur = r;
}
/* make r current; returns previous */
static inline AlxRegion *alx_region_use(AlxRegion *r) {
    AlxRegion *p = alx_region_cur();
    alx_region_set(r);
    return p;
}
/* Region structs freed with their one chunk wait here for the next enter. */
enum { ALX_CACHED_CAP = 8 };
/* A fresh empty region, made current. The common case (a pooled struct that
 * kept its chunk) is inline. */
static inline AlxRegion *alx_region_enter(void) {
    AlxRegion *r = alx_tl_pool;
    if (__builtin_expect(r && r->chunks, 1)) {
        alx_tl_pool = r->pool_next;
        alx_tl_ncached--;
        r->cbase = (char *)r->chunks + 16;
        r->cur = r->cbase;
        r->end = (char *)r->chunks + ALX_CHUNK_BYTES;
        r->larges = NULL; r->pool_next = NULL; r->counted = 0;
        r->parent = r->child = r->sib = r->sib_prev = NULL;
        alx_region_set(r);
        return r;
    }
    return alx_region_enter_slow();
}
/* Free r (everything in it); make saved current. Inline when r is current
 * and holds one chunk and nothing else: the struct keeps it, pooled. */
static inline void alx_region_exit(AlxRegion *r, AlxRegion *saved) {
    if (__builtin_expect(alx_region_cur() == r && r != saved && !r->child && !r->parent && !r->larges && r->chunks && !r->chunks->next && alx_tl_ncached < ALX_CACHED_CAP, 1)) {
        alx_bump_cur = saved->cur; alx_bump_end = saved->end;
        alx_tl_cur = saved;
        alx_tl_ncached++;
        r->pool_next = alx_tl_pool; alx_tl_pool = r;
        return;
    }
    alx_region_exit_slow(r, saved);
}
/* The region of this thread whose chunk or large block contains p (interior
 * pointers included); else (NULL, literals, stack, malloc, other threads) the
 * program region. O(1) expected. */
AlxRegion *alx_region_of_slow(const void *p);
/* The region holding p (the program region if none): the last answer is
 * kept for its 1 MB granule. */
static inline AlxRegion *alx_region_of(const void *p) {
    uintptr_t g = (uintptr_t)p >> 20;
    if (g && g == alx_tls.last_g) return alx_tls.last_r;  /* (granule 0 is never a region's) */
    return alx_region_of_slow(p);
}
/* Stats (all threads): bytes held in chunks/large blocks, live regions + free lists. */
size_t alx_mem_held(void);
size_t alx_mem_peak(void);
bool alx_count_allocs(bool on);
int64_t alx_alloc_count(void);

/* ---------- panics ---------- */
_Noreturn void alx_panic(const char *what, const char *loc);
/* A panic with a computed message (no "alexandrite: " prefix). */
_Noreturn void alx_panic_str(AlxStr msg);
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
static inline bool alx_mul_ovf(int64_t a, int64_t b) { int64_t r; return __builtin_mul_overflow(a, b, &r); }
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
        if (a->len >= a->cap) { /* a slice's cap is 0: it never writes past it */      \
            int64_t nc = a->len ? 2 * a->len : 4;                                     \
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
bool alx_str_eq_slow(AlxStr a, AlxStr b);
/* Short strings (map keys, words) compare inline. */
static inline bool alx_str_eq(AlxStr a, AlxStr b) {
    if (a.len != b.len) return false;
    if (a.ptr == b.ptr || a.len == 0) return true;
    if (a.len <= 16) {
        for (int64_t i = 0; i < a.len; i++)
            if (a.ptr[i] != b.ptr[i]) return false;
        return true;
    }
    return alx_str_eq_slow(a, b);
}
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
AlxStr alx_str_join(Arr_Str a, AlxStr sep);
int64_t alx_str_to_i(AlxStr s);
int64_t alx_str_index(AlxStr s, AlxStr sub, int64_t from);
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
/* Test support: a monotonic clock, and process-wide capture of stdout. */
int64_t alx_now_ns(void);
int64_t alx_cap_begin(void);
AlxStr alx_cap_end(void);

/* ---------- output ---------- */
void alx_puts_i64(int64_t v);
void alx_puts_str(AlxStr s);
void alx_puts_bool(bool b);
void alx_puts_pint(AlxPInt v);

/* ---------- parallel map ---------- */
typedef void (*AlxWorker)(const void *in, void *out);
void alx_pmap(const void *in, int64_t n, size_t in_size, void *out, size_t out_size, AlxWorker fn);
/* fn writes a Result (a bool at offset 0, the value at val_off, res_size in
 * all): out gets the values; err the Result of the lowest failing index,
 * or a zeroed one with the bool true when nothing fails. */
void alx_pmap_try(const void *in, int64_t n, size_t in_size, void *out, size_t val_size, AlxWorker fn,
                  size_t res_size, size_t val_off, void *err);

/* ---------- tasks and channels ---------- */
typedef struct AlxTask AlxTask;
typedef struct AlxChan AlxChan;
typedef struct AlxLock AlxLock;
AlxLock *alx_lock_new(void);
/* Take the lock; a poisoned one (its holder panicked) panics at loc. */
void alx_lock(AlxLock *l, const char *loc);
void alx_unlock(AlxLock *l);
bool alx_lock_poisoned(AlxLock *l);
void alx_lock_clear_poison(AlxLock *l);
int64_t *alx_atomic_new(int64_t v);
/* A case of alx_select. `buf` holds the value to send / receives the value.
 * `ok` is written only on the chosen recv case (the caller presets it). */
typedef struct { AlxChan *ch; void *buf; int64_t is_send; int64_t ok; } AlxSelCase;
/* Run `fn(copy of env, result)` as a task: a coroutine on one of N worker
 * threads (ALX_PROCS, default the CPU count), 8 MiB of stack address space,
 * committed as touched (ALX_TASK_STACK, e.g. `512k`, `64m`, `1g`).
 * Blocking channel/wait calls park it. A panic in the task is caught: see
 * alx_task_wait. */
AlxTask *alx_spawn(AlxWorker fn, const void *env, size_t in_size, size_t out_size);
/* Block until the task is done. True: result copied to `out`. False: the task
 * panicked; `*msg` is its message (`out` untouched). Repeatable. */
bool alx_task_wait(AlxTask *t, void *out, AlxStr *msg);
AlxChan *alx_chan_new(int64_t cap, size_t esz);
int64_t alx_chan_len(AlxChan *c);
void alx_chan_send(AlxChan *c, const void *val, const char *loc);
/* False if the channel is closed and drained (`out` untouched). */
bool alx_chan_recv(AlxChan *c, void *out);
void alx_chan_close(AlxChan *c, const char *loc);
/* Index of the case run, or n if none was ready and `has_default`. */
int64_t alx_select(AlxSelCase *cases, int64_t n, bool has_default, const char *loc);

/* ---------- bignums (promote mode) ---------- */
AlxPInt alx_p_from(int64_t v);
AlxPInt alx_p_from_str(AlxStr s);
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
AlxStr alx_f_fmt_e(double x, int64_t digits, bool upper);
AlxStr alx_str_pad(AlxStr s, int64_t width, int64_t flags);
AlxStr alx_str_quote(AlxStr s);
void alx_print_str(AlxStr s);
/* Sleep: a task parks (its worker runs others); a plain thread sleeps. */
void alx_sleep_ns(int64_t ns);
/* Let the other tasks queued on this worker run first. */
void alx_task_yield(void);
void alx_set_args(int argc, char **argv);
int64_t alx_argc(void);
const char *alx_argv(int64_t i);
int64_t alx_wall_ns(void);
int64_t alx_mono_ns(void);
int64_t alx_local_offset(int64_t unix_sec);
const char *alx_local_zone(int64_t unix_sec);
int64_t alx_f_to_i(double x, const char *loc);
static inline int64_t alx_f_bits(double x) { int64_t b; memcpy(&b, &x, 8); return b; }
static inline double alx_f_from_bits(int64_t b) { double x; memcpy(&x, &b, 8); return x; }
void alx_puts_f64(double x);
AlxStr alx_str_cat(int64_t n, const AlxStr *parts);
/* a + b; appends in place when a is the last concatenation and ends at the
 * bump pointer (see alx_str_cat). */
static inline AlxStr alx_str_cat2(AlxStr a, AlxStr b) {
    /* Strings are immutable: "" + b is b. (A copy is a one-part alx_str_cat.) */
    if (a.len == 0) return b;
    if (b.len == 0) return a;
    if (a.len > 0 && a.ptr + a.len == alx_last_cat && b.len <= 64) {
        char *f = (char *)a.ptr, *top = alx_bump_cur;
        size_t len = (size_t)(a.len + b.len);
        size_t had = ((size_t)a.len + 15) & ~(size_t)15, need = (len + 15) & ~(size_t)15;
        if (f + had == top && (size_t)(alx_bump_end - f) >= need) {
            alx_bump_cur = f + need;
            for (int64_t k = 0; k < b.len; k++) f[a.len + k] = b.ptr[k];
            alx_last_cat = f + len;
            AlxStr s = { f, (int64_t)len };
            return s;
        }
    }
    AlxStr parts[2] = { a, b };
    return alx_str_cat(2, parts);
}

/* ---------- sized integers ---------- */
AlxStr alx_u64_to_s(int64_t bits);
AlxStr alx_int_fmt(int64_t v, int64_t base, bool upper, bool is_u64);
int64_t alx_f_to_u64(double x, const char *loc);
AlxStr alx_rune_to_s(int64_t r);
AlxStr alx_str_from_bytes(const uint8_t *p, int64_t n);
void alx_puts_u64(int64_t bits);
Arr_PInt alx_p_digits(AlxPInt a, const char *loc);

/* ---------- C foreign functions (`extern def`) ----------
 * The generated C declares each extern under a private name bound to the real
 * link name, so it never clashes with a system header:
 *   extern int64_t alx_ffi_0(int32_t, uint8_t *) __asm__(ALX_SYM("write"));
 * ALX_SYM adds the platform's leading underscore (macOS) via __USER_LABEL_PREFIX__. */
#define ALX_STR2_(x) #x
#define ALX_STR_(x) ALX_STR2_(x)
#define ALX_SYM(s) ALX_STR_(__USER_LABEL_PREFIX__) s
/* errno as it was right after the last extern call on this thread. */
extern _Thread_local int alx_ffi_errno_;
static inline void alx_ffi_save_errno(void) { alx_ffi_errno_ = errno; }
/* Before each extern call: flush our stdout buffer, so what `puts` printed
 * comes out before what the C function writes; and zero errno. */
void alx_ffi_enter(void);
/* A malloc'd NUL-terminated copy of s (free it with free()). */
char *alx_cstr_new(AlxStr s);
AlxStr alx_strerror(int64_t n);
/* Copies; a NULL pointer gives the empty string. */
AlxStr alx_str_from_cstr(const char *p);
AlxStr alx_str_from_ptr(const char *p, int64_t n);
/* Non-variadic wrappers for variadic libc functions (std calls these as
 * `extern def`s): open(2) and fcntl(2) pass their last argument differently
 * on arm64 macOS, so alexandrite never calls a variadic function directly. */
int32_t alx_sys_open(const char *path, int32_t flags, int32_t mode);
int32_t alx_sys_fcntl(int32_t fd, int32_t cmd, int64_t arg);
/* Platform constants by name (O_CREAT, SEEK_END, ENOENT, S_IFDIR, CLOCK_MONOTONIC, ...);
 * -1 if the name is unknown here. */
int64_t alx_sys_const(const char *name);
int64_t alx_sys_const_count(void);
const char *alx_sys_const_name(int64_t i);

/* ---------- files and directories (std os) ---------- */
/* stat(2)/lstat(2)/fstat(2) as ALX_STAT_FIELDS int64s (native endian) in out:
 * mode, size, mtime_ns, atime_ns, ino, nlink. 0 or -errno. */
#define ALX_STAT_FIELDS 6
int32_t alx_sys_stat(const char *path, uint8_t *out, int32_t follow);
int32_t alx_sys_fstat(int32_t fd, uint8_t *out);
/* A directory handle or -errno; entries one at a time (NULL at the end). */
int64_t alx_sys_dir_open(const char *path);
const char *alx_sys_dir_next(int64_t h, uint8_t *kind);
void alx_sys_dir_close(int64_t h);
/* The i-th "NAME=value" of the environment, NULL past the end. */
const char *alx_environ(int64_t i);

/* ---------- processes (std os/exec) ----------
 * spawn: argv / env are argc / envc NUL-terminated strings back to back
 * (envc < 0: inherit this process's environment); dir "" = this directory;
 * fd0..fd2 become the child's stdin/stdout/stderr. PATH is searched for
 * argv[0]. The pid, or -errno. */
int64_t alx_sys_spawn(const uint8_t *argv, int64_t argc, const uint8_t *env, int64_t envc, const char *dir,
                      int64_t fd0, int64_t fd1, int64_t fd2);
/* Waits for pid. out (5 int64s): 0 exited / 1 killed, its status / signal,
 * user and system CPU ns, peak resident bytes. 0 or -errno. */
int64_t alx_sys_wait(int64_t pid, uint8_t *out);
/* A pipe, both ends close-on-exec (the read end non-blocking if nonblock):
 * out = 2 int64s (read, write). 0 or -errno. */
int64_t alx_sys_pipe(uint8_t *out, int64_t nonblock);
/* Replaces this process (execvp; dir "" = stay): returns only on failure,
 * with -errno. Arguments as for alx_sys_spawn. */
int64_t alx_sys_exec(const uint8_t *argv, int64_t argc, const uint8_t *env, int64_t envc, const char *dir,
                     int64_t fd0, int64_t fd1, int64_t fd2);
/* Blocks until a or b (either may be -1) is readable or hung up. 0 or -errno. */
int64_t alx_sys_poll2(int64_t a, int64_t b);
int64_t alx_sig_watch(int64_t mask);
int64_t alx_sig_unwatch(int64_t rfd);
int64_t alx_sig_reset(int64_t mask, int64_t how);
int64_t alx_sig_ignored(int64_t sig);
int64_t alx_user_lookup(int64_t kind, const char *key, uint8_t *out, int64_t n);
int64_t alx_user_groups(const char *name, int64_t gid, uint8_t *out, int64_t n);

/* ---------- the I/O event loop and sockets (std net, L3) ---------- */
/* Wait until fd is readable (mode 1) or writable (mode 2) or in error: parks a
 * task (poller thread), poll(2)s otherwise. 0, or -errno if fd can't be polled. */
int64_t alx_fd_wait(int64_t fd, int64_t mode);
/* close(2) that wakes tasks waiting on fd first. 0 or -errno. */
int64_t alx_fd_close(int64_t fd);
/* Sockets: non-blocking, close-on-exec fds; negative results are -errno
 * (-100000: name not found). Address buffers must hold 64 bytes. */
int64_t alx_sock_listen(const char *host, int64_t port, int64_t backlog);
int64_t alx_sock_accept(int64_t fd, uint8_t *out_addr);
int64_t alx_sock_connect(const char *host, int64_t port);
int64_t alx_sock_error(int64_t fd);
int64_t alx_sock_local_addr(int64_t fd, uint8_t *out);
int64_t alx_sock_peer_addr(int64_t fd, uint8_t *out);
int64_t alx_sock_set_nodelay(int64_t fd, int64_t on);
int64_t alx_sock_shutdown(int64_t fd, int64_t how);
int64_t alx_sock_lookup(const char *host, uint8_t *out, int64_t n);
int64_t alx_sock_open(int64_t kind, const char *host, int64_t port, int64_t listen_, int64_t backlog);
int64_t alx_sock_recvfrom(int64_t fd, uint8_t *buf, int64_t n, uint8_t *out);



/* Light frames (see alx_region_mark_slow). */
static inline AlxRegion *alx_region_mark(void) {
    if (!alx_tl_cur) return alx_region_mark_slow();
    return (AlxRegion *)alx_bump_cur;
}
static inline void *alx_region_mark_larges(void) {
    return alx_tl_cur ? (void *)alx_tl_cur->larges : NULL;
}
static inline void alx_region_reset(AlxRegion *mark, void *larges) {
    char *m = (char *)mark, *b = alx_bump_cur;
    AlxRegion *c = alx_tl_cur;
    /* Still in the chunk it was made in, no large block since: just the
     * bump position back. */
    if (c && m && b && !((uintptr_t)m & 1) && (void *)c->larges == larges) {
        char *cb = (char *)((uintptr_t)(b - 1) & ~(uintptr_t)(ALX_CHUNK_BYTES - 1));
        if (m >= cb && m <= b) {
            alx_bump_cur = m;
            return;
        }
    }
    alx_region_reset_slow(mark, larges);
}

#endif
