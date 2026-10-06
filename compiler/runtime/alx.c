/* SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception */
#include "alx.h"

#include <assert.h>
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <fcntl.h>
#include <setjmp.h>
#include <signal.h>
#include <sys/stat.h>
#include <dirent.h>
#include <time.h>
#include <sys/mman.h>
#include <unistd.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <stddef.h>

/* ---------- memory ---------- */

size_t alx_allocs;
bool alx_counting;
_Thread_local struct AlxTls alx_tls;
static size_t mem_held, mem_peak;
static bool alx_memstats;

size_t alx_mem_held(void) { return __atomic_load_n(&mem_held, __ATOMIC_RELAXED); }
size_t alx_mem_peak(void) { return __atomic_load_n(&mem_peak, __ATOMIC_RELAXED); }
/* testing.allocs_per_run: count allocations from now on (returns whether
 * counting was on) and read the count. */
bool alx_count_allocs(bool on) { bool was = alx_counting; alx_counting = on || getenv("ALX_COUNT_ALLOCS") != NULL; return was; }
int64_t alx_alloc_count(void) { return (int64_t)__atomic_load_n(&alx_allocs, __ATOMIC_RELAXED); }

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
typedef AlxBlk Blk;   /* struct AlxRegion and AlxBlk: alx.h */

#define tl_prog alx_tl_prog
#define tl_cur alx_tl_cur
#define tl_pool alx_tl_pool
static _Thread_local Blk *tl_free;             /* free chunks */
/* Pooled region structs that kept their only chunk (a frame that allocated a
 * little): reusing one costs no registry or free-list work. */
#define tl_ncached alx_tl_ncached
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
    alx_tls.last_g = 0; /* alx_region_of's cached answer may be stale */
    for (uintptr_t g = g0; g <= g1; g++) { if (r) reg_put(g, r); else reg_del(g); }
}

AlxRegion *alx_region_of_slow(const void *p) {
    if (p && tl_reg_n) {
        uintptr_t g = (uintptr_t)p >> 20;
        size_t m = tl_reg_cap - 1, i = reg_hash(g, m);
        for (RegEnt *e; (e = &tl_reg[i])->g; i = (i + 1) & m)
            if (e->g == g) {
                alx_tls.last_g = g;
                alx_tls.last_r = e->r;
                return e->r;
            }
    }
    return &tl_prog;
}

static inline AlxRegion *cur_region(void) { return tl_cur ? tl_cur : &tl_prog; }

static AlxRegion *region_alloc(void) {
    AlxRegion *r = tl_pool;
    if (r) tl_pool = r->pool_next;
    else {
        r = malloc(sizeof *r);
        if (!r) alx_panic("out of memory", "runtime");
        r->chunks = NULL;
    }
    if (r->chunks) {
        /* It kept its chunk (still registered to it): start it over. */
        tl_ncached--;
        r->cbase = (char *)r->chunks + ALX_HDR;
        r->cur = r->cbase; r->end = (char *)r->chunks + ALX_CHUNK;
    } else {
        r->cur = r->end = NULL; r->cbase = NULL;
    }
    r->larges = NULL; r->pool_next = NULL; r->counted = 0;
    r->parent = r->child = r->sib = r->sib_prev = NULL;
    return r;
}

AlxRegion *alx_region_enter_slow(void) {
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
    /* One chunk, nothing else: keep it with the struct for the next region. */
    if (r->chunks && !r->chunks->next && !r->larges && tl_ncached < ALX_CACHED_CAP) {
        tl_ncached++;
        r->pool_next = tl_pool; tl_pool = r;
        return;
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

/* A light frame (a call that allocates only for itself): instead of a region
 * of its own, a mark in the current one (its bump position and its newest
 * large block), rolled back on the way out. The common cases are inline in
 * alx.h; these handle the rest. */

/* In the program region (shared by this thread's tasks: one parked inside the
 * call could see another allocate above the mark), a real region instead,
 * tagged in the low bit. */
AlxRegion *alx_region_mark_slow(void) {
    return (AlxRegion *)((uintptr_t)alx_region_enter() | 1);
}

void alx_region_reset_slow(AlxRegion *mark, void *larges) {
    if ((uintptr_t)mark & 1) {
        alx_region_exit((AlxRegion *)((uintptr_t)mark & ~(uintptr_t)1), alx_region_program());
        return;
    }
    AlxRegion *c = cur_region();
    char *m = (char *)mark;
    /* Chunks added since the mark (those that don't hold it) go, except the
     * first of them, kept empty for the next call. */
    Blk *keep = NULL;
    while (c->chunks && !(m >= (char *)c->chunks && m <= (char *)c->chunks + ALX_CHUNK)) {
        Blk *b = c->chunks;
        c->chunks = b->next;
        if (keep) {
            reg_blk(keep, ALX_CHUNK, NULL);
            if (tl_nfree < ALX_FREE_CAP) { if (!tl_free_armed) arm_free(); keep->next = tl_free; tl_free = keep; tl_nfree++; }
            else { mem_sub(keep->size); free(keep); }
        }
        keep = b;
    }
    char *cur = m, *end = c->chunks ? (char *)c->chunks + ALX_CHUNK : NULL;
    if (keep) {
        keep->next = c->chunks;
        c->chunks = keep;
        if (c->cbase && m) c->counted += (size_t)(m - c->cbase);
        c->cbase = (char *)keep + ALX_HDR;
        cur = c->cbase;
        end = (char *)keep + ALX_CHUNK;
    }
    while ((void *)c->larges != larges && c->larges) {
        Blk *b = c->larges;
        c->larges = b->next;
        reg_blk(b, b->size, NULL); mem_sub(b->size); free(b);
    }
    alx_bump_cur = cur; alx_bump_end = end;
}

void alx_region_exit_slow(AlxRegion *r, AlxRegion *saved) {
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
 * control longjmps back to the task's entry (see task_entry), which releases
 * and poisons the locks the task holds. In a `pmap` element (on any thread)
 * a panic stops the job, and the caller of pmap re-raises it once every
 * thread is done (see pmap_run). Everywhere else (the main thread) a panic
 * prints and aborts. */
static _Thread_local AlxTask *tl_task;
static _Noreturn void task_panic(char *msg);

/* A pmap element running on this thread (or task): where its panic goes,
 * and the newest lock held when it started (the element's locks are above). */
typedef struct PmapCtx { jmp_buf jb; char *msg; AlxLock *mark; } PmapCtx;
static PmapCtx **pm_slot(void);
static bool panic_caught(void) { return tl_task || *pm_slot(); }

/* m: a malloc'd message, or NULL (out of memory). */
static _Noreturn void panic_catch(char *m) {
    PmapCtx *p = *pm_slot();
    if (p) {
        p->msg = m ? m : (char *)"alexandrite: out of memory";
        _longjmp(p->jb, 1);
    }
    task_panic(m);
}

/* Raise a whole panic message (with its `alexandrite: ` prefix) again. */
static _Noreturn void panic_full(char *m) {
    if (panic_caught()) panic_catch(m);
    fflush(stdout);
    fprintf(stderr, "%s\n", m);
    abort();
}

void alx_panic(const char *what, const char *loc) {
    if (panic_caught()) {
        size_t n = (size_t)snprintf(NULL, 0, "alexandrite: %s at %s", what, loc) + 1;
        char *m = malloc(n);
        if (m) snprintf(m, n, "alexandrite: %s at %s", what, loc);
        panic_catch(m);
    }
    fflush(stdout);
    fprintf(stderr, "alexandrite: %s at %s\n", what, loc);
    abort();
}

void alx_panic_str(AlxStr msg) {
    if (panic_caught()) {
        size_t n = (size_t)msg.len + 14;
        char *m = malloc(n);
        if (m) snprintf(m, n, "alexandrite: %.*s", (int)msg.len, msg.ptr);
        panic_catch(m);
    }
    fflush(stdout);
    fprintf(stderr, "alexandrite: %.*s\n", (int)msg.len, msg.ptr);
    abort();
}

void alx_overflow(const char *loc) {
    static const char hint[] = "hint: add `#![overflow(promote)]` to this file to promote to bignums";
    if (panic_caught()) {
        size_t n = (size_t)snprintf(NULL, 0, "alexandrite: overflow at %s\n%s", loc, hint) + 1;
        char *m = malloc(n);
        if (m) snprintf(m, n, "alexandrite: overflow at %s\n%s", loc, hint);
        panic_catch(m);
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

bool alx_str_eq_slow(AlxStr a, AlxStr b) { return a.len == b.len && (a.len == 0 || memcmp(a.ptr, b.ptr, (size_t)a.len) == 0); }

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
    char c0 = sep.ptr[0];
    for (int64_t i = 0; i + sep.len <= s.len;) {
        /* The next place the separator's first byte occurs. */
        const char *hit = memchr(s.ptr + i, c0, (size_t)(s.len - sep.len + 1 - i));
        if (!hit) break;
        i = hit - s.ptr;
        if (sep.len == 1 || memcmp(s.ptr + i + 1, sep.ptr + 1, (size_t)sep.len - 1) == 0) {
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

AlxStr alx_str_join(Arr_Str a, AlxStr sep) {
    int64_t len = 0;
    for (int64_t i = 0; i < a.len; i++) len += a.ptr[i].len + (i > 0 ? sep.len : 0);
    char *p = alx_alloc((size_t)(len ? len : 1));
    int64_t o = 0;
    for (int64_t i = 0; i < a.len; i++) {
        if (i > 0 && sep.len) { memcpy(p + o, sep.ptr, (size_t)sep.len); o += sep.len; }
        if (a.ptr[i].len) { memcpy(p + o, a.ptr[i].ptr, (size_t)a.ptr[i].len); o += a.ptr[i].len; }
    }
    AlxStr r = { p, len };
    return r;
}

/* s.byteindex(sub, from): the first offset >= from (clamped to 0) where sub
 * occurs, or -1. memchr finds candidates for the first byte. */
int64_t alx_str_index(AlxStr s, AlxStr sub, int64_t from) {
    if (from < 0) from = 0;
    if (from > s.len) return -1;
    if (sub.len == 0) return from;
    char c0 = sub.ptr[0];
    for (int64_t i = from; i + sub.len <= s.len;) {
        const char *hit = memchr(s.ptr + i, c0, (size_t)(s.len - sub.len + 1 - i));
        if (!hit) return -1;
        i = hit - s.ptr;
        if (sub.len == 1 || memcmp(s.ptr + i + 1, sub.ptr + 1, (size_t)sub.len - 1) == 0) return i;
        i++;
    }
    return -1;
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

/* The length of the UTF-8 sequence at i: 1 for ASCII and for anything
 * invalid by Go's rules (bad lead or continuation bytes, overlong forms,
 * surrogates, past U+10FFFF, truncated), which decodes as U+FFFD. */
int64_t alx_str_charlen(AlxStr s, int64_t i) {
    const unsigned char *p = (const unsigned char *)s.ptr + i;
    int64_t left = s.len - i;
    unsigned char c = p[0];
    if (c < 0x80) return 1;
    int64_t n;
    unsigned char lo = 0x80, hi = 0xBF;
    if (c >= 0xC2 && c <= 0xDF) n = 2;
    else if (c >= 0xE0 && c <= 0xEF) { n = 3; if (c == 0xE0) lo = 0xA0; if (c == 0xED) hi = 0x9F; }
    else if (c >= 0xF0 && c <= 0xF4) { n = 4; if (c == 0xF0) lo = 0x90; if (c == 0xF4) hi = 0x8F; }
    else return 1;
    if (left < n) return 1;
    if (p[1] < lo || p[1] > hi) return 1;
    for (int64_t k = 2; k < n; k++)
        if (p[k] < 0x80 || p[k] > 0xBF) return 1;
    return n;
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

/* The end of the last string alx_str_cat made. When the first part is that
 * string and nothing was allocated after it (it ends at the bump pointer),
 * the rest is appended in place: `s += x` in a loop is linear. Only the most
 * recent concatenation qualifies, so no other string can be using the bytes
 * past its end. */
#define tl_last_cat alx_last_cat

AlxStr alx_str_cat(int64_t n, const AlxStr *parts) {
    int64_t len = 0;
    for (int64_t i = 0; i < n; i++) len += parts[i].len;
    const char *f = parts[0].ptr;
    if (n >= 2 && f && parts[0].len > 0 && f + parts[0].len == tl_last_cat) {
        char *top = alx_bump_cur;
        size_t had = ((size_t)parts[0].len + 15) & ~(size_t)15, need = ((size_t)len + 15) & ~(size_t)15;
        if ((char *)f + had == top && (size_t)(alx_bump_end - (char *)f) >= need) {
            alx_bump_cur = (char *)f + need;
            int64_t o = parts[0].len;
            for (int64_t i = 1; i < n; i++) {
                int64_t m = parts[i].len;
                if (m <= 16) {
                    for (int64_t k = 0; k < m; k++) ((char *)f)[o + k] = parts[i].ptr[k];
                } else {
                    memmove((char *)f + o, parts[i].ptr, (size_t)m);
                }
                o += m;
            }
            tl_last_cat = f + len;
            AlxStr s = { f, len };
            return s;
        }
    }
    char *p = alx_alloc((size_t)len);
    int64_t o = 0;
    for (int64_t i = 0; i < n; i++) {
        int64_t m = parts[i].len;
        if (m <= 16) {
            for (int64_t k = 0; k < m; k++) p[o + k] = parts[i].ptr[k];
        } else {
            memcpy(p + o, parts[i].ptr, (size_t)m);
        }
        o += m;
    }
    tl_last_cat = p + len;
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

/* ---------- parallel map ----------
 * Threads claim blocks of BLK items from a shared counter, so uneven work
 * (and slower efficiency cores) doesn't leave one thread with the tail. */

typedef struct {
    const char *in;
    char *out;
    int64_t n, blk;
    int64_t next;
    size_t in_size, out_size;
    AlxWorker fn;
    size_t res_size, val_off; /* fallible workers (alx_pmap_try) */
    char *err;
    int64_t err_i;
    pthread_mutex_t err_mu;
    char *panic;              /* the first element panic, raised again by the caller */
} PmapJob;

static _Thread_local bool tl_uncounted;
static AlxLock **held_head(void);
static void locks_poison(AlxLock **head, AlxLock *mark);

static void pmap_loop(PmapJob *j) {
    for (;;) {
        int64_t lo = __atomic_fetch_add(&j->next, j->blk, __ATOMIC_RELAXED);
        if (lo >= j->n) break;
        int64_t hi = lo + j->blk > j->n ? j->n : lo + j->blk;
        if (!j->err) {
            for (int64_t i = lo; i < hi; i++) j->fn(j->in + (size_t)i * j->in_size, j->out + (size_t)i * j->out_size);
            continue;
        }
        _Alignas(16) char res[j->res_size];
        for (int64_t i = lo; i < hi; i++) {
            j->fn(j->in + (size_t)i * j->in_size, res);
            if (*(bool *)res) {
                memcpy(j->out + (size_t)i * j->out_size, res + j->val_off, j->out_size);
            } else {
                pthread_mutex_lock(&j->err_mu);
                if (i < j->err_i) {
                    j->err_i = i;
                    memcpy(j->err, res, j->res_size);
                }
                pthread_mutex_unlock(&j->err_mu);
            }
        }
    }
}

/* A panic in an element (on this thread or task) lands here: the element's
 * locks are released and poisoned, no more blocks are claimed, and the
 * first message is kept for the caller of pmap, which raises it again. */
static void *pmap_run(void *arg) {
    PmapJob *j = arg;
    PmapCtx ctx = { .msg = NULL };
    PmapCtx *prev = *pm_slot();
    ctx.mark = *held_head();
    *pm_slot() = &ctx;
    if (_setjmp(ctx.jb) == 0) {
        pmap_loop(j);
    } else {
        locks_poison(held_head(), ctx.mark);
        __atomic_store_n(&j->next, j->n, __ATOMIC_RELAXED);
        pthread_mutex_lock(&j->err_mu);
        if (!j->panic) j->panic = ctx.msg;
        pthread_mutex_unlock(&j->err_mu);
    }
    *pm_slot() = prev;
    return NULL;
}

static void *pmap_thread(void *arg) {
    tl_uncounted = true; /* not a task: ignored by deadlock detection */
    return pmap_run(arg);
}

static void pmap_go(PmapJob *job) {
    long cpus = sysconf(_SC_NPROCESSORS_ONLN);
    const char *forced = getenv("ALX_THREADS");
    if (forced && atol(forced) > 0) cpus = atol(forced);
    int64_t n = job->n, threads = cpus > 0 ? cpus : 4;
    if (threads > 64) threads = 64;
    if (n < threads * 64) threads = n / 64 + 1;
    pthread_t tids[64];
    job->blk = n / (threads * 16);
    if (job->blk < 1) job->blk = 1;
    pthread_mutex_init(&job->err_mu, NULL);
    for (int64_t t = 1; t < threads; t++) pthread_create(&tids[t], NULL, pmap_thread, job);
    pmap_run(job);
    for (int64_t t = 1; t < threads; t++) pthread_join(tids[t], NULL);
    pthread_mutex_destroy(&job->err_mu);
    if (job->panic) panic_full(job->panic);
}

void alx_pmap(const void *in, int64_t n, size_t in_size, void *out, size_t out_size, AlxWorker fn) {
    PmapJob job = { .in = in, .out = out, .n = n, .in_size = in_size, .out_size = out_size, .fn = fn };
    pmap_go(&job);
}

void alx_pmap_try(const void *in, int64_t n, size_t in_size, void *out, size_t val_size, AlxWorker fn,
                  size_t res_size, size_t val_off, void *err) {
    PmapJob job = { .in = in, .out = out, .n = n, .in_size = in_size, .out_size = val_size, .fn = fn,
                    .res_size = res_size, .val_off = val_off, .err = err, .err_i = INT64_MAX };
    memset(err, 0, res_size);
    *(bool *)err = true;
    pmap_go(&job);
}

/* ---------- tasks and channels ----------
 * Tasks are stackful coroutines (hand-written context switch) multiplexed
 * over N persistent worker threads. Blocking operations park the task; the
 * worker goes on to run another one.
 *
 * Stacks: every worker owns one mmap'd run stack (guard page below, size
 * ALX_TASK_STACK, default 8 MiB of address space, committed only as it is
 * touched) that all its tasks run on. When a task
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
    AlxLock *held;               /* the locks it holds, newest first (through hnext) */
    PmapCtx *pm;                 /* the pmap element it is running, if any */
};

/* Outside tasks (main, pmap helper threads) the same state is per thread.
 * noinline: a task can move to another thread across a blocking call, so the
 * thread-local address must be looked up again each time. */
static _Thread_local AlxLock *tl_held;
static _Thread_local PmapCtx *tl_pm;
static __attribute__((noinline)) AlxLock **held_head(void) { return tl_task ? &tl_task->held : &tl_held; }
static __attribute__((noinline)) PmapCtx **pm_slot(void) { return tl_task ? &tl_task->pm : &tl_pm; }

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

/* Let the other tasks queued on this worker run before going on (a task
 * that never blocks would otherwise starve them: tasks are pinned). Outside
 * a task: give the CPU away once. */
static void sw_out(AlxTask *t, bool dying);
static AlxTask *tq_pop(TQ *q);
void alx_task_yield(void) {
    AlxTask *t = tl_task;
    if (!t) {
        sched_yield();
        return;
    }
    pthread_mutex_lock(&g_mu);
    if (!t->home->q.head) {
        /* Nothing queued here: take a task that hasn't started yet, if any
         * (the worker runs its own queue first). */
        AlxTask *n = tq_pop(&g_newq);
        if (!n) {
            pthread_mutex_unlock(&g_mu);
            return;
        }
        n->home = t->home;
        tq_push(&t->home->q, n);
    }
    tq_push(&t->home->q, t);
    pthread_mutex_unlock(&g_mu);
    sw_out(t, false);
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

/* After a panic: release the locks in *head above `mark` (newest first) and
 * poison them, since what they guard may be half-updated. */
static void locks_poison(AlxLock **head, AlxLock *mark);

/* ---- stacks ---- */

static size_t parse_size(const char *s, size_t dflt) {
    char *e;
    double v = strtod(s, &e);
    if (e == s || v <= 0) return dflt;
    if (*e == 'k' || *e == 'K') v *= 1024;
    else if (*e == 'm' || *e == 'M') v *= 1024 * 1024;
    else if (*e == 'g' || *e == 'G') v *= 1024.0 * 1024 * 1024;
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
    else {
        t->panicked = true;
        t->pm = NULL;
        locks_poison(&t->held, NULL);
    }
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
    size_t dflt = 8 << 20;       /* reserved, committed as touched: deep recursion (regexp trees nest 2000 deep) */
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
            /* A loop polling with `else` never parks: now and then, let the
             * tasks waiting for this worker (and the lock) run. */
            static _Thread_local unsigned polls;
            if ((++polls & 15) == 0) alx_task_yield();
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
/* Each holder (task, or thread outside tasks) keeps the locks it holds on
 * an intrusive stack through `hnext` (lock blocks nest, so it's LIFO): a
 * panic releases them and marks them poisoned. Taking a poisoned lock
 * panics until `clear_poison!`. */
struct AlxLock {
    bool held, poisoned;
    WNode *w;                    /* parked takers */
    AlxLock *hnext;              /* the holder's next older lock */
};

AlxLock *alx_lock_new(void) {
    AlxLock *l = calloc(1, sizeof *l);
    if (!l) alx_panic("out of memory", "runtime");
    return l;
}

void alx_lock(AlxLock *l, const char *loc) {
    pthread_mutex_lock(&g_mu);
    while (l->held)
        if (!block_wait1(&l->w)) deadlock();
    if (l->poisoned) {
        pthread_mutex_unlock(&g_mu);
        alx_panic("Mutex poisoned: a task panicked while holding it", loc);
    }
    l->held = true;
    AlxLock **h = held_head();
    l->hnext = *h;
    *h = l;
    pthread_mutex_unlock(&g_mu);
}

void alx_unlock(AlxLock *l) {
    pthread_mutex_lock(&g_mu);
    AlxLock **h = held_head();
    while (*h && *h != l) h = &(*h)->hnext;
    if (*h) *h = l->hnext;
    l->hnext = NULL;
    l->held = false;
    wake(&l->w);
    pthread_mutex_unlock(&g_mu);
}

static void locks_poison(AlxLock **head, AlxLock *mark) {
    if (*head == mark) return;
    pthread_mutex_lock(&g_mu);
    while (*head && *head != mark) {
        AlxLock *l = *head;
        *head = l->hnext;
        l->hnext = NULL;
        l->held = false;
        l->poisoned = true;
        wake(&l->w);
    }
    pthread_mutex_unlock(&g_mu);
}

bool alx_lock_poisoned(AlxLock *l) {
    pthread_mutex_lock(&g_mu);
    bool p = l->poisoned;
    pthread_mutex_unlock(&g_mu);
    return p;
}

void alx_lock_clear_poison(AlxLock *l) {
    pthread_mutex_lock(&g_mu);
    l->poisoned = false;
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
/* keep: an fd wait's timer (alx_fd_wait_until); the timer thread marks it
 * fired instead of freeing it, and the waiter frees it. */
typedef struct Timer { int64_t at; Parker *p; struct Timer *next; bool keep, fired; } Timer;
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
            /* At most a day at a time: a far deadline (a huge sleep) would
             * overflow the absolute time, and an overflowed one returns at
             * once, spinning this thread. */
            int64_t wait = g_timers->at - now;
            if (wait > 86400 * (int64_t)1000000000) wait = 86400 * (int64_t)1000000000;
            int64_t until = (int64_t)rt.tv_sec * 1000000000 + rt.tv_nsec + wait;
            struct timespec dl = { (time_t)(until / 1000000000), (long)(until % 1000000000) };
            pthread_cond_timedwait(&g_tcv, &g_mu, &dl);
            continue;
        }
        Timer *t = g_timers;
        g_timers = t->next;
        g_sleepers--;
        unpark(t->p);
        if (t->keep) t->fired = true;
        else free(t);
    }
    return NULL;
}

static void timer_add(Timer *t);

void alx_sleep_ns(int64_t ns) {
    if (ns <= 0) return;
    if (!tl_task) {
        struct timespec ts = { (time_t)(ns / 1000000000), (long)(ns % 1000000000) };
        while (nanosleep(&ts, &ts) != 0 && errno == EINTR) {}
        return;
    }
    Parker *p = cur_pk();
    Timer *t = calloc(1, sizeof *t);
    if (!t) alx_panic("out of memory", "runtime");
    int64_t now = mono_ns();
    t->at = ns > INT64_MAX - now ? INT64_MAX : now + ns;
    t->p = p;
    pthread_mutex_lock(&g_mu);
    timer_add(t);
    p->parked = true; p->dead = false;
    if (p->counted) g_runnable--;
    pthread_mutex_unlock(&g_mu);
    sw_out(p->task, false);
}

/* Queue t on the timer list (g_mu held); it counts as a sleeper. */
static void timer_add(Timer *t) {
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
}

/* Take t off the timer list if it's still there (g_mu held). */
static void timer_cancel(Timer *t) {
    for (Timer **pp = &g_timers; *pp; pp = &(*pp)->next)
        if (*pp == t) { *pp = t->next; g_sleepers--; return; }
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

/* ---------- C foreign functions (extern def) ---------- */

_Thread_local int alx_ffi_errno_;

void alx_ffi_enter(void) {
    fflush(stdout);
    errno = 0; /* so C.errno is 0 after a call that set nothing */
}

char *alx_cstr_new(AlxStr s) {
    char *p = malloc((size_t)s.len + 1);
    if (!p) alx_panic("out of memory", "extern call");
    if (s.len) memcpy(p, s.ptr, (size_t)s.len);
    p[s.len] = 0;
    return p;
}

AlxStr alx_str_from_cstr(const char *p) {
    if (!p) { AlxStr e = { "", 0 }; return e; }
    return alx_str_from_bytes((const uint8_t *)p, (int64_t)strlen(p));
}

AlxStr alx_str_from_ptr(const char *p, int64_t n) {
    if (!p || n <= 0) { AlxStr e = { "", 0 }; return e; }
    return alx_str_from_bytes((const uint8_t *)p, n);
}

AlxStr alx_strerror(int64_t n) {
    static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER; /* strerror isn't thread-safe */
    pthread_mutex_lock(&mu);
    AlxStr s = alx_str_from_cstr(strerror((int)n));
    pthread_mutex_unlock(&mu);
    return s;
}

int32_t alx_sys_open(const char *path, int32_t flags, int32_t mode) { return open(path, flags, (mode_t)mode); }
int32_t alx_sys_fcntl(int32_t fd, int32_t cmd, int64_t arg) { return fcntl(fd, cmd, (long)arg); }

typedef struct { const char *name; int64_t val; } SysConst;
#define SC(x) { #x, (int64_t)(x) },
static const SysConst sys_consts[] = {
    SC(O_RDONLY) SC(O_WRONLY) SC(O_RDWR) SC(O_CREAT) SC(O_EXCL) SC(O_TRUNC) SC(O_APPEND) SC(O_NONBLOCK) SC(O_CLOEXEC)
#ifdef O_DIRECTORY
    SC(O_DIRECTORY)
#endif
#ifdef O_SYNC
    SC(O_SYNC)
#endif
    SC(SEEK_SET) SC(SEEK_CUR) SC(SEEK_END)
    SC(F_GETFD) SC(F_SETFD) SC(F_GETFL) SC(F_SETFL) SC(FD_CLOEXEC) SC(F_DUPFD_CLOEXEC) SC(SIGPIPE) SC(SIGBUS)
    SC(EPERM) SC(ENOENT) SC(ESRCH) SC(EINTR) SC(EIO) SC(ENXIO) SC(E2BIG) SC(ENOEXEC) SC(EBADF) SC(ECHILD)
    SC(EAGAIN) SC(ENOMEM) SC(EACCES) SC(EFAULT) SC(EBUSY) SC(EEXIST) SC(EXDEV) SC(ENODEV) SC(ENOTDIR)
    SC(EISDIR) SC(EINVAL) SC(ENFILE) SC(EMFILE) SC(ENOTTY) SC(EFBIG) SC(ENOSPC) SC(ESPIPE) SC(EROFS)
    SC(EMLINK) SC(EPIPE) SC(EDOM) SC(ERANGE) SC(EWOULDBLOCK) SC(ENAMETOOLONG) SC(ENOSYS) SC(ENOTEMPTY)
    SC(ELOOP) SC(ETIMEDOUT) SC(ECONNREFUSED) SC(ECONNRESET) SC(EADDRINUSE)
    SC(S_IFMT) SC(S_IFREG) SC(S_IFDIR) SC(S_IFLNK) SC(S_IFIFO) SC(S_IFCHR) SC(S_IFBLK) SC(S_IFSOCK)
    SC(S_IRWXU) SC(S_IRUSR) SC(S_IWUSR) SC(S_IXUSR) SC(S_IRWXG) SC(S_IRWXO)
    SC(CLOCK_REALTIME) SC(CLOCK_MONOTONIC)
    SC(O_NOFOLLOW) SC(EINPROGRESS) SC(ENOTSUP) SC(EOVERFLOW) SC(ETXTBSY) SC(EDQUOT) SC(ESTALE) SC(ENOBUFS)
    SC(ECONNABORTED) SC(ENOTCONN) SC(EHOSTUNREACH) SC(ENETUNREACH) SC(EADDRNOTAVAIL) SC(EAFNOSUPPORT)
    SC(EPROTOTYPE) SC(EOPNOTSUPP) SC(ENOPROTOOPT) SC(EPROTONOSUPPORT) SC(ENOTSOCK) SC(EISCONN) SC(EALREADY)
    SC(EDESTADDRREQ) SC(EMSGSIZE) SC(ENETDOWN) SC(ENETRESET) SC(EHOSTDOWN) SC(ESHUTDOWN)
    /* Socket constants (the Rust oracle's socket shims read them from here). */
    SC(AF_UNIX) SC(AF_INET) SC(AF_INET6) SC(SOCK_STREAM) SC(SOCK_DGRAM) SC(SOCK_RAW) SC(SOCK_SEQPACKET)
    SC(SOL_SOCKET) SC(IPPROTO_IP) SC(IPPROTO_IPV6) SC(IPPROTO_TCP) SC(MSG_PEEK) SC(MSG_OOB)
    SC(SO_REUSEADDR) SC(SO_BROADCAST) SC(SO_KEEPALIVE) SC(SO_RCVBUF) SC(SO_SNDBUF) SC(SO_ERROR) SC(SO_TYPE) SC(SO_LINGER)
    SC(TCP_NODELAY) SC(IPV6_V6ONLY) SC(IP_TTL) SC(IP_TOS) SC(IP_MULTICAST_TTL) SC(IP_MULTICAST_LOOP)
    SC(IPV6_UNICAST_HOPS) SC(IPV6_MULTICAST_HOPS) SC(IPV6_MULTICAST_LOOP) SC(IPV6_MULTICAST_IF)
#ifdef SO_REUSEPORT
    SC(SO_REUSEPORT)
#endif
#ifdef SO_NOSIGPIPE
    SC(SO_NOSIGPIPE)
#endif
#ifdef TCP_KEEPIDLE
    SC(TCP_KEEPIDLE)
#else
    { "TCP_KEEPIDLE", (int64_t)TCP_KEEPALIVE },
#endif
#ifdef TCP_KEEPINTVL
    SC(TCP_KEEPINTVL)
#endif
#ifdef TCP_KEEPCNT
    SC(TCP_KEEPCNT)
#endif
    SC(S_ISUID) SC(S_ISGID) SC(S_ISVTX) SC(S_IRGRP) SC(S_IWGRP) SC(S_IXGRP) SC(S_IROTH) SC(S_IWOTH) SC(S_IXOTH)
    SC(SIGHUP) SC(SIGINT) SC(SIGQUIT) SC(SIGILL) SC(SIGTRAP) SC(SIGABRT) SC(SIGFPE) SC(SIGKILL) SC(SIGUSR1)
    SC(SIGSEGV) SC(SIGUSR2) SC(SIGALRM) SC(SIGTERM) SC(SIGCHLD) SC(SIGCONT) SC(SIGSTOP) SC(SIGTSTP) SC(SIGTTIN)
    SC(SIGTTOU) SC(SIGURG) SC(SIGXCPU) SC(SIGXFSZ) SC(SIGVTALRM) SC(SIGPROF) SC(SIGWINCH) SC(SIGIO) SC(SIGSYS)
};
#undef SC

int64_t alx_sys_const_count(void) { return (int64_t)(sizeof sys_consts / sizeof sys_consts[0]); }
const char *alx_sys_const_name(int64_t i) { return sys_consts[i].name; }
int64_t alx_sys_const(const char *name) {
    for (size_t i = 0; i < sizeof sys_consts / sizeof sys_consts[0]; i++)
        if (strcmp(sys_consts[i].name, name) == 0) return sys_consts[i].val;
    return -1;
}

/* ---------- files and directories (std os) ----------
 * Non-variadic, layout-free views of stat(2) and readdir(3). They return 0 or
 * -errno rather than leaving the answer in errno, so every backend (the Rust
 * oracle included) agrees. */

#ifdef __APPLE__
#define ALX_MTIME_NS(st) ((int64_t)(st).st_mtimespec.tv_sec * 1000000000 + (st).st_mtimespec.tv_nsec)
#define ALX_ATIME_NS(st) ((int64_t)(st).st_atimespec.tv_sec * 1000000000 + (st).st_atimespec.tv_nsec)
#else
#define ALX_MTIME_NS(st) ((int64_t)(st).st_mtim.tv_sec * 1000000000 + (st).st_mtim.tv_nsec)
#define ALX_ATIME_NS(st) ((int64_t)(st).st_atim.tv_sec * 1000000000 + (st).st_atim.tv_nsec)
#endif

static void stat_out(const struct stat *st, uint8_t *out) {
    int64_t v[ALX_STAT_FIELDS] = { (int64_t)st->st_mode, (int64_t)st->st_size, ALX_MTIME_NS(*st), ALX_ATIME_NS(*st), (int64_t)st->st_ino, (int64_t)st->st_nlink };
    memcpy(out, v, sizeof v);
}

int32_t alx_sys_stat(const char *path, uint8_t *out, int32_t follow) {
    struct stat st;
    if ((follow ? stat(path, &st) : lstat(path, &st)) != 0) return -errno;
    stat_out(&st, out);
    return 0;
}

int32_t alx_sys_fstat(int32_t fd, uint8_t *out) {
    struct stat st;
    if (fstat(fd, &st) != 0) return -errno;
    stat_out(&st, out);
    return 0;
}

/* A directory handle (a DIR*) or -errno. */
int64_t alx_sys_dir_open(const char *path) {
    DIR *d = opendir(path);
    return d ? (int64_t)(intptr_t)d : -(int64_t)errno;
}

/* The next entry's name (valid until the next call; "." and ".." are skipped),
 * or NULL at the end. kind[0] = 1 file, 2 directory, 3 symlink, 4 other, 0 unknown. */
const char *alx_sys_dir_next(int64_t h, uint8_t *kind) {
    DIR *d = (DIR *)(intptr_t)h;
    struct dirent *e;
    while ((e = readdir(d)) != NULL) {
        if (strcmp(e->d_name, ".") == 0 || strcmp(e->d_name, "..") == 0) continue;
        switch (e->d_type) {
        case DT_REG: kind[0] = 1; break;
        case DT_DIR: kind[0] = 2; break;
        case DT_LNK: kind[0] = 3; break;
        case DT_UNKNOWN: kind[0] = 0; break;
        default: kind[0] = 4;
        }
        return e->d_name;
    }
    return NULL;
}

void alx_sys_dir_close(int64_t h) { closedir((DIR *)(intptr_t)h); }

/* environ[i], NULL past the end. */
extern char **environ;
const char *alx_environ(int64_t i) {
    if (!environ) return NULL;
    for (int64_t k = 0; k <= i; k++)
        if (!environ[k]) return NULL;
    return environ[i];
}

/* ---------- processes (std os/exec) ---------- */

#include <spawn.h>
#include <sys/wait.h>
#include <sys/resource.h>
#include <poll.h>

static char **nul_list(const uint8_t *p, int64_t n) {
    char **v = malloc(sizeof(char *) * (size_t)(n + 1));
    if (!v) return NULL;
    for (int64_t i = 0; i < n; i++) {
        v[i] = (char *)p;
        p += strlen((const char *)p) + 1;
    }
    v[n] = NULL;
    return v;
}

int64_t alx_sys_spawn(const uint8_t *argv, int64_t argc, const uint8_t *env, int64_t envc, const char *dir,
                      int64_t fd0, int64_t fd1, int64_t fd2) {
    char **av = nul_list(argv, argc);
    char **ev = envc >= 0 ? nul_list(env, envc) : environ;
    if (!av || !ev) return -ENOMEM;
    posix_spawn_file_actions_t fa;
    posix_spawn_file_actions_init(&fa);
    int64_t fds[3] = { fd0, fd1, fd2 };
    for (int i = 0; i < 3; i++)
        if (fds[i] >= 0) posix_spawn_file_actions_adddup2(&fa, (int)fds[i], i);
    if (dir && dir[0]) posix_spawn_file_actions_addchdir_np(&fa, dir);
    posix_spawnattr_t at;
    posix_spawnattr_init(&at);
    sigset_t none, all;
    sigemptyset(&none);
    sigfillset(&all);
    posix_spawnattr_setsigmask(&at, &none);
    posix_spawnattr_setsigdefault(&at, &all);
    posix_spawnattr_setflags(&at, POSIX_SPAWN_SETSIGMASK | POSIX_SPAWN_SETSIGDEF);
    pid_t pid;
    int rc = posix_spawnp(&pid, av[0], &fa, &at, av, ev);
    posix_spawn_file_actions_destroy(&fa);
    posix_spawnattr_destroy(&at);
    free(av);
    if (envc >= 0) free(ev);
    return rc ? -(int64_t)rc : (int64_t)pid;
}

/* A task doesn't block its worker in wait4(2): tasks are pinned to their
 * worker, and one of them may be what the child is waiting on (port-issues
 * #154). wait4_task is with the poller below. */
static int64_t wait4_task(pid_t pid, int *st, struct rusage *ru);

int64_t alx_sys_wait(int64_t pid, uint8_t *out) {
    int st;
    struct rusage ru;
    if (tl_task) {
        int64_t r = wait4_task((pid_t)pid, &st, &ru);
        if (r < 0) return r;
    } else {
        while (wait4((pid_t)pid, &st, 0, &ru) < 0)
            if (errno != EINTR) return -errno;
    }
#ifdef __APPLE__
    int64_t rss = (int64_t)ru.ru_maxrss;  /* bytes */
#else
    int64_t rss = (int64_t)ru.ru_maxrss * 1024;  /* KiB */
#endif
    int64_t v[5] = {
        WIFSIGNALED(st) ? 1 : 0,
        WIFSIGNALED(st) ? WTERMSIG(st) : WEXITSTATUS(st),
        (int64_t)ru.ru_utime.tv_sec * 1000000000 + (int64_t)ru.ru_utime.tv_usec * 1000,
        (int64_t)ru.ru_stime.tv_sec * 1000000000 + (int64_t)ru.ru_stime.tv_usec * 1000,
        rss,
    };
    memcpy(out, v, sizeof v);
    return 0;
}

int64_t alx_sys_pipe(uint8_t *out, int64_t nonblock) {
    int p[2];
    if (pipe(p) != 0) return -errno;
    fcntl(p[0], F_SETFD, FD_CLOEXEC);
    fcntl(p[1], F_SETFD, FD_CLOEXEC);
    /* nonblock: bit 0 the read end, bit 1 the write end. */
    if (nonblock & 1) fcntl(p[0], F_SETFL, fcntl(p[0], F_GETFL) | O_NONBLOCK);
    if (nonblock & 2) fcntl(p[1], F_SETFL, fcntl(p[1], F_GETFL) | O_NONBLOCK);
    int64_t v[2] = { p[0], p[1] };
    memcpy(out, v, sizeof v);
    return 0;
}

int64_t alx_sys_exec(const uint8_t *argv, int64_t argc, const uint8_t *env, int64_t envc, const char *dir,
                     int64_t fd0, int64_t fd1, int64_t fd2) {
    char **av = nul_list(argv, argc);
    char **ev = envc >= 0 ? nul_list(env, envc) : environ;
    if (!av || !ev) return -ENOMEM;
    if (dir && dir[0] && chdir(dir) != 0) return -errno;
    int64_t fds[3] = { fd0, fd1, fd2 };
    for (int i = 0; i < 3; i++)
        if (fds[i] >= 0 && fds[i] != i && dup2((int)fds[i], i) < 0) return -errno;
    fflush(stdout);
    fflush(stderr);
    if (envc >= 0) environ = ev;
    execvp(av[0], av);
    return -errno;
}

/* ---------- signals (std os/signal) ----------
 * A self-pipe per watcher: alx_sig_watch(mask) installs one handler for the
 * signals in `mask` (bit n = signal n) and returns the read end of a fresh
 * non-blocking pipe; the handler writes the signal's number (one byte) to the
 * write end of every watcher whose mask has it (dropped when the pipe is
 * full, like Go's non-blocking sends). The std package reads the pipe from a
 * task that parks in alx_fd_wait and forwards to channels. alx_sig_unwatch
 * closes the write end (the reader sees EOF) and restores the default action
 * of signals nobody watches any more. The handler only reads atomics and
 * calls write(2): async-signal-safe. */
#define ALX_SIGW 64
static struct { uint64_t mask; int wfd; int rfd; } g_sigw[ALX_SIGW];
static pthread_mutex_t g_sig_mu = PTHREAD_MUTEX_INITIALIZER;
static uint64_t g_sig_handled, g_sig_ignored;
static bool g_sig_init;
/* Handlers running right now: alx_sig_unwatch waits for none before it closes a pipe. */
static int g_sig_busy;

static void alx_sig_handler(int sig) {
    int saved = errno;
    __atomic_add_fetch(&g_sig_busy, 1, __ATOMIC_SEQ_CST);
    uint64_t bit = (uint64_t)1 << sig;
    for (int i = 0; i < ALX_SIGW; i++) {
        if (__atomic_load_n(&g_sigw[i].mask, __ATOMIC_SEQ_CST) & bit) {
            int fd = __atomic_load_n(&g_sigw[i].wfd, __ATOMIC_SEQ_CST);
            unsigned char b = (unsigned char)sig;
            if (fd >= 0) (void)!write(fd, &b, 1);
        }
    }
    __atomic_sub_fetch(&g_sig_busy, 1, __ATOMIC_SEQ_CST);
    errno = saved;
}

static void sig_set(int sig, void (*h)(int)) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = h;
    sa.sa_flags = SA_RESTART;
    sigemptyset(&sa.sa_mask);
    sigaction(sig, &sa, NULL);
}

/* Signals no watcher has any more go back to the default action. */
static void sig_release_unwatched(void) {
    uint64_t live = 0;
    for (int i = 0; i < ALX_SIGW; i++) live |= __atomic_load_n(&g_sigw[i].mask, __ATOMIC_SEQ_CST);
    for (int s = 1; s < 64; s++) {
        uint64_t bit = (uint64_t)1 << s;
        if ((g_sig_handled & bit) && !(live & bit)) {
            sig_set(s, SIG_DFL);
            g_sig_handled &= ~bit;
        }
    }
}

int64_t alx_sig_watch(int64_t mask) {
    int p[2];
    if (pipe(p) != 0) return -errno;
    for (int k = 0; k < 2; k++) {
        fcntl(p[k], F_SETFD, FD_CLOEXEC);
        fcntl(p[k], F_SETFL, fcntl(p[k], F_GETFL) | O_NONBLOCK);
    }
    pthread_mutex_lock(&g_sig_mu);
    if (!g_sig_init) {
        for (int i = 0; i < ALX_SIGW; i++) { __atomic_store_n(&g_sigw[i].wfd, -1, __ATOMIC_SEQ_CST); g_sigw[i].rfd = -1; }
        g_sig_init = true;
    }
    int slot = -1;
    for (int i = 0; i < ALX_SIGW && slot < 0; i++)
        if (g_sigw[i].rfd < 0) slot = i;
    if (slot < 0) {
        pthread_mutex_unlock(&g_sig_mu);
        close(p[0]);
        close(p[1]);
        return -EMFILE;
    }
    g_sigw[slot].rfd = p[0];
    __atomic_store_n(&g_sigw[slot].wfd, p[1], __ATOMIC_SEQ_CST);
    __atomic_store_n(&g_sigw[slot].mask, (uint64_t)mask, __ATOMIC_SEQ_CST);
    for (int s = 1; s < 64; s++) {
        uint64_t bit = (uint64_t)1 << s;
        if (((uint64_t)mask & bit) && !(g_sig_handled & bit) && s != SIGKILL && s != SIGSTOP) {
            sig_set(s, alx_sig_handler);
            g_sig_handled |= bit;
            g_sig_ignored &= ~bit;
        }
    }
    pthread_mutex_unlock(&g_sig_mu);
    return p[0];
}

int64_t alx_sig_unwatch(int64_t rfd) {
    pthread_mutex_lock(&g_sig_mu);
    for (int i = 0; g_sig_init && i < ALX_SIGW; i++) {
        if (g_sigw[i].rfd == rfd) {
            __atomic_store_n(&g_sigw[i].mask, 0, __ATOMIC_SEQ_CST);
            int w = __atomic_exchange_n(&g_sigw[i].wfd, -1, __ATOMIC_SEQ_CST);
            if (w >= 0) {
                /* A handler that read the old descriptor is still counted in. */
                while (__atomic_load_n(&g_sig_busy, __ATOMIC_SEQ_CST) != 0) {}
                close(w);
            }
            g_sigw[i].rfd = -1;
        }
    }
    sig_release_unwatched();
    pthread_mutex_unlock(&g_sig_mu);
    return 0;
}

/* Go's Reset (how = 0: default action) and Ignore (how = 1): the signals in
 * mask leave every watcher. */
int64_t alx_sig_reset(int64_t mask, int64_t how) {
    pthread_mutex_lock(&g_sig_mu);
    for (int i = 0; g_sig_init && i < ALX_SIGW; i++)
        __atomic_fetch_and(&g_sigw[i].mask, ~(uint64_t)mask, __ATOMIC_SEQ_CST);
    for (int s = 1; s < 64; s++) {
        uint64_t bit = (uint64_t)1 << s;
        if (!((uint64_t)mask & bit) || s == SIGKILL || s == SIGSTOP) continue;
        if (how) {
            sig_set(s, SIG_IGN);
            g_sig_ignored |= bit;
        } else {
            if (g_sig_handled & bit) sig_set(s, SIG_DFL);
            g_sig_ignored &= ~bit;
        }
        g_sig_handled &= ~bit;
    }
    pthread_mutex_unlock(&g_sig_mu);
    return 0;
}

/* Whether sig is ignored (by alx_sig_reset(.., 1) or inherited SIG_IGN). */
int64_t alx_sig_ignored(int64_t sig) {
    if (sig <= 0 || sig >= 64) return 0;
    pthread_mutex_lock(&g_sig_mu);
    bool ign = (g_sig_ignored >> sig) & 1;
    bool handled = (g_sig_handled >> sig) & 1;
    pthread_mutex_unlock(&g_sig_mu);
    if (ign) return 1;
    if (handled) return 0;
    struct sigaction cur;
    if (sigaction((int)sig, NULL, &cur) != 0) return 0;
    return cur.sa_handler == SIG_IGN;
}

/* ---------- users and groups (std os/user) ----------
 * kind 0: user by uid, 1: user by name, 2: group by gid, 3: group by name.
 * Writes NUL-terminated fields to out: a user's uid, gid, login name, GECOS
 * name (up to the first comma) and home directory; a group's gid and name.
 * Returns the bytes written, 0 if there is no such user or group, -ERANGE if
 * out is too small (ask again with more), or -errno. */
#include <pwd.h>
#include <grp.h>

static int64_t put_fields(uint8_t *out, int64_t n, const char **fs, int k) {
    int64_t need = 0;
    for (int i = 0; i < k; i++) need += (int64_t)strlen(fs[i]) + 1;
    if (need > n) return -ERANGE;
    int64_t at = 0;
    for (int i = 0; i < k; i++) {
        size_t l = strlen(fs[i]);
        memcpy(out + at, fs[i], l + 1);
        at += (int64_t)l + 1;
    }
    return at;
}

int64_t alx_user_lookup(int64_t kind, const char *key, uint8_t *out, int64_t n) {
    size_t bl = 16384;
    for (;;) {
        char *buf = malloc(bl);
        if (!buf) return -ENOMEM;
        int rc;
        int64_t r = 0;
        if (kind <= 1) {
            struct passwd pw, *res = NULL;
            rc = kind == 0 ? getpwuid_r((uid_t)strtoul(key, NULL, 10), &pw, buf, bl, &res) : getpwnam_r(key, &pw, buf, bl, &res);
            if (rc == 0 && res) {
                char uid[24], gid[24];
                snprintf(uid, sizeof uid, "%lu", (unsigned long)pw.pw_uid);
                snprintf(gid, sizeof gid, "%lu", (unsigned long)pw.pw_gid);
                char *gecos = pw.pw_gecos ? pw.pw_gecos : "";
                char *comma = strchr(gecos, ',');
                if (comma) *comma = 0;
                const char *fs[5] = { uid, gid, pw.pw_name ? pw.pw_name : "", gecos, pw.pw_dir ? pw.pw_dir : "" };
                r = put_fields(out, n, fs, 5);
            }
        } else {
            struct group gr, *res = NULL;
            rc = kind == 2 ? getgrgid_r((gid_t)strtoul(key, NULL, 10), &gr, buf, bl, &res) : getgrnam_r(key, &gr, buf, bl, &res);
            if (rc == 0 && res) {
                char gid[24];
                snprintf(gid, sizeof gid, "%lu", (unsigned long)gr.gr_gid);
                const char *fs[2] = { gid, gr.gr_name ? gr.gr_name : "" };
                r = put_fields(out, n, fs, 2);
            }
        }
        free(buf);
        if (rc == ERANGE && bl < (1 << 22)) { bl *= 4; continue; }
        /* Not found is 0 (some systems report ENOENT/ESRCH for that). */
        if (rc != 0 && rc != ENOENT && rc != ESRCH && rc != EBADF && rc != EPERM) return -rc;
        return r;
    }
}

/* The group IDs of user `name` (primary group gid included), into out (n
 * slots). Returns the count; more than n means ask again with more room. */
int64_t alx_user_groups(const char *name, int64_t gid, uint8_t *out, int64_t n) {
    int cap = 64;
    for (;;) {
#ifdef __APPLE__
        int *gs = malloc(sizeof(int) * (size_t)cap);
        int cnt = cap;
        int rc = getgrouplist(name, (int)gid, gs, &cnt);
#else
        gid_t *gs = malloc(sizeof(gid_t) * (size_t)cap);
        int cnt = cap;
        int rc = getgrouplist(name, (gid_t)gid, gs, &cnt);
#endif
        if (rc < 0 && cap < 65536) {
            free(gs);
            cap *= 4;
            continue;
        }
        for (int i = 0; i < cnt && i < n; i++) { int64_t v = (int64_t)(unsigned)gs[i]; memcpy(out + 8 * i, &v, 8); }
        free(gs);
        return cnt;
    }
}

/* ======================================================================
 * L3: the I/O event loop (netpoll) and sockets
 * ======================================================================
 * A task that would block on a non-blocking fd calls alx_fd_wait(fd, mode):
 * it registers one-shot interest with the poller (kqueue on macOS/BSD, epoll
 * on Linux) and parks like a sleeper, so its worker runs other tasks. One
 * dedicated poller thread blocks in kevent/epoll_wait and, on readiness (or
 * error/hangup), unparks the waiters. Parked I/O waiters count in g_sleepers
 * (they will wake on their own, so they are not a deadlock). Outside a task
 * (main, a plain thread) alx_fd_wait just poll(2)s.
 *
 * Waiters live in a hash table keyed by fd (g_mu guards it), so events are
 * matched by (fd, mode) rather than by pointer: a stale event for a waiter
 * that alx_fd_close already woke finds nothing and is ignored. Known limits:
 * a woken waiter retries its syscall, so an fd closed and reused in between
 * is not detected (Go uses fd refcounts); the Linux (epoll) path is UNTESTED. */
#include <poll.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <arpa/inet.h>
#include <netdb.h>
#include <sys/un.h>
#if defined(__linux__)
#  include <sys/epoll.h>
#  define ALX_EPOLL 1
#else
#  include <sys/event.h>
#endif

typedef struct IoWait { int fd, mode; Parker *p; struct IoWait *next; } IoWait;
enum { IOW_BUCKETS = 1024 };
static IoWait *g_iow[IOW_BUCKETS];
static int g_pollfd = -1;
static bool g_poller_started;

#define IOW_B(fd) (&g_iow[(unsigned)(fd) % IOW_BUCKETS])

static void iow_remove(IoWait *w) {
    IoWait **pp = IOW_B(w->fd);
    while (*pp && *pp != w) pp = &(*pp)->next;
    if (*pp) *pp = w->next;
}

#ifdef ALX_EPOLL
/* Arm (one-shot) for the union of the modes waited on this fd (g_mu held). */
static int poll_arm(int fd) {
    uint32_t ev = 0;
    for (IoWait *w = *IOW_B(fd); w; w = w->next)
        if (w->fd == fd) ev |= w->mode == 1 ? EPOLLIN : EPOLLOUT;
    if (!ev) return 0;
    struct epoll_event e;
    memset(&e, 0, sizeof e);
    e.events = ev | EPOLLONESHOT | EPOLLRDHUP;
    e.data.fd = fd;
    if (epoll_ctl(g_pollfd, EPOLL_CTL_MOD, fd, &e) == 0) return 0;
    if (errno == ENOENT && epoll_ctl(g_pollfd, EPOLL_CTL_ADD, fd, &e) == 0) return 0;
    return -errno;
}
#else
/* kqueue filters are per (fd, mode); EV_ADD on an existing one just refreshes it. */
static int poll_arm(int fd, int mode) {
    struct kevent kev;
    EV_SET(&kev, fd, mode == 1 ? EVFILT_READ : EVFILT_WRITE, EV_ADD | EV_ONESHOT, 0, 0, NULL);
    return kevent(g_pollfd, &kev, 1, NULL, 0, NULL) < 0 ? -errno : 0;
}
#endif

/* Wake every waiter of (fd, mode); mode 0 = all modes (g_mu held). */
static void iow_wake(int fd, int mode) {
    IoWait **pp = IOW_B(fd);
    while (*pp) {
        IoWait *w = *pp;
        if (w->fd == fd && (mode == 0 || w->mode == mode)) {
            *pp = w->next;
            g_sleepers--;
            unpark(w->p);
        } else pp = &w->next;
    }
}

static void *poller_main(void *arg) {
    (void)arg;
    tl_uncounted = true;
    for (;;) {
#ifdef ALX_EPOLL
        struct epoll_event evs[64];
        int n = epoll_wait(g_pollfd, evs, 64, -1);
        if (n < 0) { if (errno == EINTR) continue; break; }
        pthread_mutex_lock(&g_mu);
        for (int i = 0; i < n; i++) {
            int fd = evs[i].data.fd;
            uint32_t e = evs[i].events;
            bool err = e & (EPOLLERR | EPOLLHUP);
            if (err || (e & (EPOLLIN | EPOLLRDHUP))) iow_wake(fd, 1);
            if (err || (e & EPOLLOUT)) iow_wake(fd, 2);
            poll_arm(fd); /* one-shot: re-arm for waiters still registered */
        }
        pthread_mutex_unlock(&g_mu);
#else
        struct kevent evs[64];
        int n = kevent(g_pollfd, NULL, 0, evs, 64, NULL);
        if (n < 0) { if (errno == EINTR) continue; break; }
        pthread_mutex_lock(&g_mu);
        for (int i = 0; i < n; i++)
            iow_wake((int)evs[i].ident, evs[i].filter == EVFILT_READ ? 1 : 2);
        pthread_mutex_unlock(&g_mu);
#endif
    }
    return NULL;
}

/* g_mu held. */
static bool poller_start(void) {
    if (g_poller_started) return true;
#ifdef ALX_EPOLL
    g_pollfd = epoll_create1(EPOLL_CLOEXEC);
#else
    g_pollfd = kqueue();
    if (g_pollfd >= 0) fcntl(g_pollfd, F_SETFD, FD_CLOEXEC);
#endif
    if (g_pollfd < 0) return false;
    pthread_t th;
    pthread_attr_t at;
    pthread_attr_init(&at);
    pthread_attr_setdetachstate(&at, PTHREAD_CREATE_DETACHED);
    int rc = pthread_create(&th, &at, poller_main, NULL);
    pthread_attr_destroy(&at);
    if (rc != 0) alx_panic("cannot start the poller thread", "runtime");
    g_poller_started = true;
    return true;
}

/* Park the current task until one of fds[i] is ready for modes[i] (1 read,
 * 2 write), has an error or hung up, or (until > 0) the monotonic clock
 * reaches `until`. Negative fds are skipped. Each fd gets its own one-shot
 * registration; whichever fires first wakes the task, and the others (and
 * the timer) are withdrawn here (a late event for one finds nothing). 0,
 * -ETIMEDOUT if only the timer fired, or -errno if an fd can't be polled.
 * In a task only. */
static int64_t fd_wait_task(const int *fds, const int *modes, int n, int64_t until) {
    if (until > 0 && mono_ns() >= until) return -ETIMEDOUT;
    IoWait *ws = calloc((size_t)(n ? n : 1), sizeof *ws);
    if (!ws) alx_panic("out of memory", "runtime");
    Parker *p = cur_pk();
    pthread_mutex_lock(&g_mu);
    if (!poller_start()) { int e = errno; pthread_mutex_unlock(&g_mu); free(ws); return -e; }
    int k = 0;
    for (int i = 0; i < n; i++) {
        if (fds[i] < 0) continue;
        IoWait *w = &ws[k++];
        w->fd = fds[i]; w->mode = modes[i]; w->p = p;
        IoWait **b = IOW_B(w->fd);
        w->next = *b; *b = w;
#ifdef ALX_EPOLL
        int rc = poll_arm(w->fd);
#else
        int rc = poll_arm(w->fd, w->mode);
#endif
        if (rc < 0) {
            for (int j = 0; j < k; j++) iow_remove(&ws[j]);
            pthread_mutex_unlock(&g_mu);
            free(ws);
            return rc;
        }
    }
    if (k == 0) { pthread_mutex_unlock(&g_mu); free(ws); return 0; }
    Timer *t = NULL;
    if (until > 0) {
        t = calloc(1, sizeof *t);
        if (!t) alx_panic("out of memory", "runtime");
        t->at = until; t->p = p; t->keep = true;
        timer_add(t);
    }
    g_sleepers += k;
    p->parked = true; p->dead = false;
    if (p->counted) g_runnable--;
    pthread_mutex_unlock(&g_mu);
    sw_out(p->task, false);
    int fired = 0;
    if (k > 1 || t) {
        /* Withdraw the registrations that didn't fire, and the timer. */
        pthread_mutex_lock(&g_mu);
        for (int j = 0; j < k; j++) {
            IoWait **pp = IOW_B(ws[j].fd);
            while (*pp && *pp != &ws[j]) pp = &(*pp)->next;
            if (*pp) { *pp = ws[j].next; g_sleepers--; } else fired++;
        }
        if (t && !t->fired) timer_cancel(t);
        pthread_mutex_unlock(&g_mu);
    }
    int64_t r = t && t->fired && fired == 0 ? -ETIMEDOUT : 0;
    free(t);
    free(ws);
    return r;
}

/* poll(2) for a plain thread: until (monotonic ns, 0 = none). 0, -ETIMEDOUT
 * or -errno. */
static int64_t fd_poll_until(struct pollfd *pf, int n, int64_t until) {
    for (;;) {
        int ms = -1;
        if (until > 0) {
            int64_t left = until - mono_ns();
            if (left <= 0) return -ETIMEDOUT;
            ms = (int)((left + 999999) / 1000000);
        }
        int r = poll(pf, (nfds_t)n, ms);
        if (r > 0) return 0;
        if (r == 0) continue;
        if (errno != EINTR) return -errno;
    }
}

/* Wait until fd is readable (mode 1) or writable (mode 2), or has an error or
 * hung up (the caller retries its syscall and sees which). Parks the task;
 * outside a task it blocks in poll(2). 0, or -errno if the fd can't be polled. */
int64_t alx_fd_wait(int64_t fd, int64_t mode) {
    return alx_fd_wait_until(fd, mode, 0);
}

/* alx_fd_wait that gives up when the monotonic clock (alx_mono_ns) reaches
 * `until` (0: never): -ETIMEDOUT then. Go's deadlines on network
 * connections. */
int64_t alx_fd_wait_until(int64_t fd, int64_t mode, int64_t until) {
    if (!tl_task) {
        struct pollfd pf;
        pf.fd = (int)fd; pf.events = mode == 1 ? POLLIN : POLLOUT; pf.revents = 0;
        return fd_poll_until(&pf, 1, until);
    }
    int f = (int)fd, m = (int)mode;
    return fd_wait_task(&f, &m, 1, until);
}

/* Wakes every task waiting on fd without closing it (they retry: a changed
 * deadline takes effect at once). */
int64_t alx_fd_wake(int64_t fd) {
    pthread_mutex_lock(&g_mu);
    iow_wake((int)fd, 0);
    pthread_mutex_unlock(&g_mu);
    return 0;
}

/* Wait until a or b (-1: none) is readable or hung up. A task parks, so its
 * worker runs other tasks: blocking the worker in poll(2) would starve every
 * task pinned to it, the one the child process is waiting on among them
 * (port-issues #154). A plain thread blocks in poll(2). 0 or -errno. */
int64_t alx_sys_poll2(int64_t a, int64_t b) {
    if (tl_task) {
        int fds[2] = { (int)a, (int)b }, modes[2] = { 1, 1 };
        return fd_wait_task(fds, modes, 2, 0);
    }
    struct pollfd pf[2] = { { (int)a, POLLIN, 0 }, { (int)b, POLLIN, 0 } };
    while (poll(pf, 2, -1) < 0)
        if (errno != EINTR) return -errno;
    return 0;
}

/* wait4(2) from a task: a helper thread blocks in it while the task parks
 * (for the same reason as alx_sys_poll2). */
typedef struct WaitJob {
    pid_t pid;
    int st, err;
    struct rusage ru;
    bool done, waiting;
    Parker *p;
} WaitJob;

static void *wait_main(void *arg) {
    WaitJob *j = arg;
    tl_uncounted = true;
    int err = 0;
    while (wait4(j->pid, &j->st, 0, &j->ru) < 0)
        if (errno != EINTR) { err = errno; break; }
    pthread_mutex_lock(&g_mu);
    j->err = err;
    j->done = true;
    if (j->waiting) { j->waiting = false; g_sleepers--; unpark(j->p); }
    pthread_mutex_unlock(&g_mu);
    return NULL;
}

/* 0 with *st and *ru filled, or -errno. The job is on the heap: a parked
 * task's stack may be another task's while it waits (copy mode). */
static int64_t wait4_task(pid_t pid, int *st, struct rusage *ru) {
    WaitJob *j = calloc(1, sizeof *j);
    if (!j) alx_panic("out of memory", "runtime");
    j->pid = pid;
    j->p = cur_pk();
    pthread_t th;
    pthread_attr_t at;
    pthread_attr_init(&at);
    pthread_attr_setdetachstate(&at, PTHREAD_CREATE_DETACHED);
    pthread_attr_setstacksize(&at, 64 << 10);
    int rc = pthread_create(&th, &at, wait_main, j);
    pthread_attr_destroy(&at);
    if (rc != 0) { free(j); return -(int64_t)rc; }
    pthread_mutex_lock(&g_mu);
    while (!j->done) {
        Parker *p = j->p;
        j->waiting = true;
        g_sleepers++;
        p->parked = true; p->dead = false;
        if (p->counted) g_runnable--;
        pthread_mutex_unlock(&g_mu);
        sw_out(p->task, false);
        pthread_mutex_lock(&g_mu);
    }
    pthread_mutex_unlock(&g_mu);
    int64_t r = j->err ? -(int64_t)j->err : 0;
    *st = j->st;
    *ru = j->ru;
    free(j);
    return r;
}

/* close(2) that first wakes every task waiting on fd (they retry and see
 * EBADF). 0 or -errno. */
int64_t alx_fd_close(int64_t fd) {
    pthread_mutex_lock(&g_mu);
    iow_wake((int)fd, 0);
#ifdef ALX_EPOLL
    if (g_poller_started) epoll_ctl(g_pollfd, EPOLL_CTL_DEL, (int)fd, NULL);
#endif
    pthread_mutex_unlock(&g_mu);
    return close((int)fd) == 0 ? 0 : -errno;
}

/* ---------- sockets ---------- */

/* getaddrinfo failed (host not found): a code that is no errno. */
#define ALX_ENOHOST 100000

static void sock_prep(int fd) {
    int fl = fcntl(fd, F_GETFL, 0);
    if (fl >= 0) fcntl(fd, F_SETFL, fl | O_NONBLOCK);
    fcntl(fd, F_SETFD, FD_CLOEXEC);
#ifdef SO_NOSIGPIPE
    int one = 1;
    setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, sizeof one);
#endif
}

static void sigpipe_ignore(void) { signal(SIGPIPE, SIG_IGN); }
static void sock_init(void) {
    static pthread_once_t once = PTHREAD_ONCE_INIT;
    pthread_once(&once, sigpipe_ignore);
}

/* "ip:port" or "[ip6]:port", NUL-terminated, into out (>= 64 bytes). Length.
 * A Unix socket's address is its path (truncated to 63 bytes; "" if unbound). */
static int64_t sock_fmt(const struct sockaddr_storage *ss, uint8_t *out) {
    char h[INET6_ADDRSTRLEN] = "";
    if (ss->ss_family == AF_UNIX) {
        const struct sockaddr_un *u = (const void *)ss;
        return snprintf((char *)out, 64, "%s", u->sun_path);
    }
    if (ss->ss_family == AF_INET6) {
        const struct sockaddr_in6 *a = (const void *)ss;
        inet_ntop(AF_INET6, &a->sin6_addr, h, sizeof h);
        return snprintf((char *)out, 64, "[%s]:%d", h, ntohs(a->sin6_port));
    }
    const struct sockaddr_in *a = (const void *)ss;
    inet_ntop(AF_INET, &a->sin_addr, h, sizeof h);
    return snprintf((char *)out, 64, "%s:%d", h, ntohs(a->sin_port));
}

/* A TCP listening socket on host:port ("" or "0.0.0.0": any IPv4; "::": any
 * IPv6, dual-stack). SO_REUSEADDR; non-blocking, close-on-exec. The fd, or
 * -errno (-ALX_ENOHOST if host doesn't resolve). Resolution blocks the worker. */
int64_t alx_sock_listen(const char *host, int64_t port, int64_t backlog) {
    sock_init();
    struct addrinfo hints, *res = NULL;
    memset(&hints, 0, sizeof hints);
    hints.ai_socktype = SOCK_STREAM;
    hints.ai_flags = AI_PASSIVE;
    if (!host[0]) host = "0.0.0.0";
    char ps[16];
    snprintf(ps, sizeof ps, "%d", (int)port);
    if (getaddrinfo(host, ps, &hints, &res) != 0) return -ALX_ENOHOST;
    int err = EADDRNOTAVAIL, fd = -1;
    for (struct addrinfo *a = res; a; a = a->ai_next) {
        fd = socket(a->ai_family, a->ai_socktype, a->ai_protocol);
        if (fd < 0) { err = errno; continue; }
        int one = 1, zero = 0;
        setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
        if (a->ai_family == AF_INET6) setsockopt(fd, IPPROTO_IPV6, IPV6_V6ONLY, &zero, sizeof zero);
        if (bind(fd, a->ai_addr, a->ai_addrlen) == 0 && listen(fd, (int)backlog) == 0) { sock_prep(fd); break; }
        err = errno;
        close(fd);
        fd = -1;
    }
    freeaddrinfo(res);
    return fd >= 0 ? fd : -err;
}

/* Non-blocking accept: the new fd (non-blocking, close-on-exec) with the peer's
 * "ip:port" in out (>= 64 bytes), or -errno (-EAGAIN: wait readable, retry). */
int64_t alx_sock_accept(int64_t fd, uint8_t *out) {
    struct sockaddr_storage ss;
    socklen_t len = sizeof ss;
    int c = accept((int)fd, (struct sockaddr *)&ss, &len);
    if (c < 0) return -errno;
    sock_prep(c);
    sock_fmt(&ss, out);
    return c;
}

/* Starts a TCP connection to host:port (IPv4 preferred when the name has
 * several addresses; "" is 127.0.0.1) without waiting for it: the fd comes
 * back at once, the caller waits writable and then reads alx_sock_error.
 * Or -errno / -ALX_ENOHOST. Resolution blocks the worker. */
int64_t alx_sock_connect(const char *host, int64_t port) {
    sock_init();
    struct addrinfo hints, *res = NULL;
    memset(&hints, 0, sizeof hints);
    hints.ai_socktype = SOCK_STREAM;
    char ps[16];
    snprintf(ps, sizeof ps, "%d", (int)port);
    if (getaddrinfo(host[0] ? host : "127.0.0.1", ps, &hints, &res) != 0) return -ALX_ENOHOST;
    struct addrinfo *pick = res;
    for (struct addrinfo *a = res; a; a = a->ai_next)
        if (a->ai_family == AF_INET) { pick = a; break; }
    int fd = socket(pick->ai_family, pick->ai_socktype, pick->ai_protocol);
    if (fd < 0) { int e = errno; freeaddrinfo(res); return -e; }
    sock_prep(fd);
    int rc = connect(fd, pick->ai_addr, pick->ai_addrlen);
    int e = errno;
    freeaddrinfo(res);
    if (rc < 0 && e != EINPROGRESS) { close(fd); return -e; }
    return fd;
}

/* SO_ERROR (and clears it): 0 if the connection succeeded. */
int64_t alx_sock_error(int64_t fd) {
    int err = 0;
    socklen_t len = sizeof err;
    if (getsockopt((int)fd, SOL_SOCKET, SO_ERROR, &err, &len) < 0) return errno;
    return err;
}

int64_t alx_sock_local_addr(int64_t fd, uint8_t *out) {
    struct sockaddr_storage ss;
    socklen_t len = sizeof ss;
    if (getsockname((int)fd, (struct sockaddr *)&ss, &len) < 0) return -errno;
    return sock_fmt(&ss, out);
}

int64_t alx_sock_peer_addr(int64_t fd, uint8_t *out) {
    struct sockaddr_storage ss;
    socklen_t len = sizeof ss;
    if (getpeername((int)fd, (struct sockaddr *)&ss, &len) < 0) return -errno;
    return sock_fmt(&ss, out);
}

int64_t alx_sock_set_nodelay(int64_t fd, int64_t on) {
    int v = on != 0;
    return setsockopt((int)fd, IPPROTO_TCP, TCP_NODELAY, &v, sizeof v) < 0 ? -errno : 0;
}

/* how: 0 read side, 1 write side, 2 both. */
int64_t alx_sock_shutdown(int64_t fd, int64_t how) {
    return shutdown((int)fd, how == 0 ? SHUT_RD : how == 1 ? SHUT_WR : SHUT_RDWR) < 0 ? -errno : 0;
}

/* The addresses of host, one per line ("127.0.0.1\n::1\n"), into out (n bytes,
 * truncated); the length, or -ALX_ENOHOST. Blocks the worker. */
int64_t alx_sock_lookup(const char *host, uint8_t *out, int64_t n) {
    struct addrinfo hints, *res = NULL;
    memset(&hints, 0, sizeof hints);
    hints.ai_socktype = SOCK_STREAM;
    if (getaddrinfo(host, NULL, &hints, &res) != 0) return -ALX_ENOHOST;
    int64_t len = 0;
    for (struct addrinfo *a = res; a; a = a->ai_next) {
        char h[INET6_ADDRSTRLEN];
        const void *src;
        if (a->ai_family == AF_INET6) src = &((struct sockaddr_in6 *)(void *)a->ai_addr)->sin6_addr;
        else if (a->ai_family == AF_INET) src = &((struct sockaddr_in *)(void *)a->ai_addr)->sin_addr;
        else continue;
        if (!inet_ntop(a->ai_family, src, h, sizeof h)) continue;
        size_t hl = strlen(h);
        if (len + (int64_t)hl + 1 > n) break;
        memcpy(out + len, h, hl);
        len += (int64_t)hl;
        out[len++] = '\n';
    }
    freeaddrinfo(res);
    return len;
}

/* A UDP or Unix-domain socket. kind: 1 "udp", 2 "unix" (stream), 3
 * "unixgram"; for kinds 2 and 3 `host` is the path and port is ignored.
 * listen 0 connects (UDP and unixgram at once; a unix stream connect may be
 * in progress: wait writable and read alx_sock_error, as for TCP), 1 binds
 * (and listens, for "unix", with `backlog`). The fd (non-blocking,
 * close-on-exec), or -errno / -ALX_ENOHOST. Resolution blocks the worker. */
int64_t alx_sock_open(int64_t kind, const char *host, int64_t port, int64_t listen_, int64_t backlog) {
    sock_init();
    if (kind == 2 || kind == 3) {
        struct sockaddr_un u;
        memset(&u, 0, sizeof u);
        u.sun_family = AF_UNIX;
        size_t n = strlen(host);
        if (n >= sizeof u.sun_path) return -EINVAL;
        memcpy(u.sun_path, host, n);
        int fd = socket(AF_UNIX, kind == 2 ? SOCK_STREAM : SOCK_DGRAM, 0);
        if (fd < 0) return -errno;
        sock_prep(fd);
        int rc = listen_ ? bind(fd, (struct sockaddr *)&u, sizeof u) : connect(fd, (struct sockaddr *)&u, sizeof u);
        if (rc == 0 && listen_ && kind == 2) rc = listen(fd, (int)backlog);
        if (rc < 0 && !(errno == EINPROGRESS && !listen_)) { int e = errno; close(fd); return -e; }
        return fd;
    }
    struct addrinfo hints, *res = NULL;
    memset(&hints, 0, sizeof hints);
    hints.ai_socktype = SOCK_DGRAM;
    if (listen_) hints.ai_flags = AI_PASSIVE;
    char ps[16];
    snprintf(ps, sizeof ps, "%d", (int)port);
    const char *h = host[0] ? host : (listen_ ? "0.0.0.0" : "127.0.0.1");
    if (getaddrinfo(h, ps, &hints, &res) != 0) return -ALX_ENOHOST;
    struct addrinfo *pick = res;
    for (struct addrinfo *a = res; a; a = a->ai_next)
        if (a->ai_family == AF_INET) { pick = a; break; }
    int fd = socket(pick->ai_family, pick->ai_socktype, pick->ai_protocol);
    if (fd < 0) { int e = errno; freeaddrinfo(res); return -e; }
    sock_prep(fd);
    int rc = listen_ ? bind(fd, pick->ai_addr, pick->ai_addrlen) : connect(fd, pick->ai_addr, pick->ai_addrlen);
    int e = errno;
    freeaddrinfo(res);
    if (rc < 0) { close(fd); return -e; }
    return fd;
}

/* Non-blocking recvfrom: the datagram's length (truncated to n) with the
 * sender's address in out (>= 64 bytes; "" if it has none), or -errno
 * (-EAGAIN: wait readable, retry). */
int64_t alx_sock_recvfrom(int64_t fd, uint8_t *buf, int64_t n, uint8_t *out) {
    struct sockaddr_storage ss;
    socklen_t len = sizeof ss;
    memset(&ss, 0, sizeof ss);
    ssize_t r = recvfrom((int)fd, buf, (size_t)n, 0, (struct sockaddr *)&ss, &len);
    if (r < 0) return -errno;
    if (len == 0) ss.ss_family = AF_UNIX; /* an unbound unixgram sender */
    sock_fmt(&ss, out);
    return (int64_t)r;
}

/* ---------- sockets, the general layer (std net) ----------
 * std/net does what Go's net does on top of system calls (sock_posix.go and
 * friends): it makes sockets, binds, connects and listens itself. Addresses
 * cross this boundary in one layout-free form, 128 bytes:
 *   [0]      family: 0 none, 1 unix, 4 inet, 6 inet6
 *   [1..2]   port, big-endian (inet, inet6)
 *   [4..7]   inet6 scope id, big-endian; for unix, the path's length
 *   [8..23]  the address (inet: [8..11]); unix: the path (up to 104 bytes,
 *            may hold NULs: Linux's abstract names)
 * Every call returns -errno on failure. */

#define ALX_SA_SIZE 128

static int sa_family_of(int f) { return f == 1 ? AF_UNIX : f == 6 ? AF_INET6 : AF_INET; }

/* The canonical form into a sockaddr: its length, or -EINVAL. */
static int sa_from(const uint8_t *in, struct sockaddr_storage *ss, socklen_t *len) {
    memset(ss, 0, sizeof *ss);
    if (in[0] == 4) {
        struct sockaddr_in *a = (void *)ss;
        a->sin_family = AF_INET;
        a->sin_port = htons((uint16_t)(in[1] << 8 | in[2]));
        memcpy(&a->sin_addr, in + 8, 4);
#ifdef __APPLE__
        a->sin_len = sizeof *a;
#endif
        *len = sizeof *a;
        return 0;
    }
    if (in[0] == 6) {
        struct sockaddr_in6 *a = (void *)ss;
        a->sin6_family = AF_INET6;
        a->sin6_port = htons((uint16_t)(in[1] << 8 | in[2]));
        a->sin6_scope_id = (uint32_t)in[4] << 24 | (uint32_t)in[5] << 16 | (uint32_t)in[6] << 8 | in[7];
        memcpy(&a->sin6_addr, in + 8, 16);
#ifdef __APPLE__
        a->sin6_len = sizeof *a;
#endif
        *len = sizeof *a;
        return 0;
    }
    if (in[0] == 1) {
        struct sockaddr_un *u = (void *)ss;
        size_t n = (size_t)in[6] << 8 | in[7];
        if (n > sizeof u->sun_path || n > ALX_SA_SIZE - 8) return -EINVAL;
        u->sun_family = AF_UNIX;
        memcpy(u->sun_path, in + 8, n);
        /* A path (not an abstract name) is NUL-terminated when there's room. */
        *len = (socklen_t)(offsetof(struct sockaddr_un, sun_path) + n);
        if (n > 0 && in[8] != 0 && n < sizeof u->sun_path) *len += 1;
#ifdef __APPLE__
        u->sun_len = (uint8_t)*len;
#endif
        return 0;
    }
    return -EAFNOSUPPORT;
}

/* A sockaddr (len bytes) into the canonical form. */
static void sa_to(const struct sockaddr_storage *ss, socklen_t len, uint8_t *out) {
    memset(out, 0, ALX_SA_SIZE);
    if (len == 0) return;
    if (ss->ss_family == AF_INET) {
        const struct sockaddr_in *a = (const void *)ss;
        uint16_t p = ntohs(a->sin_port);
        out[0] = 4; out[1] = (uint8_t)(p >> 8); out[2] = (uint8_t)p;
        memcpy(out + 8, &a->sin_addr, 4);
    } else if (ss->ss_family == AF_INET6) {
        const struct sockaddr_in6 *a = (const void *)ss;
        uint16_t p = ntohs(a->sin6_port);
        uint32_t z = a->sin6_scope_id;
        out[0] = 6; out[1] = (uint8_t)(p >> 8); out[2] = (uint8_t)p;
        out[4] = (uint8_t)(z >> 24); out[5] = (uint8_t)(z >> 16); out[6] = (uint8_t)(z >> 8); out[7] = (uint8_t)z;
        memcpy(out + 8, &a->sin6_addr, 16);
    } else if (ss->ss_family == AF_UNIX) {
        const struct sockaddr_un *u = (const void *)ss;
        size_t off = offsetof(struct sockaddr_un, sun_path);
        size_t n = len > off ? len - off : 0;
        if (n > sizeof u->sun_path) n = sizeof u->sun_path;
        if (n > ALX_SA_SIZE - 8) n = ALX_SA_SIZE - 8;
        /* A path: up to its NUL. An abstract name (Linux, leading NUL): all
         * of it. Elsewhere a leading NUL is an unnamed socket. */
#ifdef __linux__
        if (n > 0 && u->sun_path[0] != 0) n = strnlen(u->sun_path, n);
#else
        n = n > 0 ? strnlen(u->sun_path, n) : 0;
#endif
        out[0] = 1;
        out[6] = (uint8_t)(n >> 8); out[7] = (uint8_t)n;
        memcpy(out + 8, u->sun_path, n);
    }
}

/* socket(2): family as the canonical form's, sotype 1 stream, 2 datagram,
 * 3 raw, 5 seqpacket; non-blocking, close-on-exec, no SIGPIPE. */
int64_t alx_net_socket(int64_t family, int64_t sotype, int64_t proto) {
    sock_init();
    int t = sotype == 1 ? SOCK_STREAM : sotype == 2 ? SOCK_DGRAM : sotype == 3 ? SOCK_RAW : SOCK_SEQPACKET;
    int fd = socket(sa_family_of((int)family), t, (int)proto);
    if (fd < 0) return -errno;
    sock_prep(fd);
    return fd;
}

int64_t alx_net_bind(int64_t fd, const uint8_t *sa) {
    struct sockaddr_storage ss;
    socklen_t len;
    int rc = sa_from(sa, &ss, &len);
    if (rc < 0) return rc;
    return bind((int)fd, (struct sockaddr *)&ss, len) < 0 ? -errno : 0;
}

/* connect(2); -EINPROGRESS: wait writable, then alx_sock_error. */
int64_t alx_net_connect(int64_t fd, const uint8_t *sa) {
    struct sockaddr_storage ss;
    socklen_t len;
    int rc = sa_from(sa, &ss, &len);
    if (rc < 0) return rc;
    return connect((int)fd, (struct sockaddr *)&ss, len) < 0 ? -errno : 0;
}

int64_t alx_net_listen(int64_t fd, int64_t backlog) {
    return listen((int)fd, (int)backlog) < 0 ? -errno : 0;
}

/* accept(2): the new descriptor (prepared like alx_net_socket's) and the
 * peer's address in out. -EAGAIN: wait readable. */
int64_t alx_net_accept(int64_t fd, uint8_t *out) {
    struct sockaddr_storage ss;
    socklen_t len = sizeof ss;
    memset(&ss, 0, sizeof ss);
    int c = accept((int)fd, (struct sockaddr *)&ss, &len);
    if (c < 0) return -errno;
    sock_prep(c);
    sa_to(&ss, len, out);
    return c;
}

/* The local (peer 0) or remote (peer 1) address into out. */
int64_t alx_net_sockname(int64_t fd, uint8_t *out, int64_t peer) {
    struct sockaddr_storage ss;
    socklen_t len = sizeof ss;
    memset(&ss, 0, sizeof ss);
    int rc = peer ? getpeername((int)fd, (struct sockaddr *)&ss, &len) : getsockname((int)fd, (struct sockaddr *)&ss, &len);
    if (rc < 0) return -errno;
    sa_to(&ss, len, out);
    return 0;
}

/* recvfrom(2) (flags: 1 MSG_PEEK, 2 MSG_OOB): the length, the sender in out
 * (family 0 if it has none). -EAGAIN: wait readable. */
int64_t alx_net_recvfrom(int64_t fd, uint8_t *buf, int64_t n, int64_t flags, uint8_t *out) {
    struct sockaddr_storage ss;
    socklen_t len = sizeof ss;
    memset(&ss, 0, sizeof ss);
    int f = (flags & 1 ? MSG_PEEK : 0) | (flags & 2 ? MSG_OOB : 0);
    ssize_t r = recvfrom((int)fd, buf, (size_t)n, f, (struct sockaddr *)&ss, &len);
    if (r < 0) return -errno;
    sa_to(&ss, len, out);
    return (int64_t)r;
}

/* sendto(2) to sa, or send(2) when sa's family is 0. -EAGAIN: wait writable. */
int64_t alx_net_sendto(int64_t fd, const uint8_t *buf, int64_t n, const uint8_t *sa) {
    ssize_t r;
    if (sa[0] == 0) r = send((int)fd, buf, (size_t)n, 0);
    else {
        struct sockaddr_storage ss;
        socklen_t len;
        int rc = sa_from(sa, &ss, &len);
        if (rc < 0) return rc;
        r = sendto((int)fd, buf, (size_t)n, 0, (struct sockaddr *)&ss, len);
    }
    return r < 0 ? -errno : (int64_t)r;
}

/* recvmsg(2) with ancillary data: the data into buf, the control messages
 * into oob, and into info (two native int64s) their length and the flags
 * (1 MSG_TRUNC, 2 MSG_CTRUNC); the sender into out. The data length. */
int64_t alx_net_recvmsg(int64_t fd, uint8_t *buf, int64_t n, uint8_t *oob, int64_t oobn, uint8_t *out, uint8_t *info) {
    struct sockaddr_storage ss;
    memset(&ss, 0, sizeof ss);
    struct iovec iov = { buf, (size_t)n };
    struct msghdr m;
    memset(&m, 0, sizeof m);
    m.msg_name = &ss; m.msg_namelen = sizeof ss;
    m.msg_iov = &iov; m.msg_iovlen = 1;
    if (oobn > 0) { m.msg_control = oob; m.msg_controllen = (socklen_t)oobn; }
    ssize_t r = recvmsg((int)fd, &m, 0);
    if (r < 0) return -errno;
    int64_t iv[2] = { (int64_t)m.msg_controllen, (m.msg_flags & MSG_TRUNC ? 1 : 0) | (m.msg_flags & MSG_CTRUNC ? 2 : 0) };
    memcpy(info, iv, sizeof iv);
    sa_to(&ss, m.msg_namelen, out);
    return (int64_t)r;
}

/* sendmsg(2) with ancillary data oob (oobn bytes) to sa (family 0: the
 * connected peer). The data length sent. */
int64_t alx_net_sendmsg(int64_t fd, const uint8_t *buf, int64_t n, const uint8_t *oob, int64_t oobn, const uint8_t *sa) {
    struct sockaddr_storage ss;
    socklen_t len = 0;
    if (sa[0] != 0) {
        int rc = sa_from(sa, &ss, &len);
        if (rc < 0) return rc;
    }
    uint8_t dummy = 0;
    struct iovec iov = { (void *)buf, (size_t)n };
    if (n == 0) { iov.iov_base = &dummy; iov.iov_len = 0; }
    struct msghdr m;
    memset(&m, 0, sizeof m);
    if (len) { m.msg_name = &ss; m.msg_namelen = len; }
    m.msg_iov = &iov; m.msg_iovlen = 1;
    if (oobn > 0) { m.msg_control = (void *)oob; m.msg_controllen = (socklen_t)oobn; }
    ssize_t r = sendmsg((int)fd, &m, 0);
    return r < 0 ? -errno : (int64_t)r;
}

/* Builds the SCM_RIGHTS control message for fds (n native int64s) into out
 * (room bytes): its length (Go's syscall.UnixRights), or -EINVAL if it
 * won't fit. */
int64_t alx_net_unix_rights(const uint8_t *fds, int64_t n, uint8_t *out, int64_t room) {
    size_t need = CMSG_SPACE((size_t)n * sizeof(int));
    if ((int64_t)need > room) return -EINVAL;
    memset(out, 0, need);
    struct cmsghdr *h = (struct cmsghdr *)(void *)out;
    h->cmsg_level = SOL_SOCKET;
    h->cmsg_type = SCM_RIGHTS;
    h->cmsg_len = CMSG_LEN((size_t)n * sizeof(int));
    int *p = (int *)(void *)CMSG_DATA(h);
    for (int64_t i = 0; i < n; i++) { int64_t v; memcpy(&v, fds + 8 * i, 8); p[i] = (int)v; }
    return (int64_t)need;
}

/* The descriptors in the SCM_RIGHTS messages of a control buffer (n bytes)
 * into fds (room native int64s): how many, or -EINVAL for a malformed
 * buffer. */
int64_t alx_net_parse_rights(uint8_t *oob, int64_t n, uint8_t *fds, int64_t room) {
    struct msghdr m;
    memset(&m, 0, sizeof m);
    m.msg_control = oob; m.msg_controllen = (socklen_t)n;
    int64_t k = 0;
    for (struct cmsghdr *h = CMSG_FIRSTHDR(&m); h; h = CMSG_NXTHDR(&m, h)) {
        if (h->cmsg_len < CMSG_LEN(0) || (uint8_t *)h + h->cmsg_len > oob + n) return -EINVAL;
        if (h->cmsg_level != SOL_SOCKET || h->cmsg_type != SCM_RIGHTS) continue;
        size_t cnt = (h->cmsg_len - CMSG_LEN(0)) / sizeof(int);
        int *p = (int *)(void *)CMSG_DATA(h);
        for (size_t i = 0; i < cnt && k < room; i++) { int64_t v = p[i]; memcpy(fds + 8 * k++, &v, 8); }
    }
    return k;
}

/* A socket option by name: level and option as Go's syscall names them. */
static int sockopt_of(const char *name, int *level, int *opt) {
    static const struct { const char *n; int l, o; } t[] = {
        { "SO_REUSEADDR", SOL_SOCKET, SO_REUSEADDR },
#ifdef SO_REUSEPORT
        { "SO_REUSEPORT", SOL_SOCKET, SO_REUSEPORT },
#endif
        { "SO_BROADCAST", SOL_SOCKET, SO_BROADCAST },
        { "SO_KEEPALIVE", SOL_SOCKET, SO_KEEPALIVE },
        { "SO_RCVBUF", SOL_SOCKET, SO_RCVBUF },
        { "SO_SNDBUF", SOL_SOCKET, SO_SNDBUF },
        { "SO_ERROR", SOL_SOCKET, SO_ERROR },
        { "SO_TYPE", SOL_SOCKET, SO_TYPE },
        { "SO_LINGER", SOL_SOCKET, SO_LINGER },
        { "TCP_NODELAY", IPPROTO_TCP, TCP_NODELAY },
#ifdef TCP_KEEPIDLE
        { "TCP_KEEPIDLE", IPPROTO_TCP, TCP_KEEPIDLE },
#else
        { "TCP_KEEPIDLE", IPPROTO_TCP, TCP_KEEPALIVE },
#endif
#ifdef TCP_KEEPINTVL
        { "TCP_KEEPINTVL", IPPROTO_TCP, TCP_KEEPINTVL },
#endif
#ifdef TCP_KEEPCNT
        { "TCP_KEEPCNT", IPPROTO_TCP, TCP_KEEPCNT },
#endif
        { "IPV6_V6ONLY", IPPROTO_IPV6, IPV6_V6ONLY },
        { "IP_TTL", IPPROTO_IP, IP_TTL },
        { "IP_TOS", IPPROTO_IP, IP_TOS },
        { "IP_MULTICAST_TTL", IPPROTO_IP, IP_MULTICAST_TTL },
        { "IP_MULTICAST_LOOP", IPPROTO_IP, IP_MULTICAST_LOOP },
        { "IPV6_UNICAST_HOPS", IPPROTO_IPV6, IPV6_UNICAST_HOPS },
        { "IPV6_MULTICAST_HOPS", IPPROTO_IPV6, IPV6_MULTICAST_HOPS },
        { "IPV6_MULTICAST_LOOP", IPPROTO_IPV6, IPV6_MULTICAST_LOOP },
        { "IPV6_MULTICAST_IF", IPPROTO_IPV6, IPV6_MULTICAST_IF },
    };
    for (size_t i = 0; i < sizeof t / sizeof t[0]; i++)
        if (strcmp(t[i].n, name) == 0) { *level = t[i].l; *opt = t[i].o; return 0; }
    return -ENOPROTOOPT;
}

/* setsockopt(2) with an int (SO_LINGER: v < 0 turns lingering off, else
 * lingers v seconds; IP_MULTICAST_TTL / _LOOP take a byte on BSDs). */
int64_t alx_net_setsockopt(int64_t fd, const char *name, int64_t v) {
    int level, opt;
    int rc = sockopt_of(name, &level, &opt);
    if (rc < 0) return rc;
    if (opt == SO_LINGER && level == SOL_SOCKET) {
        struct linger l = { v >= 0, v >= 0 ? (int)v : 0 };
        return setsockopt((int)fd, level, opt, &l, sizeof l) < 0 ? -errno : 0;
    }
#ifndef __linux__
    if (level == IPPROTO_IP && (opt == IP_MULTICAST_TTL || opt == IP_MULTICAST_LOOP)) {
        unsigned char b = (unsigned char)v;
        return setsockopt((int)fd, level, opt, &b, sizeof b) < 0 ? -errno : 0;
    }
#endif
    int iv = (int)v;
    return setsockopt((int)fd, level, opt, &iv, sizeof iv) < 0 ? -errno : 0;
}

/* getsockopt(2) of an int: the value (>= 0), or -errno. SO_LINGER: the
 * seconds it lingers, 0 when off. */
int64_t alx_net_getsockopt(int64_t fd, const char *name) {
    int level, opt;
    int rc = sockopt_of(name, &level, &opt);
    if (rc < 0) return rc;
    if (opt == SO_LINGER && level == SOL_SOCKET) {
        struct linger l;
        socklen_t len = sizeof l;
        if (getsockopt((int)fd, level, opt, &l, &len) < 0) return -errno;
        return l.l_onoff ? l.l_linger : 0;
    }
    unsigned char b[sizeof(int)] = { 0 };
    int iv = 0;
    socklen_t len = sizeof iv;
    if (getsockopt((int)fd, level, opt, b, &len) < 0) return -errno;
    if (len == 1) return b[0];
    memcpy(&iv, b, sizeof iv);
    return iv;
}

/* Joins (join 1) or leaves (0) the multicast group ip (4 or 16 bytes, by
 * family) on the interface ifindex (0: the default). For IPv4, ifaddr (4
 * bytes) picks the interface instead. */
int64_t alx_net_mcast(int64_t fd, int64_t family, const uint8_t *ip, int64_t ifindex, const uint8_t *ifaddr, int64_t join) {
    if (family == 4) {
        struct ip_mreq m;
        memset(&m, 0, sizeof m);
        memcpy(&m.imr_multiaddr, ip, 4);
        memcpy(&m.imr_interface, ifaddr, 4);
        return setsockopt((int)fd, IPPROTO_IP, join ? IP_ADD_MEMBERSHIP : IP_DROP_MEMBERSHIP, &m, sizeof m) < 0 ? -errno : 0;
    }
    struct ipv6_mreq m;
    memset(&m, 0, sizeof m);
    memcpy(&m.ipv6mr_multiaddr, ip, 16);
    m.ipv6mr_interface = (unsigned)ifindex;
    return setsockopt((int)fd, IPPROTO_IPV6, join ? IPV6_JOIN_GROUP : IPV6_LEAVE_GROUP, &m, sizeof m) < 0 ? -errno : 0;
}

/* IP_MULTICAST_IF for IPv4: the interface's address (4 bytes). */
int64_t alx_net_mcast_if4(int64_t fd, const uint8_t *ifaddr) {
    struct in_addr a;
    memcpy(&a, ifaddr, 4);
    return setsockopt((int)fd, IPPROTO_IP, IP_MULTICAST_IF, &a, sizeof a) < 0 ? -errno : 0;
}

/* getaddrinfo(3) for a host: "ip" or "ip%scope" lines into out (n bytes);
 * the length, -ALX_ENOHOST for a name that doesn't exist, or -(ALX_ENOHOST
 * + 1) for another failure (the message from alx_net_gai_error). family: 0
 * any, 4, 6. Blocks the worker. */
int64_t alx_net_getaddrinfo(const char *host, int64_t family, uint8_t *out, int64_t n) {
    struct addrinfo hints, *res = NULL;
    memset(&hints, 0, sizeof hints);
    hints.ai_socktype = SOCK_STREAM;
    hints.ai_family = family == 4 ? AF_INET : family == 6 ? AF_INET6 : AF_UNSPEC;
    hints.ai_flags = AI_ADDRCONFIG;
    int rc = getaddrinfo(host, NULL, &hints, &res);
    if (rc == EAI_NONAME
#ifdef EAI_NODATA
        || rc == EAI_NODATA
#endif
        || (rc == EAI_FAIL)) return -ALX_ENOHOST;
    if (rc != 0) {
        /* AI_ADDRCONFIG fails on hosts with no configured address: retry without. */
        hints.ai_flags = 0;
        rc = getaddrinfo(host, NULL, &hints, &res);
        if (rc == EAI_NONAME) return -ALX_ENOHOST;
        if (rc != 0) return -(ALX_ENOHOST + 1);
    }
    int64_t len = 0;
    for (struct addrinfo *a = res; a; a = a->ai_next) {
        char h[INET6_ADDRSTRLEN + 16];
        if (a->ai_family == AF_INET6) {
            const struct sockaddr_in6 *s6 = (const void *)a->ai_addr;
            if (!inet_ntop(AF_INET6, &s6->sin6_addr, h, INET6_ADDRSTRLEN)) continue;
            if (s6->sin6_scope_id) snprintf(h + strlen(h), 16, "%%%u", (unsigned)s6->sin6_scope_id);
        } else if (a->ai_family == AF_INET) {
            if (!inet_ntop(AF_INET, &((const struct sockaddr_in *)(const void *)a->ai_addr)->sin_addr, h, INET6_ADDRSTRLEN)) continue;
        } else continue;
        size_t hl = strlen(h);
        if (len + (int64_t)hl + 1 > n) break;
        memcpy(out + len, h, hl);
        len += (int64_t)hl;
        out[len++] = '\n';
    }
    freeaddrinfo(res);
    return len;
}

/* The canonical name of host (getaddrinfo with AI_CANONNAME) into out:
 * its length, or as alx_net_getaddrinfo's failures. */
int64_t alx_net_canonname(const char *host, uint8_t *out, int64_t n) {
    struct addrinfo hints, *res = NULL;
    memset(&hints, 0, sizeof hints);
    hints.ai_socktype = SOCK_STREAM;
    hints.ai_flags = AI_CANONNAME;
    int rc = getaddrinfo(host, NULL, &hints, &res);
    if (rc == EAI_NONAME) return -ALX_ENOHOST;
    if (rc != 0) return -(ALX_ENOHOST + 1);
    int64_t len = 0;
    if (res && res->ai_canonname) {
        size_t cl = strlen(res->ai_canonname);
        if ((int64_t)cl > n) cl = (size_t)n;
        memcpy(out, res->ai_canonname, cl);
        len = (int64_t)cl;
    }
    freeaddrinfo(res);
    return len;
}

/* Reverse lookup (getnameinfo with NI_NAMEREQD) of the address sa into out:
 * the name's length, or -ALX_ENOHOST. Blocks the worker. */
int64_t alx_net_getnameinfo(const uint8_t *sa, uint8_t *out, int64_t n) {
    struct sockaddr_storage ss;
    socklen_t len;
    if (sa_from(sa, &ss, &len) < 0) return -ALX_ENOHOST;
    char h[NI_MAXHOST];
    int rc = getnameinfo((struct sockaddr *)&ss, len, h, sizeof h, NULL, 0, NI_NAMEREQD);
    if (rc != 0) return rc == EAI_NONAME ? -ALX_ENOHOST : -(ALX_ENOHOST + 1);
    size_t hl = strlen(h);
    if ((int64_t)hl > n) hl = (size_t)n;
    memcpy(out, h, hl);
    return (int64_t)hl;
}

/* The network interfaces, one per line: "index name flags mtu hwaddr", the
 * hardware address in hex ("" if none) and flags as Go's (1 up, 2
 * broadcast, 4 loopback, 8 point-to-point, 16 multicast, 32 running); then
 * their addresses: "@index family hexip prefixlen" lines. The length, or
 * -errno. */
#include <net/if.h>
#include <ifaddrs.h>
#include <sys/ioctl.h>
#ifdef __APPLE__
#  include <net/if_dl.h>
#endif
#ifdef __linux__
#  include <netpacket/packet.h>
#endif
int64_t alx_net_interfaces(uint8_t *out, int64_t n) {
    struct ifaddrs *ifs = NULL;
    if (getifaddrs(&ifs) < 0) return -errno;
    int64_t len = 0;
    char line[512];
    /* First pass: interfaces (one line each, at their first entry). */
    for (struct ifaddrs *a = ifs; a; a = a->ifa_next) {
        bool seen = false;
        for (struct ifaddrs *b = ifs; b != a; b = b->ifa_next)
            if (strcmp(b->ifa_name, a->ifa_name) == 0) { seen = true; break; }
        if (seen) continue;
        unsigned idx = if_nametoindex(a->ifa_name);
        unsigned fl = a->ifa_flags, gf = 0;
        if (fl & IFF_UP) gf |= 1;
        if (fl & IFF_BROADCAST) gf |= 2;
        if (fl & IFF_LOOPBACK) gf |= 4;
        if (fl & IFF_POINTOPOINT) gf |= 8;
        if (fl & IFF_MULTICAST) gf |= 16;
        if (fl & IFF_RUNNING) gf |= 32;
        int mtu = 0;
        int s = socket(AF_INET, SOCK_DGRAM, 0);
        if (s >= 0) {
            struct ifreq r;
            memset(&r, 0, sizeof r);
            snprintf(r.ifr_name, sizeof r.ifr_name, "%s", a->ifa_name);
            if (ioctl(s, SIOCGIFMTU, &r) == 0) mtu = r.ifr_mtu;
            close(s);
        }
        char hw[128] = "";
        for (struct ifaddrs *b = ifs; b; b = b->ifa_next) {
            if (strcmp(b->ifa_name, a->ifa_name) != 0 || !b->ifa_addr) continue;
            const unsigned char *p = NULL;
            int hl = 0;
#ifdef __APPLE__
            if (b->ifa_addr->sa_family == AF_LINK) {
                const struct sockaddr_dl *d = (const void *)b->ifa_addr;
                p = (const unsigned char *)LLADDR(d); hl = d->sdl_alen;
            }
#endif
#ifdef __linux__
            if (b->ifa_addr->sa_family == AF_PACKET) {
                const struct sockaddr_ll *d = (const void *)b->ifa_addr;
                p = d->sll_addr; hl = d->sll_halen;
            }
#endif
            if (p && hl > 0 && hl <= 32) {
                bool zero = true;
                for (int i = 0; i < hl; i++) if (p[i]) zero = false;
                if (zero && (fl & IFF_LOOPBACK)) break;
                for (int i = 0; i < hl; i++) snprintf(hw + 2 * i, 3, "%02x", p[i]);
                break;
            }
        }
        int k = snprintf(line, sizeof line, "%u %s %u %d %s\n", idx, a->ifa_name, gf, mtu, hw);
        if (len + k > n) { freeifaddrs(ifs); return -ENOBUFS; }
        memcpy(out + len, line, (size_t)k);
        len += k;
    }
    for (struct ifaddrs *a = ifs; a; a = a->ifa_next) {
        if (!a->ifa_addr) continue;
        int f = a->ifa_addr->sa_family;
        if (f != AF_INET && f != AF_INET6) continue;
        const unsigned char *ip, *mk = NULL;
        int il = f == AF_INET ? 4 : 16;
        if (f == AF_INET) ip = (const void *)&((const struct sockaddr_in *)(const void *)a->ifa_addr)->sin_addr;
        else ip = (const void *)&((const struct sockaddr_in6 *)(const void *)a->ifa_addr)->sin6_addr;
        if (a->ifa_netmask) {
            if (f == AF_INET) mk = (const void *)&((const struct sockaddr_in *)(const void *)a->ifa_netmask)->sin_addr;
            else mk = (const void *)&((const struct sockaddr_in6 *)(const void *)a->ifa_netmask)->sin6_addr;
        }
        int ones = 0;
        if (mk) for (int i = 0; i < il; i++) for (int b = 7; b >= 0; b--) if (mk[i] >> b & 1) ones++;
        int k = snprintf(line, sizeof line, "@%u %d ", if_nametoindex(a->ifa_name), f == AF_INET ? 4 : 6);
        for (int i = 0; i < il; i++) k += snprintf(line + k, sizeof line - (size_t)k, "%02x", ip[i]);
        k += snprintf(line + k, sizeof line - (size_t)k, " %d\n", ones);
        if (len + k > n) { freeifaddrs(ifs); return -ENOBUFS; }
        memcpy(out + len, line, (size_t)k);
        len += k;
    }
    freeifaddrs(ifs);
    return len;
}

/* socketpair(2) (sotype as alx_net_socket's) into fds (two native int64s). */
int64_t alx_net_socketpair(int64_t sotype, uint8_t *fds) {
    sock_init();
    int t = sotype == 1 ? SOCK_STREAM : sotype == 2 ? SOCK_DGRAM : SOCK_SEQPACKET;
    int p[2];
    if (socketpair(AF_UNIX, t, 0, p) < 0) return -errno;
    sock_prep(p[0]); sock_prep(p[1]);
    int64_t v[2] = { p[0], p[1] };
    memcpy(fds, v, sizeof v);
    return 0;
}

/* dup(2) of a socket, prepared like alx_net_socket's (Go's FileConn and
 * File methods). */
int64_t alx_net_dup(int64_t fd, int64_t nonblock) {
    int c = fcntl((int)fd, F_DUPFD_CLOEXEC, 0);
    if (c < 0) return -errno;
    int fl = fcntl(c, F_GETFL, 0);
    if (fl >= 0) fcntl(c, F_SETFL, nonblock ? fl | O_NONBLOCK : fl & ~O_NONBLOCK);
    return c;
}
