/* Probe 11: error sets in C. Every tag in the program gets a global number,
 * and there is ONE error type: tag + small payload union. Set membership is
 * a compile-time fact owned by our checker, so widening a set across `try`
 * is a plain copy (same type), and exhaustiveness is checked by us; C gets
 * an aborting `default:` as a backstop. */
#include "../10_c/alx_rt.h"

typedef struct { const char *ptr; size_t len; } Str;

typedef enum {
    ERR_BadDigit = 1, ERR_Empty, ERR_Overflow, /* ParseError + builtin */
    ERR_NotFound, ERR_Denied,                    /* IoError */
    ERR_Missing, ERR_Invalid,                    /* ConfigError */
} ErrTag;

typedef struct {
    uint16_t tag;
    union { uint8_t bad_digit; Str not_found; Str missing; } p;
} Err;

/* `T!` lowers to a struct; on error, `val` is unreadable (DESIGN: destination
 * after an error). */
typedef struct { Err err; int64_t val; } ResInt;
typedef struct { Err err; Str val; } ResStr;
#define OK(T, v) ((T){ .err = { 0 }, .val = (v) })
#define FAIL(T, ...) ((T){ .err = { __VA_ARGS__ } })
/* `try e`: propagate (widen = same type, copy) or yield the value. */
#define TRY(T, lhs, expr) do { __typeof__(expr) r_ = (expr); \
    if (r_.err.tag) return (T){ .err = r_.err }; lhs = r_.val; } while (0)

static ResInt digit(uint8_t b) {
    if (b < '0' || b > '9') return FAIL(ResInt, .tag = ERR_BadDigit, .p.bad_digit = b);
    return OK(ResInt, b - '0');
}

static ResInt parse(Str s) {
    if (s.len == 0) return FAIL(ResInt, .tag = ERR_Empty);
    int64_t acc = 0;
    for (size_t i = 0; i < s.len; i++) {
        int64_t d;
        TRY(ResInt, d, digit((uint8_t)s.ptr[i]));
        if (!ALX_MUL(acc, 10, &acc) || !ALX_ADD(acc, d, &acc))
            return FAIL(ResInt, .tag = ERR_Overflow);
    }
    return OK(ResInt, acc);
}

static ResStr read(Str path) {
    if (path.len && path.ptr[0] == '/') return FAIL(ResStr, .tag = ERR_Denied);
    if (path.len == 7 && !memcmp(path.ptr, "missing", 7))
        return FAIL(ResStr, .tag = ERR_NotFound, .p.not_found = path);
    return OK(ResStr, path); /* the "file contents" are the path, for the probe */
}

static ResInt load(Str path) {
    Str text;
    TRY(ResInt, text, read(path));
    return parse(text);
}

/* config: rescue maps everything into ConfigError. */
static ResInt config(Str p) {
    ResInt r = load(p);
    if (!r.err.tag) return r;
    switch ((ErrTag)r.err.tag) {
    case ERR_NotFound: return FAIL(ResInt, .tag = ERR_Missing, .p.missing = r.err.p.not_found);
    default:           return FAIL(ResInt, .tag = ERR_Invalid);
    }
}

static const char *name(uint16_t t) {
    switch ((ErrTag)t) {
    case ERR_BadDigit: return "BadDigit"; case ERR_Empty: return "Empty";
    case ERR_Overflow: return "Overflow"; case ERR_NotFound: return "NotFound";
    case ERR_Denied: return "Denied"; case ERR_Missing: return "Missing";
    case ERR_Invalid: return "Invalid";
    }
    ALX_PANIC("unknown error tag"); /* backstop: our checker proves this unreachable */
}

static Str S(const char *c) { return (Str){ c, strlen(c) }; }

int main(void) {
    const char *in[] = { "123", "12x", "", "99999999999999999999", "missing", "/etc" };
    printf("sizeof(Err)=%zu sizeof(ResInt)=%zu\n", sizeof(Err), sizeof(ResInt));
    for (size_t i = 0; i < sizeof in / sizeof *in; i++) {
        ResInt l = load(S(in[i])), c = config(S(in[i]));
        printf("%-22s load -> ", in[i][0] ? in[i] : "\"\"");
        if (l.err.tag) printf("%-9s", name(l.err.tag)); else printf("%-9lld", (long long)l.val);
        printf(" config -> ");
        if (c.err.tag) printf("%s", name(c.err.tag)); else printf("%lld", (long long)c.val);
        if (c.err.tag == ERR_Missing) printf("(%.*s)", (int)c.err.p.missing.len, c.err.p.missing.ptr);
        if (l.err.tag == ERR_BadDigit) printf("  [payload '%c']", l.err.p.bad_digit);
        printf("\n");
    }
    return 0;
}
