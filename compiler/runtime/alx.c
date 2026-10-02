/* SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception */
#include "alx.h"

#include <assert.h>
#include <errno.h>
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <sys/mman.h>
#include <unistd.h>

/* ---------- memory ---------- */

size_t alx_allocs;
bool alx_counting;
_Thread_local char *alx_bump_cur, *alx_bump_end;
static size_t mem_held, mem_peak;
static bool alx_memstats;

size_t alx_mem_held(void) { return __atomic_load_n(&mem_held, __ATOMIC_RELAXED); }
size_t alx_mem_peak(void) { return __atomic_load_n(&mem_peak, __ATOMIC_RELAXED); }

static void mem_add(size_t n) {
    size_t h = __atomic_add_fetch(&mem_held, n, __ATOMIC_RELAXED);
    size_t p = __atomic_load_n(&mem_peak, __ATOMIC_RELAXED);
    while (h > p && !__atomic_compare_exchange_n(&mem_peak, &p, h, true, __ATOMIC_RELAXED, __ATOMIC_RELAXED)) {}
}
static void mem_sub(size_t n) { __atomic_sub_fetch(&mem_held, n, __ATOMIC_RELAXED); }

static void alx_report(void) {
    if (alx_counting) fprintf(stderr, "alx-allocs: %zu\n", alx_allocs);
    if (alx_memstats) fprintf(stderr, "alx-mem: peak=%zu final=%zu\n", alx_mem_peak(), alx_mem_held());
}

void alx_init(void) {
    alx_counting = getenv("ALX_COUNT_ALLOCS") != NULL;
    alx_memstats = getenv("ALX_MEMSTATS") != NULL;
    atexit(alx_report);
}

enum { ALX_CHUNK = 1 << 20, ALX_HDR = 16 };

#if defined(__has_feature)
#  if __has_feature(address_sanitizer)
#    define ALX_ASAN 1
#  endif
#endif
#if defined(__SANITIZE_ADDRESS__)
#  define ALX_ASAN 1
#endif
/* Under ASan freed chunks are free()d (not reused) so use-after-free is caught. */
#ifdef ALX_ASAN
enum { ALX_FREE_CAP = 0 };
#else
enum { ALX_FREE_CAP = 4 };  /* chunks kept per thread (4 MB) */
#endif

/* Chunks and large blocks start with a 16-byte header. */
typedef struct Blk { struct Blk *next; size_t size; } Blk;

struct AlxRegion {
    char *cur, *end;       /* bump position; valid only while not current */
    Blk *chunks, *larges;
    AlxRegion *pool_next;
    char *cbase;           /* payload start of the newest chunk (NULL: none) */
    size_t counted;        /* bytes used in older chunks + large blocks */
    AlxRegion *parent, *child, *sib, *sib_prev;   /* child regions (R3) */
};

static _Thread_local AlxRegion tl_prog;
static _Thread_local AlxRegion *tl_cur;        /* NULL = program region */
static _Thread_local AlxRegion *tl_pool;       /* spare region structs */
static _Thread_local Blk *tl_free;             /* free chunks */
static _Thread_local int tl_nfree;

/* Granule registry: this thread's map from 1 MB granule number to the region
 * owning the chunk / large block that covers it (open addressing, linear
 * probing, backward-shift deletion; granule 0 is never mapped so g == 0 marks
 * an empty slot). Chunks and large blocks are 1 MB-aligned and large blocks
 * are padded to whole granules, so a granule belongs to at most one block. */
typedef struct { uintptr_t g; AlxRegion *r; } RegEnt;
static _Thread_local RegEnt *tl_reg;
static _Thread_local size_t tl_reg_cap, tl_reg_n;   /* cap is a power of two */

static pthread_key_t free_key;
static pthread_once_t free_once = PTHREAD_ONCE_INIT;
static _Thread_local bool tl_free_armed;
/* At thread exit, return the thread's free chunks to the OS. */
static void free_chunks(void *unused) {
    (void)unused;
    free(tl_reg); tl_reg = NULL; tl_reg_cap = tl_reg_n = 0;
    for (Blk *b = tl_free, *n; b; b = n) { n = b->next; mem_sub(b->size); free(b); }
    tl_free = NULL; tl_nfree = 0;
}
static void free_key_init(void) { pthread_key_create(&free_key, free_chunks); }
static void arm_free(void) {
    pthread_once(&free_once, free_key_init);
    pthread_setspecific(free_key, &tl_free_armed);
    tl_free_armed = true;
}

static inline size_t reg_hash(uintptr_t g, size_t mask) {
    return (size_t)((g * 0x9E3779B97F4A7C15ull) >> 32) & mask;
}
static void reg_put(uintptr_t g, AlxRegion *r) {
    if (!tl_reg_cap || (tl_reg_n + 1) * 2 > tl_reg_cap) {
        size_t nc = tl_reg_cap ? tl_reg_cap * 2 : 64;
        RegEnt *t = calloc(nc, sizeof *t);
        if (!t) alx_panic("out of memory", "runtime");
        for (size_t i = 0; i < tl_reg_cap; i++)
            if (tl_reg[i].g) {
                size_t j = reg_hash(tl_reg[i].g, nc - 1);
                while (t[j].g) j = (j + 1) & (nc - 1);
                t[j] = tl_reg[i];
            }
        free(tl_reg); tl_reg = t; tl_reg_cap = nc;
        if (!tl_free_armed) arm_free();
    }
    size_t m = tl_reg_cap - 1, i = reg_hash(g, m);
    while (tl_reg[i].g && tl_reg[i].g != g) i = (i + 1) & m;
    if (!tl_reg[i].g) tl_reg_n++;
    tl_reg[i].g = g; tl_reg[i].r = r;
}
static void reg_del(uintptr_t g) {
    if (!tl_reg_cap) return;
    size_t m = tl_reg_cap - 1, i = reg_hash(g, m);
    while (tl_reg[i].g && tl_reg[i].g != g) i = (i + 1) & m;
    if (!tl_reg[i].g) return;
    tl_reg_n--;
    for (size_t j = i;;) {
        j = (j + 1) & m;
        if (!tl_reg[j].g) break;
        size_t h = reg_hash(tl_reg[j].g, m);
        /* entry j stays put if its home h lies cyclically in (i, j] */
        if (i <= j ? (i < h && h <= j) : (i < h || h <= j)) continue;
        tl_reg[i] = tl_reg[j]; i = j;
    }
    tl_reg[i].g = 0;
}
static void reg_blk(const void *b, size_t size, AlxRegion *r) {
    uintptr_t g0 = (uintptr_t)b >> 20, g1 = ((uintptr_t)b + size - 1) >> 20;
    for (uintptr_t g = g0; g <= g1; g++) { if (r) reg_put(g, r); else reg_del(g); }
}

AlxRegion *alx_region_of(const void *p) {
    if (p && tl_reg_n) {
        uintptr_t g = (uintptr_t)p >> 20;
        size_t m = tl_reg_cap - 1, i = reg_hash(g, m);
        for (RegEnt *e; (e = &tl_reg[i])->g; i = (i + 1) & m)
            if (e->g == g) return e->r;
    }
    return &tl_prog;
}

static inline AlxRegion *cur_region(void) { return tl_cur ? tl_cur : &tl_prog; }

AlxRegion *alx_region_program(void) { return &tl_prog; }
AlxRegion *alx_region_cur(void) { return cur_region(); }

void alx_region_set(AlxRegion *r) {
    AlxRegion *c = cur_region();
    if (c == r) return;
    c->cur = alx_bump_cur; c->end = alx_bump_end;
    alx_bump_cur = r->cur; alx_bump_end = r->end;
    tl_cur = r;
}

AlxRegion *alx_region_use(AlxRegion *r) {
    AlxRegion *p = cur_region();
    alx_region_set(r);
    return p;
}

static AlxRegion *region_alloc(void) {
    AlxRegion *r = tl_pool;
    if (r) tl_pool = r->pool_next;
    else {
        r = malloc(sizeof *r);
        if (!r) alx_panic("out of memory", "runtime");
    }
    r->cur = r->end = NULL; r->chunks = r->larges = NULL; r->pool_next = NULL;
    r->cbase = NULL; r->counted = 0;
    r->parent = r->child = r->sib = r->sib_prev = NULL;
    return r;
}

AlxRegion *alx_region_enter(void) {
    AlxRegion *r = region_alloc();
    alx_region_set(r);
    return r;
}

AlxRegion *alx_region_new_child(AlxRegion *parent) {
    AlxRegion *r = region_alloc();
    r->parent = parent;
    r->sib = parent->child;
    if (r->sib) r->sib->sib_prev = r;
    parent->child = r;
    return r;
}

/* Free r's children (recursively), then its chunks and large blocks, unlink
 * it, and recycle the struct. Does not touch the current region. */
static void region_release(AlxRegion *r) {
    while (r->child) region_release(r->child);
    if (r->parent) {
        if (r->sib_prev) r->sib_prev->sib = r->sib; else r->parent->child = r->sib;
        if (r->sib) r->sib->sib_prev = r->sib_prev;
        r->parent = NULL;
    }
    for (Blk *b = r->chunks, *n; b; b = n) {
        n = b->next;
        reg_blk(b, ALX_CHUNK, NULL);
        if (tl_nfree < ALX_FREE_CAP) { if (!tl_free_armed) arm_free(); b->next = tl_free; tl_free = b; tl_nfree++; }
        else { mem_sub(b->size); free(b); }
    }
    for (Blk *b = r->larges, *n; b; b = n) { n = b->next; reg_blk(b, b->size, NULL); mem_sub(b->size); free(b); }
    r->chunks = r->larges = NULL;
    r->pool_next = tl_pool; tl_pool = r;
}

void alx_region_exit(AlxRegion *r, AlxRegion *saved) {
    AlxRegion *c = cur_region();
    if (c != r && c != saved) { c->cur = alx_bump_cur; c->end = alx_bump_end; }
    if (c != saved) { alx_bump_cur = saved->cur; alx_bump_end = saved->end; }
    tl_cur = saved;
    region_release(r);
}

void alx_region_free(AlxRegion *r) {
    assert(r != &tl_prog && "alx_region_free of the program region");
    for (AlxRegion *c = cur_region(); c; c = c->parent)
        assert(c != r && "alx_region_free of a region that is (inside) the current one");
    region_release(r);
}

int64_t alx_region_bytes(AlxRegion *r) {
    if (!r->cbase) return (int64_t)r->counted;
    char *cur = r == cur_region() ? alx_bump_cur : r->cur;
    return (int64_t)(r->counted + (size_t)(cur - r->cbase));
}

/* `n` is rounded to 16 already. */
void *alx_alloc_slow(size_t n) {
    AlxRegion *r = cur_region();
    if (n > ALX_CHUNK / 16) {
        size_t sz = n + ALX_HDR;
        Blk *b = aligned_alloc(ALX_CHUNK, (sz + ALX_CHUNK - 1) & ~(size_t)(ALX_CHUNK - 1));
        if (!b) alx_panic("out of memory", "runtime");
        mem_add(sz);
        b->size = sz; b->next = r->larges; r->larges = b;
        r->counted += n;
        reg_blk(b, sz, r);
        return (char *)b + ALX_HDR;
    }
    Blk *c = tl_free;
    if (c) { tl_free = c->next; tl_nfree--; }
    else {
        c = aligned_alloc(ALX_CHUNK, ALX_CHUNK);
        if (!c) alx_panic("out of memory", "runtime");
        mem_add(ALX_CHUNK);
        c->size = ALX_CHUNK;
    }
    c->next = r->chunks; r->chunks = c;
    reg_blk(c, ALX_CHUNK, r);
    char *base = (char *)c + ALX_HDR;
    if (r->cbase) r->counted += (size_t)(alx_bump_cur - r->cbase);
    r->cbase = base;
    alx_bump_cur = base + n;
    alx_bump_end = (char *)c + ALX_CHUNK;
    return base;
}

/* ---------- panics and errors ---------- */

/* In a spawned task a panic ends only the task: the message is recorded and
 * control longjmps back to the task's entry (see task_main). Everywhere else
 * (main thread, pmap threads) a panic prints and aborts. */
static _Thread_local AlxTask *tl_task;
static _Noreturn void task_panic(char *msg);

void alx_panic(const char *what, const char *loc) {
    if (tl_task) {
        size_t n = (size_t)snprintf(NULL, 0, "alexandrite: %s at %s", what, loc) + 1;
        char *m = malloc(n);
        if (m) snprintf(m, n, "alexandrite: %s at %s", what, loc);
        task_panic(m);
    }
    fflush(stdout);
    fprintf(stderr, "alexandrite: %s at %s\n", what, loc);
    abort();
}

void alx_panic_str(AlxStr msg) {
    if (tl_task) {
        size_t n = (size_t)msg.len + 14;
        char *m = malloc(n);
        if (m) snprintf(m, n, "alexandrite: %.*s", (int)msg.len, msg.ptr);
        task_panic(m);
    }
    fflush(stdout);
    fprintf(stderr, "alexandrite: %.*s\n", (int)msg.len, msg.ptr);
    abort();
}

void alx_overflow(const char *loc) {
    static const char hint[] = "hint: add `#![overflow(promote)]` to this file to promote to bignums";
    if (tl_task) {
        size_t n = (size_t)snprintf(NULL, 0, "alexandrite: overflow at %s\n%s", loc, hint) + 1;
        char *m = malloc(n);
        if (m) snprintf(m, n, "alexandrite: overflow at %s\n%s", loc, hint);
        task_panic(m);
    }
    fflush(stdout);
    fprintf(stderr, "alexandrite: overflow at %s\n%s\n", loc, hint);
    abort();
}

void alx_die_str(AlxStr msg) {
    fflush(stdout);
    fwrite(msg.ptr, 1, (size_t)msg.len, stderr);
    fputc('\n', stderr);
    exit(1);
}

/* ---------- integers ---------- */

bool alx_try_pow(int64_t a, int64_t b, int64_t *r) {
    if (b < 0) return false;
    int64_t acc = 1;
    while (b > 0) {
        if (b & 1) {
            if (__builtin_mul_overflow(acc, a, &acc)) return false;
        }
        b >>= 1;
        if (b && __builtin_mul_overflow(a, a, &a)) return false;
    }
    *r = acc;
    return true;
}

int64_t alx_pow(int64_t a, int64_t b, const char *loc) {
    if (b < 0) alx_panic("negative exponent", loc);
    int64_t r;
    if (!alx_try_pow(a, b, &r)) alx_overflow(loc);
    return r;
}

int64_t alx_isqrt(int64_t n, const char *loc) {
    if (n < 0) alx_panic("Int.sqrt of a negative number", loc);
    int64_t x = (int64_t)__builtin_sqrt((double)n);
    while (x > 0 && x > n / x) x--;
    while ((x + 1) <= n / (x + 1)) x++;
    return x;
}

Arr_I64 alx_digits(int64_t v, const char *loc) {
    if (v < 0) alx_panic("`digits` of a negative number", loc);
    Arr_I64 a = Arr_I64_cap(20);
    do {
        Arr_I64_push(&a, v % 10);
        v /= 10;
    } while (v > 0);
    return a;
}

/* ---------- strings ---------- */

AlxStr alx_str_rev(AlxStr s) {
    char *p = alx_alloc((size_t)s.len);
    /* Reverse by UTF-8 characters, not bytes. */
    int64_t o = s.len;
    for (int64_t i = 0; i < s.len;) {
        int64_t n = alx_str_charlen(s, i);
        o -= n;
        memcpy(p + o, s.ptr + i, (size_t)n);
        i += n;
    }
    AlxStr r = { p, s.len };
    return r;
}

bool alx_str_eq(AlxStr a, AlxStr b) { return a.len == b.len && (a.len == 0 || memcmp(a.ptr, b.ptr, (size_t)a.len) == 0); }

int alx_str_cmp(AlxStr a, AlxStr b) {
    int64_t n = a.len < b.len ? a.len : b.len;
    int c = n ? memcmp(a.ptr, b.ptr, (size_t)n) : 0;
    if (c) return c;
    return (a.len > b.len) - (a.len < b.len);
}

AlxStr alx_str_delete(AlxStr s, AlxStr chars) {
    char *p = alx_alloc((size_t)s.len);
    int64_t o = 0;
    for (int64_t i = 0; i < s.len; i++) {
        bool del = false;
        for (int64_t j = 0; j < chars.len; j++) {
            if (s.ptr[i] == chars.ptr[j]) {
                del = true;
                break;
            }
        }
        if (!del) p[o++] = s.ptr[i];
    }
    AlxStr r = { p, o };
    return r;
}

/* Go's strings.Split: every field, empty ones included; an empty separator
 * splits after each UTF-8 sequence (an invalid byte on its own). */
Arr_Str alx_str_split(AlxStr s, AlxStr sep) {
    Arr_Str a = Arr_Str_cap(16);
    if (sep.len == 0) {
        for (int64_t i = 0; i < s.len;) {
            uint8_t c = (uint8_t)s.ptr[i];
            int64_t n = c < 0x80 ? 1 : c >= 0xF0 ? 4 : c >= 0xE0 ? 3 : c >= 0xC0 ? 2 : 1;
            if (i + n > s.len) n = 1;
            for (int64_t k = 1; k < n; k++)
                if (((uint8_t)s.ptr[i + k] & 0xC0) != 0x80) { n = 1; break; }
            AlxStr part = { s.ptr + i, n };
            Arr_Str_push(&a, part);
            i += n;
        }
        return a;
    }
    int64_t start = 0;
    for (int64_t i = 0; i + sep.len <= s.len;) {
        if (memcmp(s.ptr + i, sep.ptr, (size_t)sep.len) == 0) {
            AlxStr part = { s.ptr + start, i - start };
            Arr_Str_push(&a, part);
            i += sep.len;
            start = i;
        } else {
            i++;
        }
    }
    AlxStr last = { s.ptr + start, s.len - start };
    Arr_Str_push(&a, last);
    return a;
}

int64_t alx_str_to_i(AlxStr s) {
    /* Ruby: leading whitespace, optional sign, digits; garbage stops it. */
    int64_t i = 0, v = 0;
    bool neg = false;
    while (i < s.len && (s.ptr[i] == ' ' || s.ptr[i] == '\t' || s.ptr[i] == '\n')) i++;
    if (i < s.len && (s.ptr[i] == '-' || s.ptr[i] == '+')) neg = s.ptr[i++] == '-';
    for (; i < s.len && s.ptr[i] >= '0' && s.ptr[i] <= '9'; i++) {
        if (__builtin_mul_overflow(v, 10, &v) || __builtin_add_overflow(v, s.ptr[i] - '0', &v)) alx_overflow("String#to_i");
    }
    return neg ? -v : v;
}

int64_t alx_str_charlen(AlxStr s, int64_t i) {
    unsigned char c = (unsigned char)s.ptr[i];
    int64_t n = c < 0x80 ? 1 : c < 0xE0 ? 2 : c < 0xF0 ? 3 : 4;
    return i + n > s.len ? s.len - i : n;
}

AlxStr alx_str_sub(AlxStr s, int64_t i, int64_t n) {
    AlxStr r = { s.ptr + i, n };
    return r;
}

static int cmp_i64(const void *a, const void *b) {
    int64_t x = *(const int64_t *)a, y = *(const int64_t *)b;
    return (x > y) - (x < y);
}
static int cmp_str(const void *a, const void *b) { return alx_str_cmp(*(const AlxStr *)a, *(const AlxStr *)b); }
void alx_sort_i64(Arr_I64 *a) { if (a->len > 1) qsort(a->ptr, (size_t)a->len, sizeof(int64_t), cmp_i64); }
void alx_sort_str(Arr_Str *a) { if (a->len > 1) qsort(a->ptr, (size_t)a->len, sizeof(AlxStr), cmp_str); }

/* Read a whole file. On failure returns false and sets *not_found (ENOENT vs. any other error). */
static bool file_read(AlxStr path, AlxStr *out, bool *not_found) {
    char *cpath = alx_alloc((size_t)path.len + 1);
    memcpy(cpath, path.ptr, (size_t)path.len);
    cpath[path.len] = 0;
    FILE *f = fopen(cpath, "rb");
    if (!f) {
        *not_found = errno == ENOENT;
        return false;
    }
    fseek(f, 0, SEEK_END);
    long n = ftell(f);
    fseek(f, 0, SEEK_SET);
    char *p = alx_alloc((size_t)(n > 0 ? n : 1));
    size_t got = n > 0 ? fread(p, 1, (size_t)n, f) : 0;
    fclose(f);
    out->ptr = p;
    out->len = (int64_t)got;
    return true;
}

int64_t alx_file_status(AlxStr path) {
    AlxStr out;
    bool not_found = false;
    if (file_read(path, &out, &not_found)) return 0;
    return not_found ? 1 : 2;
}

AlxStr alx_file_read_or_empty(AlxStr path) {
    AlxStr out;
    bool not_found;
    if (file_read(path, &out, &not_found)) return out;
    AlxStr e = { "", 0 };
    return e;
}

/* ---------- test support ---------- */

int64_t alx_now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

/* Capture redirects file descriptor 1 into an anonymous file, so it sees
 * everything printed by any backend's `puts`. One capture at a time. */
static FILE *cap_file;
static int cap_saved = -1;

int64_t alx_cap_begin(void) {
    fflush(stdout);
    if (cap_file) return 0;
    cap_file = tmpfile();
    if (!cap_file) return 0;
    cap_saved = dup(1);
    dup2(fileno(cap_file), 1);
    return 0;
}

AlxStr alx_cap_end(void) {
    AlxStr e = { "", 0 };
    if (!cap_file) return e;
    fflush(stdout);
    dup2(cap_saved, 1);
    close(cap_saved);
    cap_saved = -1;
    fflush(cap_file);
    long n = ftell(cap_file);
    fseek(cap_file, 0, SEEK_SET);
    char *p = alx_alloc((size_t)(n > 0 ? n : 1));
    size_t got = n > 0 ? fread(p, 1, (size_t)n, cap_file) : 0;
    fclose(cap_file);
    cap_file = NULL;
    AlxStr out = { p, (int64_t)got };
    return out;
}

/* ---------- floats ---------- */

static AlxStr str_of(const char *p, size_t n) {
    char *q = alx_alloc(n);
    memcpy(q, p, n);
    AlxStr s = { q, (int64_t)n };
    return s;
}

/* A Float as Go's fmt prints it (%v): the shortest digits that read back
 * as x; exponent form when the exponent is < -4 or >= 6, e.g. 100, 0.5,
 * 1e+06, 1.5e-07, +Inf, NaN. */
AlxStr alx_f_to_s(double x) {
    if (isnan(x)) return str_of("NaN", 3);
    if (isinf(x)) return x < 0 ? str_of("-Inf", 4) : str_of("+Inf", 4);
    if (x == 0) return signbit(x) ? str_of("-0", 2) : str_of("0", 1);
    char e[40];
    for (int p = 1; p <= 17; p++) {
        snprintf(e, sizeof e, "%.*e", p - 1, x);
        if (strtod(e, NULL) == x) break;
    }
    /* e = [-]d[.ddd]e[+-]XX */
    const char *q = e;
    bool neg = *q == '-';
    if (neg) q++;
    char digits[24];
    int n = 0;
    for (; *q && *q != 'e'; q++)
        if (*q != '.') digits[n++] = *q;
    int exp10 = atoi(q + 1);
    int decpt = exp10 + 1;
    char out[64];
    int o = 0;
    if (neg) out[o++] = '-';
    if (exp10 < -4 || exp10 >= 6) {
        out[o++] = digits[0];
        if (n > 1) {
            out[o++] = '.';
            for (int i = 1; i < n; i++) out[o++] = digits[i];
        }
        o += snprintf(out + o, sizeof out - (size_t)o, "e%+03d", exp10);
    } else if (decpt <= 0) {
        out[o++] = '0';
        out[o++] = '.';
        for (int i = 0; i < -decpt; i++) out[o++] = '0';
        for (int i = 0; i < n; i++) out[o++] = digits[i];
    } else {
        for (int i = 0; i < decpt; i++) out[o++] = i < n ? digits[i] : '0';
        if (n > decpt) {
            out[o++] = '.';
            for (int i = decpt; i < n; i++) out[o++] = digits[i];
        }
    }
    return str_of(out, (size_t)o);
}

/* format("%.Nf", x) */
AlxStr alx_f_fmt(double x, int64_t digits) {
    if (isnan(x)) return str_of("NaN", 3);
    if (isinf(x)) return x < 0 ? str_of("-Inf", 4) : str_of("+Inf", 4);
    int n = snprintf(NULL, 0, "%.*f", (int)digits, x);
    char *p = alx_alloc((size_t)n + 1);
    snprintf(p, (size_t)n + 1, "%.*f", (int)digits, x);
    AlxStr s = { p, n };
    return s;
}

/* format("%.Ne", x), as Go prints it. */
AlxStr alx_f_fmt_e(double x, int64_t digits, bool upper) {
    if (isnan(x)) return str_of("NaN", 3);
    if (isinf(x)) return x < 0 ? str_of("-Inf", 4) : str_of("+Inf", 4);
    const char *f = upper ? "%.*E" : "%.*e";
    int n = snprintf(NULL, 0, f, (int)digits, x);
    char *p = alx_alloc((size_t)n + 1);
    snprintf(p, (size_t)n + 1, f, (int)digits, x);
    AlxStr s = { p, n };
    return s;
}

/* Pad to `width` runes: flags 1 = on the right, 2 = zeros after a sign. */
AlxStr alx_str_pad(AlxStr s, int64_t width, int64_t flags) {
    int64_t runes = 0;
    for (int64_t i = 0; i < s.len; i++) runes += ((uint8_t)s.ptr[i] & 0xC0) != 0x80;
    if (runes >= width) return s;
    int64_t pad = width - runes;
    char *p = alx_alloc((size_t)(s.len + pad));
    if (flags & 1) {
        memcpy(p, s.ptr, (size_t)s.len);
        memset(p + s.len, ' ', (size_t)pad);
    } else if (flags & 2) {
        int64_t sign = s.len > 0 && (s.ptr[0] == '-' || s.ptr[0] == '+' || s.ptr[0] == ' ');
        memcpy(p, s.ptr, (size_t)sign);
        memset(p + sign, '0', (size_t)pad);
        memcpy(p + sign + pad, s.ptr + sign, (size_t)(s.len - sign));
    } else {
        memset(p, ' ', (size_t)pad);
        memcpy(p + pad, s.ptr, (size_t)s.len);
    }
    AlxStr r = { p, s.len + pad };
    return r;
}

/* Go's strconv.Quote: printable runes as they are, the usual backslash
 * escapes, \xHH for other bytes below 0x80 and for invalid UTF-8, \uHHHH
 * for C1 controls. */
AlxStr alx_str_quote(AlxStr s) {
    char *p = alx_alloc((size_t)s.len * 4 + 2);
    int64_t o = 0;
    p[o++] = '"';
    static const char hex[] = "0123456789abcdef";
    for (int64_t i = 0; i < s.len;) {
        uint8_t c = (uint8_t)s.ptr[i];
        if (c < 0x80) {
            const char *e = NULL;
            switch (c) {
            case '\a': e = "\\a"; break;
            case '\b': e = "\\b"; break;
            case '\f': e = "\\f"; break;
            case '\n': e = "\\n"; break;
            case '\r': e = "\\r"; break;
            case '\t': e = "\\t"; break;
            case '\v': e = "\\v"; break;
            case '\\': e = "\\\\"; break;
            case '"': e = "\\\""; break;
            }
            if (e) { p[o++] = e[0]; p[o++] = e[1]; }
            else if (c < 0x20 || c == 0x7f) { p[o++] = '\\'; p[o++] = 'x'; p[o++] = hex[c >> 4]; p[o++] = hex[c & 15]; }
            else p[o++] = (char)c;
            i++;
            continue;
        }
        /* A multi-byte rune: copied if well-formed, else \xHH per byte. */
        int n = c >= 0xF0 ? 4 : c >= 0xE0 ? 3 : c >= 0xC2 ? 2 : 0;
        bool ok = n > 0 && i + n <= s.len && c <= 0xF4;
        for (int k = 1; ok && k < n; k++) ok = ((uint8_t)s.ptr[i + k] & 0xC0) == 0x80;
        if (ok && n == 3 && c == 0xE0) ok = (uint8_t)s.ptr[i + 1] >= 0xA0;
        if (ok && n == 3 && c == 0xED) ok = (uint8_t)s.ptr[i + 1] < 0xA0;
        if (ok && n == 4 && c == 0xF0) ok = (uint8_t)s.ptr[i + 1] >= 0x90;
        if (ok && n == 4 && c == 0xF4) ok = (uint8_t)s.ptr[i + 1] < 0x90;
        if (!ok) { p[o++] = '\\'; p[o++] = 'x'; p[o++] = hex[c >> 4]; p[o++] = hex[c & 15]; i++; continue; }
        if (n == 2 && c == 0xC2 && (uint8_t)s.ptr[i + 1] < 0xA0) {
            uint8_t r = (uint8_t)s.ptr[i + 1];
            memcpy(p + o, "\\u00", 4); o += 4; p[o++] = hex[r >> 4]; p[o++] = hex[r & 15];
            i += 2;
            continue;
        }
        memcpy(p + o, s.ptr + i, (size_t)n);
        o += n;
        i += n;
    }
    p[o++] = '"';
    AlxStr r = { p, o };
    return r;
}

int64_t alx_f_to_i(double x, const char *loc) {
    if (isnan(x) || isinf(x)) alx_panic("Float#to_i of NaN or Infinity", loc);
    if (x >= 9223372036854775808.0 || x < -9223372036854775808.0) alx_panic("Float#to_i: out of Int range", loc);
    return (int64_t)x;
}

void alx_puts_f64(double x) { alx_puts_str(alx_f_to_s(x)); }

AlxStr alx_str_cat(int64_t n, const AlxStr *parts) {
    int64_t len = 0;
    for (int64_t i = 0; i < n; i++) len += parts[i].len;
    char *p = alx_alloc((size_t)len);
    int64_t o = 0;
    for (int64_t i = 0; i < n; i++) {
        if (parts[i].len) memcpy(p + o, parts[i].ptr, (size_t)parts[i].len);
        o += parts[i].len;
    }
    AlxStr s = { p, len };
    return s;
}

/* ---------- sized integers ---------- */

AlxStr alx_u64_to_s(int64_t bits) {
    char buf[24];
    int n = snprintf(buf, sizeof buf, "%llu", (unsigned long long)(uint64_t)bits);
    return str_of(buf, (size_t)n);
}

/* Go's %x %X %o %b: sign, then the magnitude in the base (U64: unsigned). */
AlxStr alx_int_fmt(int64_t v, int64_t base, bool upper, bool is_u64) {
    const char *digits = upper ? "0123456789ABCDEF" : "0123456789abcdef";
    bool neg = !is_u64 && v < 0;
    uint64_t m = neg ? -(uint64_t)v : (uint64_t)v;
    char buf[72];
    int o = sizeof buf;
    do {
        buf[--o] = digits[m % (uint64_t)base];
        m /= (uint64_t)base;
    } while (m);
    if (neg) buf[--o] = '-';
    return str_of(buf + o, sizeof buf - (size_t)o);
}

int64_t alx_f_to_u64(double x, const char *loc) {
    if (isnan(x) || isinf(x)) alx_panic("Float#to_u64 of NaN or Infinity", loc);
    if (x < 0 || x >= 18446744073709551616.0) alx_panic("conversion overflow: the value doesn't fit U64", loc);
    return (int64_t)(uint64_t)x;
}

/* A Rune as UTF-8; invalid code points become U+FFFD, as in Go. */
AlxStr alx_rune_to_s(int64_t r) {
    if (r < 0 || r > 0x10FFFF || (r >= 0xD800 && r <= 0xDFFF)) r = 0xFFFD;
    char b[4];
    int n;
    if (r < 0x80) { b[0] = (char)r; n = 1; }
    else if (r < 0x800) { b[0] = (char)(0xC0 | (r >> 6)); b[1] = (char)(0x80 | (r & 0x3F)); n = 2; }
    else if (r < 0x10000) { b[0] = (char)(0xE0 | (r >> 12)); b[1] = (char)(0x80 | ((r >> 6) & 0x3F)); b[2] = (char)(0x80 | (r & 0x3F)); n = 3; }
    else { b[0] = (char)(0xF0 | (r >> 18)); b[1] = (char)(0x80 | ((r >> 12) & 0x3F)); b[2] = (char)(0x80 | ((r >> 6) & 0x3F)); b[3] = (char)(0x80 | (r & 0x3F)); n = 4; }
    return str_of(b, (size_t)n);
}

AlxStr alx_str_from_bytes(const uint8_t *p, int64_t n) { return str_of((const char *)p, (size_t)n); }

void alx_puts_u64(int64_t bits) { printf("%llu\n", (unsigned long long)(uint64_t)bits); }

/* ---------- output ---------- */

void alx_puts_i64(int64_t v) { printf("%lld\n", (long long)v); }
void alx_puts_str(AlxStr s) {
    fwrite(s.ptr, 1, (size_t)s.len, stdout);
    if (s.len == 0 || s.ptr[s.len - 1] != '\n') fputc('\n', stdout);
}
void alx_puts_bool(bool b) { puts(b ? "true" : "false"); }
void alx_print_str(AlxStr s) { fwrite(s.ptr, 1, (size_t)s.len, stdout); }

/* ---------- parallel map ---------- */

typedef struct {
    const char *in;
    char *out;
    int64_t lo, hi;
    size_t in_size, out_size;
    AlxWorker fn;
} PmapJob;

static _Thread_local bool tl_uncounted;

static void *pmap_run(void *arg) {
    PmapJob *j = arg;
    for (int64_t i = j->lo; i < j->hi; i++) j->fn(j->in + (size_t)i * j->in_size, j->out + (size_t)i * j->out_size);
    return NULL;
}

static void *pmap_thread(void *arg) {
    tl_uncounted = true; /* not a task: ignored by deadlock detection */
    return pmap_run(arg);
}

void alx_pmap(const void *in, int64_t n, size_t in_size, void *out, size_t out_size, AlxWorker fn) {
    long cpus = sysconf(_SC_NPROCESSORS_ONLN);
    const char *forced = getenv("ALX_THREADS");
    if (forced && atol(forced) > 0) cpus = atol(forced);
    int64_t threads = cpus > 0 ? cpus : 4;
    if (threads > 64) threads = 64;
    if (n < threads * 64) threads = n / 64 + 1;
    PmapJob jobs[64];
    pthread_t tids[64];
    int64_t chunk = (n + threads - 1) / threads;
    for (int64_t t = 0; t < threads; t++) {
        int64_t lo = t * chunk, hi = lo + chunk > n ? n : lo + chunk;
        jobs[t] = (PmapJob){ in, out, lo, hi, in_size, out_size, fn };
        if (t > 0) pthread_create(&tids[t], NULL, pmap_thread, &jobs[t]);
    }
    pmap_run(&jobs[0]);
    for (int64_t t = 1; t < threads; t++) pthread_join(tids[t], NULL);
}

/* ---------- tasks and channels ----------
 * Tasks are stackful coroutines (hand-written context switch) multiplexed
 * over N persistent worker threads. Blocking operations park the task; the
 * worker goes on to run another one.
 *
 * Stacks: every worker owns one mmap'd run stack (guard page below, size
 * ALX_TASK_STACK, default 256 KiB) that all its tasks run on. When a task
 * parks, its live part [sp, top) is copied to the heap; it is copied back to
 * the same addresses on resume. A parked task costs its used bytes (about a
 * kilobyte) instead of a page-rounded private stack, so 100000 tasks fit.
 * That relies on nothing outside the task pointing into its stack while it
 * is parked (wait-list nodes live in the task, not on the stack). Under
 * ASan/TSan (shadow state is per address) and with -DALX_PRIVATE_STACKS each
 * task gets a private pooled stack instead.
 *
 * A task is PINNED to the worker that first runs it. Compiled C caches the
 * address of a thread-local (alx_bump_cur / alx_bump_end) in a callee-saved
 * register across calls such as alx_chan_recv, so a task resumed on another
 * thread would keep writing the old thread's bump cache. Pinning makes that
 * impossible; the cost is that a woken task waits for its own worker. New
 * tasks sit in a global queue and are taken by whichever worker is free.
 *
 * One global mutex g_mu guards every channel, task and run queue. Each
 * channel (and each task, for `wait`) has a wait list; a state change wakes
 * only that list's waiters (a woken waiter re-checks its condition, so
 * spurious wakeups are harmless). Deadlock detection: g_runnable counts the
 * threads/tasks that are running, queued or not yet started (main included);
 * when it would drop to zero, main (always parked then) reports it. */

#ifdef __SANITIZE_THREAD__
#  define ALX_TSAN 1
#elif defined(__has_feature)
#  if __has_feature(thread_sanitizer)
#    define ALX_TSAN 1
#  endif
#endif
#if defined(ALX_ASAN) || defined(ALX_TSAN) || defined(ALX_PRIVATE_STACKS)
#  define ALX_PRIV 1
#endif
#ifdef ALX_ASAN
void __sanitizer_start_switch_fiber(void **fake_save, const void *bottom, size_t size);
void __sanitizer_finish_switch_fiber(void *fake_save, const void **bottom_old, size_t *size_old);
void __asan_unpoison_memory_region(const volatile void *p, size_t n);
#endif
#ifdef ALX_TSAN
void *__tsan_get_current_fiber(void);
void *__tsan_create_fiber(unsigned flags);
void __tsan_destroy_fiber(void *fiber);
void __tsan_switch_to_fiber(void *fiber, unsigned flags);
#endif

/* Context switch: push the callee-saved registers, store sp in *from, load
 * `to`, pop, return into it. */
void alx_ctx_switch(void **from, void *to);
#ifdef __APPLE__
#  define CTX_SYM "_alx_ctx_switch"
#  define CTX_TYPE ""
#elif defined(__aarch64__)
#  define CTX_SYM "alx_ctx_switch"
#  define CTX_TYPE ".type alx_ctx_switch,%function\n"
#else
#  define CTX_SYM "alx_ctx_switch"
#  define CTX_TYPE ".type alx_ctx_switch,@function\n"
#endif
#if defined(__aarch64__)
enum { CTX_FRAME = 160 };
__asm__(".text\n.p2align 2\n.globl " CTX_SYM "\n" CTX_TYPE CTX_SYM ":\n"
    "sub sp, sp, #160\n"
    "stp x19, x20, [sp, #0]\n stp x21, x22, [sp, #16]\n stp x23, x24, [sp, #32]\n"
    "stp x25, x26, [sp, #48]\n stp x27, x28, [sp, #64]\n stp x29, x30, [sp, #80]\n"
    "stp d8, d9, [sp, #96]\n stp d10, d11, [sp, #112]\n stp d12, d13, [sp, #128]\n stp d14, d15, [sp, #144]\n"
    "mov x9, sp\n str x9, [x0]\n mov sp, x1\n"
    "ldp x19, x20, [sp, #0]\n ldp x21, x22, [sp, #16]\n ldp x23, x24, [sp, #32]\n"
    "ldp x25, x26, [sp, #48]\n ldp x27, x28, [sp, #64]\n ldp x29, x30, [sp, #80]\n"
    "ldp d8, d9, [sp, #96]\n ldp d10, d11, [sp, #112]\n ldp d12, d13, [sp, #128]\n ldp d14, d15, [sp, #144]\n"
    "add sp, sp, #160\n ret\n");
#elif defined(__x86_64__)
enum { CTX_FRAME = 64 };
__asm__(".text\n.p2align 4\n.globl " CTX_SYM "\n" CTX_TYPE CTX_SYM ":\n"
    "pushq %rbp\n pushq %rbx\n pushq %r12\n pushq %r13\n pushq %r14\n pushq %r15\n"
    "movq %rsp, (%rdi)\n movq %rsi, %rsp\n"
    "popq %r15\n popq %r14\n popq %r13\n popq %r12\n popq %rbx\n popq %rbp\n ret\n");
#else
#  error "alx.c: no context switch for this architecture"
#endif

/* The initial frame of a new task: switching to it "returns" into `entry`. */
static void *ctx_init(char *top, void (*entry)(void)) {
    void **sp = (void **)(top - CTX_FRAME);
    memset(sp, 0, CTX_FRAME);
#if defined(__aarch64__)
    sp[11] = (void *)entry;      /* x30 */
#else
    sp[6] = (void *)entry;       /* return address; sp[7] = 0 pads the alignment */
#endif
    return sp;
}

static pthread_mutex_t g_mu = PTHREAD_MUTEX_INITIALIZER;
static int64_t g_runnable = 1;   /* main */

typedef struct WNode { struct WNode *next, **pprev; struct Parker *p; } WNode;
typedef struct Parker {
    AlxTask *task;               /* NULL: a plain thread (parks on cv) */
    pthread_cond_t cv;
    bool parked, dead, counted, cv_init;
} Parker;
typedef struct Worker Worker;
typedef struct { AlxTask *head, *tail; } TQ;

struct AlxTask {
    AlxWorker fn;
    void *env, *res;
    size_t out_size;
    bool finished, panicked;
    char *msg;
    jmp_buf jb;
    WNode *w;                    /* waiters for `finished` */
    Parker pk;
    AlxTask *qnext;
    Worker *home;                /* the worker it runs on (set at first run) */
    void *sp, *fake, *fiber;     /* saved sp; sanitizer fiber state */
    char *guard, *lo;            /* its stack: guard page, usable low end */
    AlxRegion *cur;              /* saved tl_cur */
    bool dead;                   /* done for good: the stack can be recycled */
    bool started;
    char *save;                  /* copy of the live stack while parked */
    size_t nsave, csave;
    WNode wn[4], *wnp;           /* wait-list nodes while parked (not on the stack) */
};

struct Worker {
    pthread_cond_t cv;
    TQ q;
    bool idle;
    Worker *idle_next;
    void *sp, *fake, *fiber;     /* the scheduler's context */
    const void *sbot;
    size_t ssz;
    char *guard, *lo;            /* the run stack (copy mode) */
};

static TQ g_newq;
static Worker *g_workers, *g_idle;
static int g_nworkers;
static size_t g_stk_size, g_page;

static void tq_push(TQ *q, AlxTask *t) { t->qnext = NULL; if (q->tail) q->tail->qnext = t; else q->head = t; q->tail = t; }
static AlxTask *tq_pop(TQ *q) {
    AlxTask *t = q->head;
    if (t && !(q->head = t->qnext)) q->tail = NULL;
    return t;
}

static void wl_add(WNode **head, WNode *n, Parker *p) {
    n->p = p; n->next = *head;
    if (*head) (*head)->pprev = &n->next;
    n->pprev = head; *head = n;
}
static void wl_del(WNode *n) {
    *n->pprev = n->next;
    if (n->next) n->next->pprev = n->pprev;
}

static void idle_wake(Worker *w) {
    for (Worker **pp = &g_idle; *pp; pp = &(*pp)->idle_next)
        if (*pp == w) { *pp = w->idle_next; break; }
    w->idle = false;
    pthread_cond_signal(&w->cv);
}

/* Make a parked task runnable again on its worker (g_mu held). */
static void enqueue(AlxTask *t) {
    Worker *w = t->home;
    tq_push(&w->q, t);
    if (w->idle) idle_wake(w);
}

static Parker *g_main_p;
/* Tasks asleep on a timer: they'll wake on their own, so not a deadlock. */
static int64_t g_sleepers;

static void unpark(Parker *p) {
    if (!p->parked) return;
    p->parked = false;
    if (p->counted) g_runnable++;
    if (p->task) enqueue(p->task); else pthread_cond_signal(&p->cv);
}

static void wake(WNode **head) { for (WNode *n = *head; n; n = n->next) unpark(n->p); }

/* Nothing runnable but main is parked: nothing can ever wake it. */
static void check_dead(void) {
    if (g_runnable == 0 && g_sleepers == 0 && g_main_p && g_main_p->parked) { g_main_p->dead = true; unpark(g_main_p); }
}

static _Thread_local Parker tl_pk;
static Parker *cur_pk(void) {
    if (tl_task) return &tl_task->pk;
    Parker *p = &tl_pk;
    if (!p->cv_init) { pthread_cond_init(&p->cv, NULL); p->cv_init = true; p->counted = !tl_uncounted; }
    return p;
}

/* Task side: flush the region state and switch to the scheduler. */
static void sw_out(AlxTask *t, bool dying) {
    AlxRegion *c = cur_region();
    c->cur = alx_bump_cur; c->end = alx_bump_end;
    t->cur = tl_cur;
    Worker *w = t->home;
#ifdef ALX_ASAN
    __sanitizer_start_switch_fiber(dying ? NULL : &t->fake, w->sbot, w->ssz);
#endif
#ifdef ALX_TSAN
    __tsan_switch_to_fiber(w->fiber, 0);
#endif
    alx_ctx_switch(&t->sp, w->sp);
#ifdef ALX_ASAN
    __sanitizer_finish_switch_fiber(t->fake, NULL, NULL);
#endif
    (void)dying;
}

/* With g_mu held, the awaited condition false: register on `heads` and sleep
 * until one of those lists is woken. False (still holding g_mu) if that can
 * never happen. Only main is ever told so: a task that finds everything
 * asleep parks, and main is woken to report it. */
static bool block_wait(WNode ***heads, int n) {
    Parker *p = cur_pk();
    if (p->counted && !p->task && g_runnable == 1 && g_sleepers == 0) return false;
    WNode ndv[p->task ? 1 : (n ? n : 1)], *nd = ndv;
    if (p->task) {
        AlxTask *t = p->task;
        if (n <= 4) nd = t->wn;
        else if (!(nd = t->wnp = realloc(t->wnp, (size_t)n * sizeof *nd))) alx_panic("out of memory", "runtime");
    }
    for (int i = 0; i < n; i++) wl_add(heads[i], &nd[i], p);
    p->parked = true; p->dead = false;
    if (p->counted) {
        g_runnable--;
        if (!p->task) g_main_p = p;
        else check_dead();
    }
    if (p->task) {
        pthread_mutex_unlock(&g_mu);
        sw_out(p->task, false);
        pthread_mutex_lock(&g_mu);
    } else {
        while (p->parked) pthread_cond_wait(&p->cv, &g_mu);
    }
    for (int i = 0; i < n; i++) wl_del(&nd[i]);
    if (p->dead) { p->dead = false; return false; }
    return true;
}
static bool block_wait1(WNode **head) { WNode **hs[1] = { head }; return block_wait(hs, 1); }

static _Noreturn void deadlock(void) {
    pthread_mutex_unlock(&g_mu);
    alx_panic("all tasks are asleep: deadlock", "runtime");
}

static _Noreturn void task_panic(char *msg) {
    AlxTask *t = tl_task;
    t->msg = msg ? msg : (char *)"alexandrite: out of memory";
    _longjmp(t->jb, 1);
}

/* ---- stacks ---- */

static size_t parse_size(const char *s, size_t dflt) {
    char *e;
    double v = strtod(s, &e);
    if (e == s || v <= 0) return dflt;
    if (*e == 'k' || *e == 'K') v *= 1024;
    else if (*e == 'm' || *e == 'M') v *= 1024 * 1024;
    return (size_t)v;
}

static char *stack_map(void) {
    void *m = mmap(NULL, g_page + g_stk_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
    if (m == MAP_FAILED) return NULL;
    if (mprotect(m, g_page, PROT_NONE)) { munmap(m, g_page + g_stk_size); return NULL; }
    return m;
}

#ifdef ALX_PRIV
static pthread_mutex_t g_stk_mu = PTHREAD_MUTEX_INITIALIZER;
static char *g_stk_pool;         /* free stacks (guard base), linked through their top word */
static int g_stk_n;
enum { STK_POOL_CAP = 256 };

static char *stack_link(char *base) { return base + g_page + g_stk_size - sizeof(void *); }

static bool stack_get(AlxTask *t) {
    pthread_mutex_lock(&g_stk_mu);
    char *base = g_stk_pool;
    if (base) { g_stk_pool = *(char **)stack_link(base); g_stk_n--; }
    pthread_mutex_unlock(&g_stk_mu);
    if (!base && !(base = stack_map())) return false;
    t->guard = base; t->lo = base + g_page;
#ifdef ALX_ASAN
    __asan_unpoison_memory_region(t->lo, g_stk_size);
#endif
    return true;
}

static void stack_put(AlxTask *t) {
    char *base = t->guard;
    pthread_mutex_lock(&g_stk_mu);
    if (g_stk_n < STK_POOL_CAP) {
        *(char **)stack_link(base) = g_stk_pool; g_stk_pool = base; g_stk_n++;
        base = NULL;
    }
    pthread_mutex_unlock(&g_stk_mu);
    if (base) munmap(base, g_page + g_stk_size);
    t->guard = t->lo = NULL;
}
#endif

/* A fault in a task's guard page is a stack overflow. */
static struct sigaction old_segv, old_bus;
static void fault_handler(int sig, siginfo_t *si, void *uc) {
    AlxTask *t = tl_task;
    char *a = si->si_addr;
    if (t && t->guard && a >= t->guard && a < t->guard + g_page) {
        static const char m[] = "alexandrite: stack overflow in a task\n";
        fflush(stdout);
        ssize_t r = write(2, m, sizeof m - 1);
        (void)r;
        abort();
    }
    struct sigaction *o = sig == SIGSEGV ? &old_segv : &old_bus;
    if (o->sa_flags & SA_SIGINFO) o->sa_sigaction(sig, si, uc);
    else if (o->sa_handler != SIG_DFL && o->sa_handler != SIG_IGN) o->sa_handler(sig);
    else signal(sig, SIG_DFL);   /* returning re-faults into the default action */
}

/* ---- scheduler ---- */

static void task_finish(AlxTask *t) {
    pthread_mutex_lock(&g_mu);
    t->finished = true;
    wake(&t->w);
    g_runnable--;
    check_dead();
    pthread_mutex_unlock(&g_mu);
}

static void task_entry(void) {
    AlxTask *t = tl_task;
#ifdef ALX_ASAN
    __sanitizer_finish_switch_fiber(NULL, &t->home->sbot, &t->home->ssz);
#endif
    if (_setjmp(t->jb) == 0) t->fn(t->env, t->res);
    else t->panicked = true;
    task_finish(t);
    t->dead = true;
    sw_out(t, true);
    abort();
}

static void run_task(Worker *w, AlxTask *t) {
    if (!t->started) {
        t->started = true;
#ifdef ALX_PRIV
        if (!stack_get(t)) {
            t->msg = (char *)"alexandrite: cannot allocate a task stack";
            t->panicked = true;
            task_finish(t);
            return;
        }
#else
        t->guard = w->guard; t->lo = w->lo;
#endif
        t->sp = ctx_init(t->lo + g_stk_size, task_entry);
#ifdef ALX_TSAN
        t->fiber = __tsan_create_fiber(0);
#endif
    }
#ifndef ALX_PRIV
    else memcpy(t->sp, t->save, t->nsave);
#endif
    AlxRegion *r = t->cur ? t->cur : &tl_prog;
    alx_bump_cur = r->cur; alx_bump_end = r->end;
    tl_cur = t->cur;
    tl_task = t;
#ifdef ALX_ASAN
    __sanitizer_start_switch_fiber(&w->fake, t->lo, g_stk_size);
#endif
#ifdef ALX_TSAN
    __tsan_switch_to_fiber(t->fiber, 0);
#endif
    alx_ctx_switch(&w->sp, t->sp);
#ifdef ALX_ASAN
    __sanitizer_finish_switch_fiber(w->fake, NULL, NULL);
#endif
    tl_task = NULL;
#ifdef ALX_PRIV
    if (t->dead) {
#ifdef ALX_TSAN
        __tsan_destroy_fiber(t->fiber);
#endif
        stack_put(t);
    }
#else
    if (!t->dead) {
        size_t n = (size_t)(w->lo + g_stk_size - (char *)t->sp);
        if (n > t->csave) {
            free(t->save);
            if (!(t->save = malloc(t->csave = n))) alx_panic("out of memory", "runtime");
        }
        memcpy(t->save, t->sp, n);
        t->nsave = n;
    } else {
        free(t->save); t->save = NULL; t->csave = t->nsave = 0;
    }
#endif
    if (t->dead) { free(t->wnp); t->wnp = NULL; }
}

static void *worker_main(void *arg) {
    Worker *w = arg;
#ifndef ALX_PRIV
    w->guard = stack_map();
    if (!w->guard) alx_panic("cannot start a task", "runtime");
    w->lo = w->guard + g_page;
#endif
    size_t ss = 64 << 10;
    stack_t st = { .ss_sp = malloc(ss), .ss_size = ss };
    if (st.ss_sp) sigaltstack(&st, NULL);
#ifdef ALX_TSAN
    w->fiber = __tsan_get_current_fiber();
#endif
    pthread_mutex_lock(&g_mu);
    for (;;) {
        AlxTask *t = tq_pop(&w->q);
        if (!t && (t = tq_pop(&g_newq))) t->home = w;
        if (!t) {
            w->idle = true; w->idle_next = g_idle; g_idle = w;
            do pthread_cond_wait(&w->cv, &g_mu); while (w->idle);
            continue;
        }
        pthread_mutex_unlock(&g_mu);
        run_task(w, t);
        pthread_mutex_lock(&g_mu);
    }
    return NULL;
}

/* With g_mu held, at the first spawn. False if no worker could start. */
static bool workers_start(void) {
    if (g_nworkers) return true;
    g_page = (size_t)sysconf(_SC_PAGESIZE);
#if defined(ALX_ASAN) || defined(ALX_TSAN)
    size_t dflt = 1 << 20;       /* instrumented frames are big */
#else
    size_t dflt = 256 << 10;
#endif
    const char *e = getenv("ALX_TASK_STACK");
    g_stk_size = e ? parse_size(e, dflt) : dflt;
    if (g_stk_size < 4 * g_page) g_stk_size = 4 * g_page;
    g_stk_size = (g_stk_size + g_page - 1) & ~(g_page - 1);
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = fault_handler;
    sa.sa_flags = SA_SIGINFO | SA_ONSTACK;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGSEGV, &sa, &old_segv);
    sigaction(SIGBUS, &sa, &old_bus);
    long n = sysconf(_SC_NPROCESSORS_ONLN);
    e = getenv("ALX_PROCS");
    if (e && atol(e) > 0) n = atol(e);
    if (n < 1) n = 1;
    if (n > 256) n = 256;
    g_workers = calloc((size_t)n, sizeof(Worker));
    if (!g_workers) return false;
    pthread_attr_t at;
    pthread_attr_init(&at);
    pthread_attr_setdetachstate(&at, PTHREAD_CREATE_DETACHED);
    int started = 0;
    for (long i = 0; i < n; i++) {
        pthread_cond_init(&g_workers[i].cv, NULL);
        pthread_t th;
        if (pthread_create(&th, &at, worker_main, &g_workers[i])) break;
        started++;
    }
    pthread_attr_destroy(&at);
    g_nworkers = started;
    return started > 0;
}

AlxTask *alx_spawn(AlxWorker fn, const void *env, size_t in_size, size_t out_size) {
    AlxTask *t = calloc(1, sizeof *t);
    if (!t) alx_panic("out of memory", "runtime");
    t->fn = fn;
    t->out_size = out_size;
    t->env = malloc(in_size ? in_size : 1);
    t->res = calloc(1, out_size ? out_size : 1);
    if (!t->env || !t->res) alx_panic("out of memory", "runtime");
    if (in_size) memcpy(t->env, env, in_size);
    t->pk.task = t; t->pk.counted = true;
    pthread_mutex_lock(&g_mu);
    if (!workers_start()) {
        pthread_mutex_unlock(&g_mu);
        alx_panic("cannot start a task", "runtime");
    }
    g_runnable++;
    tq_push(&g_newq, t);
    if (g_idle) idle_wake(g_idle);
    pthread_mutex_unlock(&g_mu);
    return t;
}

bool alx_task_wait(AlxTask *t, void *out, AlxStr *msg) {
    pthread_mutex_lock(&g_mu);
    while (!t->finished)
        if (!block_wait1(&t->w)) deadlock();
    pthread_mutex_unlock(&g_mu);
    if (t->panicked) {
        msg->ptr = t->msg;
        msg->len = (int64_t)strlen(t->msg);
        return false;
    }
    if (t->out_size) memcpy(out, t->res, t->out_size);
    return true;
}

struct AlxChan {
    size_t esz;
    int64_t cap, count, head;
    char *buf;                     /* ring buffer (cap > 0) */
    char *slot;                    /* rendezvous slot (cap == 0) */
    bool slot_full, closed;
    uint64_t send_seq, taken_seq;  /* ticket of the slot's sender / last taken */
    int64_t recv_waiting, send_waiting;
    WNode *w;                      /* parked senders / receivers / selects */
};

AlxChan *alx_chan_new(int64_t cap, size_t esz) {
    if (cap < 0) cap = 0;
    AlxChan *c = calloc(1, sizeof *c);
    if (!c) alx_panic("out of memory", "runtime");
    c->esz = esz;
    c->cap = cap;
    if (cap > 0) c->buf = malloc((size_t)cap * (esz ? esz : 1));
    else c->slot = malloc(esz ? esz : 1);
    if (!c->buf && !c->slot) alx_panic("out of memory", "runtime");
    return c;
}

int64_t alx_chan_len(AlxChan *c) {
    pthread_mutex_lock(&g_mu);
    int64_t n = c->count;
    pthread_mutex_unlock(&g_mu);
    return n;
}

/* Take a value if one is available (g_mu held). */
static bool try_take(AlxChan *c, void *out) {
    if (c->count > 0) {
        memcpy(out, c->buf + (size_t)c->head * c->esz, c->esz);
        c->head = (c->head + 1) % c->cap;
        c->count--;
        return true;
    }
    if (c->slot_full) {
        memcpy(out, c->slot, c->esz);
        c->slot_full = false;
        c->taken_seq = c->send_seq;
        return true;
    }
    return false;
}

/* Buffered: push if there is room. Unbuffered: place into the slot if it is
 * free and (when `need_waiter`) a receiver is waiting (g_mu held). */
static bool try_put(AlxChan *c, const void *val, bool need_waiter) {
    if (c->cap > 0) {
        if (c->count == c->cap) return false;
        memcpy(c->buf + (size_t)((c->head + c->count) % c->cap) * c->esz, val, c->esz);
        c->count++;
        return true;
    }
    if (c->slot_full || (need_waiter && c->recv_waiting == 0)) return false;
    memcpy(c->slot, val, c->esz);
    c->slot_full = true;
    c->send_seq++;
    return true;
}

void alx_chan_send(AlxChan *c, const void *val, const char *loc) {
    pthread_mutex_lock(&g_mu);
    while (!c->closed) {
        if (try_put(c, val, false)) {
            wake(&c->w);
            if (c->cap > 0) {
                pthread_mutex_unlock(&g_mu);
                return;
            }
            /* Rendezvous: wait until a receiver took it. */
            uint64_t ticket = c->send_seq;
            while (c->taken_seq < ticket && !c->closed)
                if (!block_wait1(&c->w)) deadlock();
            if (c->taken_seq >= ticket) {
                pthread_mutex_unlock(&g_mu);
                return;
            }
            break; /* closed while waiting: close dropped the value */
        }
        if (!block_wait1(&c->w)) deadlock();
    }
    pthread_mutex_unlock(&g_mu);
    alx_panic("send on a closed channel", loc);
}

bool alx_chan_recv(AlxChan *c, void *out) {
    pthread_mutex_lock(&g_mu);
    for (;;) {
        if (try_take(c, out)) {
            wake(&c->w);
            pthread_mutex_unlock(&g_mu);
            return true;
        }
        if (c->closed) {
            pthread_mutex_unlock(&g_mu);
            return false;
        }
        c->recv_waiting++;
        if (c->send_waiting) wake(&c->w); /* a blocked select-sender may now proceed */
        bool ok = block_wait1(&c->w);
        c->recv_waiting--;
        if (!ok) deadlock();
    }
}

void alx_chan_close(AlxChan *c, const char *loc) {
    pthread_mutex_lock(&g_mu);
    if (c->closed) {
        pthread_mutex_unlock(&g_mu);
        alx_panic("close of a closed channel", loc);
    }
    c->closed = true;
    c->slot_full = false; /* a blocked sender's value is dropped; it panics */
    wake(&c->w);
    pthread_mutex_unlock(&g_mu);
}

static _Thread_local uint64_t tl_rng;

static uint64_t rnd(void) {
    if (!tl_rng) tl_rng = (((uint64_t)(uintptr_t)&tl_rng * 0x9E3779B97F4A7C15ull) ^ (uint64_t)clock() ^ ((uint64_t)(uintptr_t)pthread_self() << 17)) | 1;
    tl_rng ^= tl_rng << 13;
    tl_rng ^= tl_rng >> 7;
    tl_rng ^= tl_rng << 17;
    return tl_rng;
}

int64_t alx_select(AlxSelCase *cs, int64_t n, bool has_default, const char *loc) {
    int64_t order[n ? n : 1];
    pthread_mutex_lock(&g_mu);
    for (;;) {
        for (int64_t i = 0; i < n; i++) order[i] = i;
        for (int64_t i = n - 1; i > 0; i--) {
            int64_t j = (int64_t)(rnd() % (uint64_t)(i + 1)), t = order[i];
            order[i] = order[j];
            order[j] = t;
        }
        for (int64_t k = 0; k < n; k++) {
            int64_t i = order[k];
            AlxSelCase *s = &cs[i];
            if (s->is_send) {
                if (s->ch->closed) {
                    pthread_mutex_unlock(&g_mu);
                    alx_panic("send on a closed channel", loc);
                }
                if (try_put(s->ch, s->buf, true)) {
                    wake(&s->ch->w);
                    pthread_mutex_unlock(&g_mu);
                    return i;
                }
            } else if (try_take(s->ch, s->buf)) {
                s->ok = 1;
                wake(&s->ch->w);
                pthread_mutex_unlock(&g_mu);
                return i;
            } else if (s->ch->closed) {
                s->ok = 0;
                pthread_mutex_unlock(&g_mu);
                return i;
            }
        }
        if (has_default) {
            pthread_mutex_unlock(&g_mu);
            return n;
        }
        WNode **hs[n ? n : 1];
        for (int64_t i = 0; i < n; i++) {
            hs[i] = &cs[i].ch->w;
            if (cs[i].is_send) {
                cs[i].ch->send_waiting++;
            } else {
                cs[i].ch->recv_waiting++;
                if (cs[i].ch->send_waiting > 0) wake(&cs[i].ch->w);
            }
        }
        bool ok = block_wait(hs, (int)n);
        for (int64_t i = 0; i < n; i++) {
            if (cs[i].is_send) cs[i].ch->send_waiting--;
            else cs[i].ch->recv_waiting--;
        }
        if (!ok) deadlock();
    }
}

/* ---------- locks and atomics (R6) ---------- */

/* A lock is a flag under the scheduler's lock with its own wait list: a
 * holder parks takers the way a full channel parks senders, so deadlock
 * detection covers it. */
struct AlxLock { bool held; WNode *w; /* parked takers */ };

AlxLock *alx_lock_new(void) {
    AlxLock *l = calloc(1, sizeof *l);
    if (!l) alx_panic("out of memory", "runtime");
    return l;
}

void alx_lock(AlxLock *l) {
    pthread_mutex_lock(&g_mu);
    while (l->held)
        if (!block_wait1(&l->w)) deadlock();
    l->held = true;
    pthread_mutex_unlock(&g_mu);
}

void alx_unlock(AlxLock *l) {
    pthread_mutex_lock(&g_mu);
    l->held = false;
    wake(&l->w);
    pthread_mutex_unlock(&g_mu);
}

int64_t *alx_atomic_new(int64_t v) {
    int64_t *a = malloc(sizeof *a);
    if (!a) alx_panic("out of memory", "runtime");
    __atomic_store_n(a, v, __ATOMIC_SEQ_CST);
    return a;
}

/* ---------- the program's arguments (os.args) ---------- */

static int g_argc;
static char **g_argv;
void alx_set_args(int argc, char **argv) { g_argc = argc; g_argv = argv; }
int64_t alx_argc(void) { return g_argc; }
/* NULL past the end. */
const char *alx_argv(int64_t i) { return i >= 0 && i < g_argc ? g_argv[i] : NULL; }

/* ---------- sleeping (time.sleep) ---------- */

/* A task asleep parks on a deadline-ordered list; one timer thread wakes
 * each at its deadline. A plain thread (main) just sleeps. */
typedef struct Timer { int64_t at; Parker *p; struct Timer *next; } Timer;
static Timer *g_timers;
static pthread_cond_t g_tcv = PTHREAD_COND_INITIALIZER;
static bool g_timer_thread;

static int64_t mono_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

static void *timer_main(void *arg) {
    (void)arg;
    tl_uncounted = true;
    pthread_mutex_lock(&g_mu);
    for (;;) {
        while (!g_timers) pthread_cond_wait(&g_tcv, &g_mu);
        int64_t now = mono_ns();
        if (g_timers->at > now) {
            /* Wait until the earliest deadline (or an earlier one arrives). */
            struct timespec rt;
            clock_gettime(CLOCK_REALTIME, &rt);
            int64_t until = (int64_t)rt.tv_sec * 1000000000 + rt.tv_nsec + (g_timers->at - now);
            struct timespec dl = { (time_t)(until / 1000000000), (long)(until % 1000000000) };
            pthread_cond_timedwait(&g_tcv, &g_mu, &dl);
            continue;
        }
        Timer *t = g_timers;
        g_timers = t->next;
        g_sleepers--;
        unpark(t->p);
        free(t);
    }
    return NULL;
}

void alx_sleep_ns(int64_t ns) {
    if (ns <= 0) return;
    if (!tl_task) {
        struct timespec ts = { (time_t)(ns / 1000000000), (long)(ns % 1000000000) };
        while (nanosleep(&ts, &ts) != 0 && errno == EINTR) {}
        return;
    }
    Parker *p = cur_pk();
    Timer *t = malloc(sizeof *t);
    if (!t) alx_panic("out of memory", "runtime");
    t->at = mono_ns() + ns;
    t->p = p;
    pthread_mutex_lock(&g_mu);
    if (!g_timer_thread) {
        pthread_t th;
        pthread_attr_t at;
        pthread_attr_init(&at);
        pthread_attr_setdetachstate(&at, PTHREAD_CREATE_DETACHED);
        if (pthread_create(&th, &at, timer_main, NULL) != 0) alx_panic("cannot start the timer thread", "runtime");
        pthread_attr_destroy(&at);
        g_timer_thread = true;
    }
    Timer **pp = &g_timers;
    while (*pp && (*pp)->at <= t->at) pp = &(*pp)->next;
    t->next = *pp;
    *pp = t;
    if (g_timers == t) pthread_cond_signal(&g_tcv);
    g_sleepers++;
    p->parked = true; p->dead = false;
    if (p->counted) g_runnable--;
    pthread_mutex_unlock(&g_mu);
    sw_out(p->task, false);
}

/* ---------- clocks (time) ---------- */

/* Wall clock: nanoseconds since 1970-01-01 UTC. */
int64_t alx_wall_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

/* Monotonic clock: nanoseconds from an arbitrary start (for durations). */
int64_t alx_mono_ns(void) { return mono_ns(); }

/* The local time zone's offset from UTC, in seconds, at this Unix time. */
int64_t alx_local_offset(int64_t unix_sec) {
    time_t t = (time_t)unix_sec;
    struct tm tm;
    if (!localtime_r(&t, &tm)) return 0;
    return (int64_t)tm.tm_gmtoff;
}

/* The local zone's abbreviation at this Unix time ("CET", "PDT"). */
const char *alx_local_zone(int64_t unix_sec) {
    time_t t = (time_t)unix_sec;
    struct tm tm;
    if (!localtime_r(&t, &tm) || !tm.tm_zone) return "UTC";
    return tm.tm_zone;
}
