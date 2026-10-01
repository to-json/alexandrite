/* SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception */
#include "alx.h"

#include <errno.h>
#include <pthread.h>
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
