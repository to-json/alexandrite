/* SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception */
#include "alx.h"

#include <errno.h>
#include <pthread.h>
#include <unistd.h>

/* ---------- memory ---------- */

static size_t alx_allocs;

static void alx_report(void) {
    if (getenv("ALX_COUNT_ALLOCS")) fprintf(stderr, "alx-allocs: %zu\n", alx_allocs);
}

void alx_init(void) { atexit(alx_report); }

void *alx_alloc(size_t bytes) {
    void *p = malloc(bytes ? bytes : 1);
    if (!p) alx_panic("out of memory", "runtime");
    __atomic_fetch_add(&alx_allocs, 1, __ATOMIC_RELAXED);
    return p;
}

/* ---------- panics and errors ---------- */

void alx_panic(const char *what, const char *loc) {
    fflush(stdout);
    fprintf(stderr, "alexandrite: %s at %s\n", what, loc);
    abort();
}

void alx_overflow(const char *loc) {
    fflush(stdout);
    fprintf(stderr, "alexandrite: overflow at %s\nhint: add `#![overflow(promote)]` to this file to promote to bignums\n", loc);
    abort();
}

AlxErr alx_err_overflow(const char *loc) {
    AlxErr e = { ALX_ERR_OVERFLOW, { "", 0 }, loc };
    return e;
}

void alx_die(AlxErr e) {
    fflush(stdout);
    switch (e.tag) {
    case ALX_ERR_NOT_FOUND:
        fprintf(stderr, "File.read: no such file `%.*s` (%s)\n", (int)e.detail.len, e.detail.ptr, e.loc);
        break;
    case ALX_ERR_IO:
        fprintf(stderr, "File.read: cannot read `%.*s` (%s)\n", (int)e.detail.len, e.detail.ptr, e.loc);
        break;
    case ALX_ERR_OVERFLOW:
        fprintf(stderr, "error: overflow (%s)\n", e.loc);
        break;
    default:
        fprintf(stderr, "error (%s)\n", e.loc);
    }
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

AlxStr alx_int_to_s(int64_t v) {
    char buf[24];
    int n = snprintf(buf, sizeof buf, "%lld", (long long)v);
    char *p = alx_alloc((size_t)n);
    memcpy(p, buf, (size_t)n);
    AlxStr s = { p, n };
    return s;
}

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

bool alx_file_read(AlxStr path, const char *loc, AlxStr *out, AlxErr *err) {
    char *cpath = alx_alloc((size_t)path.len + 1);
    memcpy(cpath, path.ptr, (size_t)path.len);
    cpath[path.len] = 0;
    FILE *f = fopen(cpath, "rb");
    if (!f) {
        err->tag = errno == ENOENT ? ALX_ERR_NOT_FOUND : ALX_ERR_IO;
        err->detail = path;
        err->loc = loc;
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
    AlxErr err;
    bool ok;
} PmapJob;

static void *pmap_run(void *arg) {
    PmapJob *j = arg;
    j->ok = true;
    for (int64_t i = j->lo; i < j->hi; i++) {
        if (!j->fn(j->in + (size_t)i * j->in_size, j->out + (size_t)i * j->out_size, &j->err)) {
            j->ok = false;
            break;
        }
    }
    return NULL;
}

bool alx_pmap(const void *in, int64_t n, size_t in_size, void *out, size_t out_size, AlxWorker fn, AlxErr *err) {
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
        jobs[t] = (PmapJob){ in, out, lo, hi, in_size, out_size, fn, { 0 }, true };
        if (t > 0) pthread_create(&tids[t], NULL, pmap_run, &jobs[t]);
    }
    pmap_run(&jobs[0]);
    for (int64_t t = 1; t < threads; t++) pthread_join(tids[t], NULL);
    /* Report the error of the lowest-numbered failing element's chunk. */
    for (int64_t t = 0; t < threads; t++) {
        if (!jobs[t].ok) {
            *err = jobs[t].err;
            return false;
        }
    }
    return true;
}
