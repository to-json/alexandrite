/* SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception */
#include "alx.h"

#include <errno.h>
#include <pthread.h>
#include <setjmp.h>
#include <unistd.h>

/* ---------- memory ---------- */

size_t alx_allocs;
bool alx_counting;
_Thread_local char *alx_bump_cur, *alx_bump_end;

static void alx_report(void) {
    if (alx_counting) fprintf(stderr, "alx-allocs: %zu\n", alx_allocs);
}

void alx_init(void) {
    alx_counting = getenv("ALX_COUNT_ALLOCS") != NULL;
    atexit(alx_report);
}

enum { ALX_CHUNK = 1 << 20 };

/* `n` is rounded to 16 already. */
void *alx_alloc_slow(size_t n) {
    if (n > ALX_CHUNK / 16) {
        void *p = malloc(n);
        if (!p) alx_panic("out of memory", "runtime");
        return p;
    }
    char *c = malloc(ALX_CHUNK);
    if (!c) alx_panic("out of memory", "runtime");
    alx_bump_cur = c + n;
    alx_bump_end = c + ALX_CHUNK;
    return c;
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

Arr_Str alx_str_split(AlxStr s, AlxStr sep) {
    Arr_Str a = Arr_Str_cap(16);
    if (sep.len == 0) alx_panic("`split` with an empty separator", "runtime");
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
    /* Ruby drops trailing empty fields. */
    while (a.len > 0 && a.ptr[a.len - 1].len == 0) a.len--;
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
 * One global mutex guards every channel and task; one global condition
 * variable is broadcast on every state change (simple, and it lets `select`
 * wait on several channels at once). `g_gen` counts broadcasts: a waiter that
 * wakes with an unchanged `g_gen` was woken spuriously. Deadlock detection:
 * `g_live` counts main plus unfinished tasks; `g_blocked` counts those
 * waiting since the last broadcast. When equal, nothing can ever wake. */

static pthread_mutex_t g_mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t g_cv = PTHREAD_COND_INITIALIZER;
static uint64_t g_gen;
static int64_t g_live = 1, g_blocked;

static void wake_all(void) {
    g_gen++;
    g_blocked = 0;
    pthread_cond_broadcast(&g_cv);
}

/* With g_mu held and the awaited condition false: sleep until the next
 * broadcast. False (still holding g_mu) if that can never come. */
static bool block_wait(void) {
    if (!tl_uncounted && ++g_blocked >= g_live) {
        g_blocked--;
        return false;
    }
    uint64_t g = g_gen;
    do pthread_cond_wait(&g_cv, &g_mu); while (g == g_gen);
    return true;
}

static _Noreturn void deadlock(void) {
    pthread_mutex_unlock(&g_mu);
    alx_panic("all tasks are asleep: deadlock", "runtime");
}

struct AlxTask {
    AlxWorker fn;
    void *env, *res;
    size_t out_size;
    bool finished, panicked;
    char *msg;
    jmp_buf jb;
};

static _Noreturn void task_panic(char *msg) {
    AlxTask *t = tl_task;
    t->msg = msg ? msg : (char *)"alexandrite: out of memory";
    _longjmp(t->jb, 1);
}

static void *task_main(void *arg) {
    AlxTask *t = arg;
    tl_task = t;
    if (_setjmp(t->jb) == 0) t->fn(t->env, t->res);
    else t->panicked = true;
    tl_task = NULL;
    pthread_mutex_lock(&g_mu);
    t->finished = true;
    g_live--;
    wake_all();
    pthread_mutex_unlock(&g_mu);
    return NULL;
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
    pthread_mutex_lock(&g_mu);
    g_live++;
    pthread_mutex_unlock(&g_mu);
    pthread_attr_t at;
    pthread_attr_init(&at);
    pthread_attr_setdetachstate(&at, PTHREAD_CREATE_DETACHED);
    pthread_attr_setstacksize(&at, (size_t)8 << 20);
    pthread_t th;
    int rc = pthread_create(&th, &at, task_main, t);
    pthread_attr_destroy(&at);
    if (rc) {
        pthread_mutex_lock(&g_mu);
        g_live--;
        pthread_mutex_unlock(&g_mu);
        alx_panic("cannot start a task", "runtime");
    }
    return t;
}

bool alx_task_wait(AlxTask *t, void *out, AlxStr *msg) {
    pthread_mutex_lock(&g_mu);
    while (!t->finished)
        if (!block_wait()) deadlock();
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
            wake_all();
            if (c->cap > 0) {
                pthread_mutex_unlock(&g_mu);
                return;
            }
            /* Rendezvous: wait until a receiver took it. */
            uint64_t ticket = c->send_seq;
            while (c->taken_seq < ticket && !c->closed)
                if (!block_wait()) deadlock();
            if (c->taken_seq >= ticket) {
                pthread_mutex_unlock(&g_mu);
                return;
            }
            break; /* closed while waiting: close dropped the value */
        }
        if (!block_wait()) deadlock();
    }
    pthread_mutex_unlock(&g_mu);
    alx_panic("send on a closed channel", loc);
}

bool alx_chan_recv(AlxChan *c, void *out) {
    pthread_mutex_lock(&g_mu);
    for (;;) {
        if (try_take(c, out)) {
            wake_all();
            pthread_mutex_unlock(&g_mu);
            return true;
        }
        if (c->closed) {
            pthread_mutex_unlock(&g_mu);
            return false;
        }
        c->recv_waiting++;
        if (c->send_waiting) wake_all(); /* a blocked select-sender may now proceed */
        bool ok = block_wait();
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
    wake_all();
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
                    wake_all();
                    pthread_mutex_unlock(&g_mu);
                    return i;
                }
            } else if (try_take(s->ch, s->buf)) {
                s->ok = 1;
                wake_all();
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
        bool wake = false;
        for (int64_t i = 0; i < n; i++) {
            if (cs[i].is_send) {
                cs[i].ch->send_waiting++;
            } else {
                cs[i].ch->recv_waiting++;
                wake |= cs[i].ch->send_waiting > 0;
            }
        }
        if (wake) wake_all();
        bool ok = block_wait();
        for (int64_t i = 0; i < n; i++) {
            if (cs[i].is_send) cs[i].ch->send_waiting--;
            else cs[i].ch->recv_waiting--;
        }
        if (!ok) deadlock();
    }
}
