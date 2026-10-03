/* SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception */
/* Promote-mode Int: values that fit in 64 bits stay unboxed (`big == NULL`);
 * on overflow the operation is redone with libtommath (public domain). */
#include "alx.h"
#include "tommath_amalgam.c"

struct AlxBig { mp_int m; };

static void check(mp_err e) {
    if (e != MP_OKAY) alx_panic("bignum operation failed", "runtime");
}

static AlxPInt normalize(AlxBig *b) {
    AlxPInt r = { 0, NULL };
    if (mp_count_bits(&b->m) <= 63) {
        r.v = mp_get_i64(&b->m);
        return r;
    }
    r.big = b;
    return r;
}

static AlxBig *new_big(void) {
    AlxBig *b = alx_alloc(sizeof *b);
    check(mp_init(&b->m));
    return b;
}

/* Operand as an mp_int: borrowed if big, a fresh temporary if small. */
static const mp_int *as_mp(AlxPInt a, mp_int *tmp) {
    if (a.big) return &a.big->m;
    check(mp_init(tmp));
    mp_set_i64(tmp, a.v);
    return tmp;
}

static void release(AlxPInt a, mp_int *tmp) {
    if (!a.big) mp_clear(tmp);
}

AlxPInt alx_p_from(int64_t v) {
    AlxPInt r = { v, NULL };
    return r;
}

int64_t alx_p_to_i64(AlxPInt a, const char *loc) {
    if (a.big) alx_overflow(loc);
    return a.v;
}

typedef mp_err (*BinFn)(const mp_int *, const mp_int *, mp_int *);

static AlxPInt slow(AlxPInt a, AlxPInt b, BinFn f) {
    mp_int ta, tb;
    const mp_int *x = as_mp(a, &ta), *y = as_mp(b, &tb);
    AlxBig *r = new_big();
    check(f(x, y, &r->m));
    release(a, &ta);
    release(b, &tb);
    return normalize(r);
}

AlxPInt alx_p_add(AlxPInt a, AlxPInt b) {
    int64_t r;
    if (!a.big && !b.big && !__builtin_add_overflow(a.v, b.v, &r)) return alx_p_from(r);
    return slow(a, b, mp_add);
}

AlxPInt alx_p_sub(AlxPInt a, AlxPInt b) {
    int64_t r;
    if (!a.big && !b.big && !__builtin_sub_overflow(a.v, b.v, &r)) return alx_p_from(r);
    return slow(a, b, mp_sub);
}

AlxPInt alx_p_mul(AlxPInt a, AlxPInt b) {
    int64_t r;
    if (!a.big && !b.big && !__builtin_mul_overflow(a.v, b.v, &r)) return alx_p_from(r);
    return slow(a, b, mp_mul);
}

int alx_p_cmp(AlxPInt a, AlxPInt b) {
    if (!a.big && !b.big) return (a.v > b.v) - (a.v < b.v);
    mp_int ta, tb;
    const mp_int *x = as_mp(a, &ta), *y = as_mp(b, &tb);
    mp_ord o = mp_cmp(x, y);
    release(a, &ta);
    release(b, &tb);
    return o == MP_LT ? -1 : o == MP_GT ? 1 : 0;
}

/* Truncating division and remainder (Go semantics). */
static void divmod(AlxPInt a, AlxPInt b, const char *loc, AlxPInt *q, AlxPInt *m) {
    if (!b.big && b.v == 0) alx_panic("division by zero", loc);
    if (!a.big && !b.big && !(a.v == INT64_MIN && b.v == -1)) {
        *q = alx_p_from(a.v / b.v);
        *m = alx_p_from(a.v % b.v);
        return;
    }
    mp_int ta, tb;
    const mp_int *x = as_mp(a, &ta), *y = as_mp(b, &tb);
    AlxBig *bq = new_big(), *br = new_big();
    check(mp_div(x, y, &bq->m, &br->m)); /* truncating */
    release(a, &ta);
    release(b, &tb);
    *q = normalize(bq);
    *m = normalize(br);
}

AlxPInt alx_p_div(AlxPInt a, AlxPInt b, const char *loc) {
    AlxPInt q, m;
    divmod(a, b, loc, &q, &m);
    return q;
}

AlxPInt alx_p_rem(AlxPInt a, AlxPInt b, const char *loc) {
    AlxPInt q, m;
    divmod(a, b, loc, &q, &m);
    return m;
}

AlxPInt alx_p_pow(AlxPInt a, AlxPInt b, const char *loc) {
    if (b.big || b.v < 0 || b.v > INT32_MAX) alx_panic("exponent out of range", loc);
    int64_t r;
    if (!a.big && alx_try_pow(a.v, b.v, &r)) return alx_p_from(r);
    mp_int ta;
    const mp_int *x = as_mp(a, &ta);
    AlxBig *res = new_big();
    check(mp_expt_n(x, (int)b.v, &res->m));
    release(a, &ta);
    return normalize(res);
}

bool alx_p_even(AlxPInt a) {
    if (!a.big) return (a.v & 1) == 0;
    return mp_iseven(&a.big->m);
}

/* Str#to_i in promote mode: Ruby's rules (leading blanks, a sign, digits up
 * to the first non-digit; none is 0), any number of digits. */
AlxPInt alx_p_from_str(AlxStr s) {
    int64_t i = 0;
    while (i < s.len && (s.ptr[i] == ' ' || s.ptr[i] == '\t' || s.ptr[i] == '\n')) i++;
    bool neg = false;
    if (i < s.len && (s.ptr[i] == '-' || s.ptr[i] == '+')) neg = s.ptr[i++] == '-';
    int64_t start = i;
    while (i < s.len && s.ptr[i] >= '0' && s.ptr[i] <= '9') i++;
    if (i - start <= 18) {
        int64_t v = 0;
        for (int64_t k = start; k < i; k++) v = v * 10 + (s.ptr[k] - '0');
        return alx_p_from(neg ? -v : v);
    }
    char *buf = malloc((size_t)(i - start) + 2);
    if (!buf) alx_panic("out of memory", "runtime");
    size_t o = 0;
    if (neg) buf[o++] = '-';
    memcpy(buf + o, s.ptr + start, (size_t)(i - start));
    buf[o + (size_t)(i - start)] = 0;
    AlxBig *b = new_big();
    check(mp_read_radix(&b->m, buf, 10));
    free(buf);
    return normalize(b);
}

AlxStr alx_p_to_s(AlxPInt a) {
    if (!a.big) return alx_int_to_s(a.v);
    int size = 0;
    check(mp_radix_size(&a.big->m, 10, &size));
    char *p = alx_alloc((size_t)size);
    size_t written = 0;
    check(mp_to_radix(&a.big->m, p, (size_t)size, &written, 10));
    AlxStr s = { p, (int64_t)written - 1 };
    return s;
}

/* 10**k, cached per thread. */
static _Thread_local mp_int **p10;
static _Thread_local int64_t p10_cap;

static const mp_int *pow10(int64_t k) {
    if (k >= p10_cap) {
        int64_t nc = p10_cap ? p10_cap : 64;
        while (nc <= k) nc *= 2;
        mp_int **t = calloc((size_t)nc, sizeof *t);
        if (!t) alx_panic("out of memory", "runtime");
        if (p10_cap) memcpy(t, p10, (size_t)p10_cap * sizeof *t);
        free(p10);
        p10 = t;
        p10_cap = nc;
    }
    if (!p10[k]) {
        mp_int *m = malloc(sizeof *m);
        if (!m) alx_panic("out of memory", "runtime");
        check(mp_init(m));
        if (k > 0 && p10[k - 1]) check(mp_mul_d(p10[k - 1], 10, m));
        else { mp_set_i64(m, 10); check(mp_expt_n(m, (int)k, m)); }
        p10[k] = m;
    }
    return p10[k];
}

/* `a.to_s.size` without converting: 10**(d-1) <= |a| < 10**d. */
int64_t alx_p_ndigits(AlxPInt a) {
    if (!a.big) return alx_int_ndigits(a.v);
    const mp_int *m = &a.big->m;
    int64_t d = (int64_t)((double)(mp_count_bits(m) - 1) * 0.30102999566398119521) + 1;
    while (d > 1 && mp_cmp_mag(m, pow10(d - 1)) == MP_LT) d--;
    while (mp_cmp_mag(m, pow10(d)) != MP_LT) d++;
    return d + (mp_isneg(m) ? 1 : 0);
}

Arr_PInt alx_p_digits(AlxPInt a, const char *loc) {
    if ((!a.big && a.v < 0) || (a.big && mp_isneg(&a.big->m))) alx_panic("`digits` of a negative number", loc);
    AlxStr s = alx_p_to_s(a);
    Arr_PInt d = Arr_PInt_cap(s.len);
    for (int64_t i = s.len - 1; i >= 0; i--) Arr_PInt_push(&d, alx_p_from(s.ptr[i] - '0'));
    return d;
}

void alx_puts_pint(AlxPInt v) { alx_puts_str(alx_p_to_s(v)); }
