/* SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception */
/* Entry points for the JIT behind `alx run`. Only int64_t and pointer
 * arguments and results: aggregates are passed by pointer, so the JIT never
 * depends on how the platform ABI passes structs. */
#include "alx.h"

typedef struct { void *ptr; int64_t len; int64_t cap; } AnyArr;

void *alxj_alloc(int64_t n) { return alx_alloc((size_t)n); }

void *alxj_zalloc(int64_t n) {
    void *p = alx_alloc((size_t)n);
    memset(p, 0, (size_t)n);
    return p;
}

/* Storage for `n` elements of `esz` bytes (NULL when n <= 0). */
void *alxj_arr_alloc(int64_t n, int64_t esz) { return n > 0 ? alx_alloc((size_t)(n * esz)) : NULL; }

/* Grow to `cap` elements, keeping the first `len`; returns the new storage. */
void *alxj_arr_grow(void *ptr, int64_t len, int64_t cap, int64_t esz) {
    void *np = alx_alloc((size_t)(cap * esz));
    if (len) memcpy(np, ptr, (size_t)(len * esz));
    return np;
}

void *alxj_arr_new(int64_t n, const void *fill, int64_t esz, const char *loc) {
    if (n < 0) alx_panic("negative array size", loc);
    char *p = alxj_arr_alloc(n, esz);
    if (esz == 1) {
        if (n) memset(p, *(const unsigned char *)fill, (size_t)n);
    } else {
        for (int64_t i = 0; i < n; i++) memcpy(p + i * esz, fill, (size_t)esz);
    }
    return p;
}

void alxj_arr_copy(AnyArr *out, const AnyArr *a, int64_t esz) {
    out->ptr = alx_alloc((size_t)((a->len ? a->len : 1) * esz));
    if (a->len) memcpy(out->ptr, a->ptr, (size_t)(a->len * esz));
    out->len = a->len;
    out->cap = a->len;
}

/* ---------- strings ---------- */
void alxj_int_to_s(AlxStr *out, int64_t v) { *out = alx_int_to_s(v); }
void alxj_str_rev(AlxStr *out, const AlxStr *s) { *out = alx_str_rev(*s); }
void alxj_str_delete(AlxStr *out, const AlxStr *s, const AlxStr *chars) { *out = alx_str_delete(*s, *chars); }
void alxj_str_split(Arr_Str *out, const AlxStr *s, const AlxStr *sep) { *out = alx_str_split(*s, *sep); }
int64_t alxj_str_to_i(const AlxStr *s) { return alx_str_to_i(*s); }
int64_t alxj_str_charlen(const AlxStr *s, int64_t i) { return alx_str_charlen(*s, i); }
void alxj_str_sub(AlxStr *out, const AlxStr *s, int64_t i, int64_t n) { *out = alx_str_sub(*s, i, n); }
int64_t alxj_str_eq(const AlxStr *a, const AlxStr *b) { return alx_str_eq(*a, *b); }
int64_t alxj_str_cmp(const AlxStr *a, const AlxStr *b) { return alx_str_cmp(*a, *b); }
int64_t alxj_str_is_pal(const AlxStr *s) { return alx_str_is_pal(*s); }
int64_t alxj_int_ndigits(int64_t v) { return alx_int_ndigits(v); }
void alxj_digits(Arr_I64 *out, int64_t v, const char *loc) { *out = alx_digits(v, loc); }

/* ---------- checked arithmetic that isn't inlined ---------- */
int64_t alxj_try_pow(int64_t a, int64_t b, int64_t *r) { return alx_try_pow(a, b, r); }

/* ---------- errors ---------- */
void alxj_die_str(const AlxStr *s) { alx_die_str(*s); }
int64_t alxj_file_status(const AlxStr *path) { return alx_file_status(*path); }
void alxj_file_read_or_empty(AlxStr *out, const AlxStr *path) { *out = alx_file_read_or_empty(*path); }

/* ---------- output, sorting, pmap ---------- */
void alxj_puts_str(const AlxStr *s) { alx_puts_str(*s); }
void alxj_puts_bool(int64_t b) { alx_puts_bool(b != 0); }
void alxj_puts_unit(void) { puts(""); }
void alxj_pmap(const void *in, int64_t n, int64_t in_size, void *out, int64_t out_size, AlxWorker fn) {
    alx_pmap(in, n, (size_t)in_size, out, (size_t)out_size, fn);
}
void alxj_finish(void) { fflush(stdout); }

/* ---------- bignums (promote mode) ---------- */
void alxj_p_add(AlxPInt *o, const AlxPInt *a, const AlxPInt *b) { *o = alx_p_add(*a, *b); }
void alxj_p_sub(AlxPInt *o, const AlxPInt *a, const AlxPInt *b) { *o = alx_p_sub(*a, *b); }
void alxj_p_mul(AlxPInt *o, const AlxPInt *a, const AlxPInt *b) { *o = alx_p_mul(*a, *b); }
void alxj_p_div(AlxPInt *o, const AlxPInt *a, const AlxPInt *b, const char *loc) { *o = alx_p_div(*a, *b, loc); }
void alxj_p_rem(AlxPInt *o, const AlxPInt *a, const AlxPInt *b, const char *loc) { *o = alx_p_rem(*a, *b, loc); }
void alxj_p_pow(AlxPInt *o, const AlxPInt *a, const AlxPInt *b, const char *loc) { *o = alx_p_pow(*a, *b, loc); }
int64_t alxj_p_cmp(const AlxPInt *a, const AlxPInt *b) { return alx_p_cmp(*a, *b); }
int64_t alxj_p_even(const AlxPInt *a) { return alx_p_even(*a); }
int64_t alxj_p_to_i64(const AlxPInt *a, const char *loc) { return alx_p_to_i64(*a, loc); }
void alxj_p_to_s(AlxStr *out, const AlxPInt *a) { *out = alx_p_to_s(*a); }
int64_t alxj_p_ndigits(const AlxPInt *a) { return alx_p_ndigits(*a); }
void alxj_p_digits(Arr_PInt *out, const AlxPInt *a, const char *loc) { *out = alx_p_digits(*a, loc); }
void alxj_puts_pint(const AlxPInt *a) { alx_puts_pint(*a); }

/* ---------- floats ---------- */
void alxj_puts_f64(double x) { alx_puts_f64(x); }
void alxj_f_to_s(AlxStr *out, double x) { *out = alx_f_to_s(x); }
void alxj_f_fmt(AlxStr *out, double x, int64_t digits) { *out = alx_f_fmt(x, digits); }
int64_t alxj_f_to_i(double x, const char *loc) { return alx_f_to_i(x, loc); }
void alxj_str_cat(AlxStr *out, const AlxStr *parts, int64_t n) { *out = alx_str_cat(n, parts); }

/* ---------- sized integers ---------- */
void alxj_puts_u64(int64_t v) { alx_puts_u64(v); }
void alxj_u64_to_s(AlxStr *out, int64_t v) { *out = alx_u64_to_s(v); }
void alxj_int_fmt(AlxStr *out, int64_t v, int64_t base, int64_t upper, int64_t uns) { *out = alx_int_fmt(v, base, upper != 0, uns != 0); }
int64_t alxj_f_to_u64(double x, const char *loc) { return alx_f_to_u64(x, loc); }
void alxj_rune_to_s(AlxStr *out, int64_t r) { *out = alx_rune_to_s(r); }
void alxj_str_from_bytes(AlxStr *out, const uint8_t *p, int64_t n) { *out = alx_str_from_bytes(p, n); }

/* ---------- tasks and channels ---------- */
void *alxj_spawn(AlxWorker fn, const void *env, int64_t in_size, int64_t out_size) { return alx_spawn(fn, env, (size_t)in_size, (size_t)out_size); }
int64_t alxj_task_wait(AlxTask *t, void *out, AlxStr *msg) { return alx_task_wait(t, out, msg); }
void *alxj_chan_new(int64_t cap, int64_t esz) { return alx_chan_new(cap, (size_t)esz); }
int64_t alxj_chan_len(AlxChan *c) { return alx_chan_len(c); }
void alxj_chan_send(AlxChan *c, const void *val, const char *loc) { alx_chan_send(c, val, loc); }
int64_t alxj_chan_recv(AlxChan *c, void *out) { return alx_chan_recv(c, out); }
void alxj_chan_close(AlxChan *c, const char *loc) { alx_chan_close(c, loc); }
int64_t alxj_select(AlxSelCase *cases, int64_t n, int64_t has_default, const char *loc) { return alx_select(cases, n, has_default != 0, loc); }
