// External scanner for the alexandrite grammar: the context-sensitive tokens.
// See the header of grammar.js for what each one decides.

#include "tree_sitter/parser.h"

#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

enum TokenType {
  NEWLINE,
  SUFFIXED_IDENTIFIER,
  BLOCK_LBRACE,
  BODY_LBRACE,
  UNARY_MINUS,
  BINARY_MINUS,
  UNARY_CARET,
  BINARY_CARET,
  HEREDOC_START,
  HEREDOC_BODY_START,
  HEREDOC_CONTENT,
  HEREDOC_ESCAPE,
  HEREDOC_END,
  ERROR_SENTINEL,
};

#define MAX_ID 64
#define MAX_HEREDOCS 16

typedef struct {
  char id[MAX_ID];
  uint8_t len;
  bool raw;
} Heredoc;

// Heredocs started on the current line wait in `pending` until its newline;
// the one whose body is being read is `active`.
typedef struct {
  Heredoc pending[MAX_HEREDOCS];
  uint8_t npending;
  Heredoc active;
  bool has_active;
} Scanner;

static inline void advance(TSLexer *lexer) { lexer->advance(lexer, false); }
static inline void skip(TSLexer *lexer) { lexer->advance(lexer, true); }

static inline bool is_word_start(int32_t c) { return (c >= 'a' && c <= 'z') || c == '_'; }
static inline bool is_word(int32_t c) {
  return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '_';
}
static inline bool is_blank(int32_t c) { return c == ' ' || c == '\t' || c == '\r' || c == '\f'; }
static inline bool is_space(int32_t c) { return is_blank(c) || c == '\n'; }

void *tree_sitter_alexandrite_external_scanner_create(void) { return calloc(1, sizeof(Scanner)); }

void tree_sitter_alexandrite_external_scanner_destroy(void *payload) { free(payload); }

static unsigned put_heredoc(char *buf, unsigned n, const Heredoc *h) {
  buf[n++] = (char)h->raw;
  buf[n++] = (char)h->len;
  memcpy(buf + n, h->id, h->len);
  return n + h->len;
}

static unsigned get_heredoc(const char *buf, unsigned n, Heredoc *h) {
  h->raw = buf[n++];
  h->len = (uint8_t)buf[n++];
  memcpy(h->id, buf + n, h->len);
  return n + h->len;
}

unsigned tree_sitter_alexandrite_external_scanner_serialize(void *payload, char *buffer) {
  Scanner *s = (Scanner *)payload;
  unsigned n = 0;
  buffer[n++] = (char)s->has_active;
  if (s->has_active) n = put_heredoc(buffer, n, &s->active);
  buffer[n++] = (char)s->npending;
  for (unsigned i = 0; i < s->npending; i++) n = put_heredoc(buffer, n, &s->pending[i]);
  return n;
}

void tree_sitter_alexandrite_external_scanner_deserialize(void *payload, const char *buffer, unsigned length) {
  Scanner *s = (Scanner *)payload;
  s->has_active = false;
  s->npending = 0;
  if (length == 0) return;
  unsigned n = 0;
  s->has_active = buffer[n++];
  if (s->has_active) n = get_heredoc(buffer, n, &s->active);
  s->npending = (uint8_t)buffer[n++];
  for (unsigned i = 0; i < s->npending; i++) n = get_heredoc(buffer, n, &s->pending[i]);
}

// After a newline: does the next code line continue the statement? A line
// starting with `.` / `?.` (a leading-dot chain, T10; comment lines may sit
// between) or `else` / `elsif` (the compiler skips newlines before them).
// Consumes lookahead only (the caller has marked the token end).
static bool continues(TSLexer *lexer) {
  for (;;) {
    while (is_space(lexer->lookahead)) advance(lexer);
    if (lexer->lookahead == '#') {
      advance(lexer);
      if (lexer->lookahead == '[' || lexer->lookahead == '!') return false;
      while (lexer->lookahead != '\n' && !lexer->eof(lexer)) advance(lexer);
      continue;
    }
    break;
  }
  int32_t c = lexer->lookahead;
  if (c == '.') {
    advance(lexer);
    return lexer->lookahead != '.';
  }
  if (c == '?') {
    advance(lexer);
    return lexer->lookahead == '.';
  }
  if (c == 'e') {
    const char *w = "else";
    for (int i = 0; w[i]; i++) {
      if (lexer->lookahead != w[i]) return false;
      advance(lexer);
    }
    if (lexer->lookahead == 'i') {
      advance(lexer);
      if (lexer->lookahead != 'f') return false;
      advance(lexer);
      return !is_word(lexer->lookahead);
    }
    if (is_word(lexer->lookahead)) return false;
    // `else =>` is a select arm, not an else clause.
    while (is_blank(lexer->lookahead)) advance(lexer);
    return lexer->lookahead != '=';
  }
  return false;
}

// The text of the active heredoc, from the current position.
static bool scan_heredoc_body(Scanner *s, TSLexer *lexer, const bool *valid) {
  bool has_content = false;
  bool line_start = lexer->get_column(lexer) == 0;
  for (;;) {
    if (line_start) {
      line_start = false;
      // The terminator line: the id alone, indent and trailing blanks allowed.
      lexer->mark_end(lexer);
      bool indented = false;
      while (is_blank(lexer->lookahead)) {
        advance(lexer);
        indented = true;
      }
      unsigned i = 0;
      while (i < s->active.len && lexer->lookahead == (unsigned char)s->active.id[i]) {
        advance(lexer);
        i++;
      }
      if (i == s->active.len && !is_word(lexer->lookahead)) {
        while (is_blank(lexer->lookahead)) advance(lexer);
        if (lexer->lookahead == '\n' || lexer->eof(lexer)) {
          if (has_content) {
            lexer->result_symbol = HEREDOC_CONTENT;
            return true;
          }
          if (!valid[HEREDOC_END]) return false;
          lexer->mark_end(lexer);
          s->has_active = false;
          lexer->result_symbol = HEREDOC_END;
          return true;
        }
      }
      if (i > 0 || indented) has_content = true;
      continue;
    }
    if (lexer->eof(lexer)) {
      if (!has_content) return false;
      lexer->mark_end(lexer);
      lexer->result_symbol = HEREDOC_CONTENT;
      return true;
    }
    int32_t c = lexer->lookahead;
    if (c == '\n') {
      advance(lexer);
      has_content = true;
      line_start = true;
      continue;
    }
    if (!s->active.raw && c == '#') {
      lexer->mark_end(lexer);
      advance(lexer);
      if (lexer->lookahead == '{') {
        // `#{`: the grammar's interpolation takes it from here.
        if (!has_content) return false;
        lexer->result_symbol = HEREDOC_CONTENT;
        return true;
      }
      has_content = true;
      continue;
    }
    if (!s->active.raw && c == '\\') {
      if (has_content) {
        lexer->mark_end(lexer);
        lexer->result_symbol = HEREDOC_CONTENT;
        return true;
      }
      advance(lexer);
      int32_t e = lexer->lookahead;
      int digits = e == 'x' ? 2 : e == 'u' ? 4 : e == 'U' ? 8 : 0;
      if (!lexer->eof(lexer)) advance(lexer);
      for (int k = 0; k < digits; k++) {
        int32_t h = lexer->lookahead;
        if (!((h >= '0' && h <= '9') || (h >= 'a' && h <= 'f') || (h >= 'A' && h <= 'F'))) break;
        advance(lexer);
      }
      lexer->mark_end(lexer);
      lexer->result_symbol = HEREDOC_ESCAPE;
      return true;
    }
    advance(lexer);
    has_content = true;
  }
}

// `<<~ID`, `<<~"ID"`, `<<~'ID'` (the `<` already seen, not consumed).
static bool scan_heredoc_start(Scanner *s, TSLexer *lexer) {
  advance(lexer);
  if (lexer->lookahead != '<') return false;
  advance(lexer);
  if (lexer->lookahead != '~') return false;
  advance(lexer);
  Heredoc h = {.len = 0, .raw = false};
  int32_t quote = 0;
  if (lexer->lookahead == '"' || lexer->lookahead == '\'') {
    quote = lexer->lookahead;
    h.raw = quote == '\'';
    advance(lexer);
  }
  while (is_word(lexer->lookahead)) {
    if (h.len >= MAX_ID) return false;
    h.id[h.len++] = (char)lexer->lookahead;
    advance(lexer);
  }
  if (h.len == 0) return false;
  if (quote) {
    if (lexer->lookahead != quote) return false;
    advance(lexer);
  }
  if (s->npending < MAX_HEREDOCS) s->pending[s->npending++] = h;
  lexer->mark_end(lexer);
  lexer->result_symbol = HEREDOC_START;
  return true;
}

bool tree_sitter_alexandrite_external_scanner_scan(void *payload, TSLexer *lexer, const bool *valid) {
  Scanner *s = (Scanner *)payload;

  // Error recovery: every symbol is valid; let the grammar's lexer recover.
  if (valid[ERROR_SENTINEL]) return false;

  if (s->has_active && (valid[HEREDOC_CONTENT] || valid[HEREDOC_END])) {
    return scan_heredoc_body(s, lexer, valid);
  }

  bool space_before = false;
  for (;;) {
    int32_t c = lexer->lookahead;
    if (is_blank(c)) {
      skip(lexer);
      space_before = true;
      continue;
    }
    if (c == '\n') {
      if (s->npending > 0 && valid[HEREDOC_BODY_START]) {
        advance(lexer);
        lexer->mark_end(lexer);
        s->active = s->pending[0];
        s->has_active = true;
        s->npending--;
        memmove(s->pending, s->pending + 1, s->npending * sizeof(Heredoc));
        lexer->result_symbol = HEREDOC_BODY_START;
        return true;
      }
      if (valid[NEWLINE]) {
        advance(lexer);
        lexer->mark_end(lexer);
        if (continues(lexer)) return false;
        lexer->result_symbol = NEWLINE;
        return true;
      }
      skip(lexer);
      space_before = true;
      continue;
    }
    break;
  }
  if (lexer->eof(lexer)) return false;

  int32_t c = lexer->lookahead;

  if (c == '{' && (valid[BLOCK_LBRACE] || valid[BODY_LBRACE])) {
    advance(lexer);
    lexer->mark_end(lexer);
    if (valid[BLOCK_LBRACE] && valid[BODY_LBRACE]) {
      // In a condition a `{` is the body, unless block parameters follow.
      while (is_blank(lexer->lookahead)) advance(lexer);
      if (lexer->lookahead == '|') {
        advance(lexer);
        lexer->result_symbol = lexer->lookahead == '|' ? BODY_LBRACE : BLOCK_LBRACE;
      } else {
        lexer->result_symbol = BODY_LBRACE;
      }
      return true;
    }
    lexer->result_symbol = valid[BLOCK_LBRACE] ? BLOCK_LBRACE : BODY_LBRACE;
    return true;
  }

  if (c == '-' || c == '^') {
    bool minus = c == '-';
    bool un = valid[minus ? UNARY_MINUS : UNARY_CARET];
    bool bin = valid[minus ? BINARY_MINUS : BINARY_CARET];
    if (!un && !bin) return false;
    advance(lexer);
    int32_t n = lexer->lookahead;
    // `-=`, `-%`, `->`, `^=`: other tokens.
    if (n == '=' || (minus && (n == '%' || n == '>'))) return false;
    lexer->mark_end(lexer);
    bool unary;
    if (un && bin) {
      // `puts -x`: a space before and none after starts an argument.
      unary = space_before && !is_space(n);
    } else {
      unary = un;
    }
    lexer->result_symbol = unary ? (minus ? UNARY_MINUS : UNARY_CARET) : (minus ? BINARY_MINUS : BINARY_CARET);
    return true;
  }

  if (c == '<' && valid[HEREDOC_START]) {
    return scan_heredoc_start(s, lexer);
  }

  if (is_word_start(c) && valid[SUFFIXED_IDENTIFIER]) {
    while (is_word(lexer->lookahead)) advance(lexer);
    int32_t t = lexer->lookahead;
    if (t != '?' && t != '!') return false;
    advance(lexer);
    int32_t n = lexer->lookahead;
    // `x?.f`, `x?:`, `a!= b`, `x?=`: the mark is punctuation.
    if (n == '=') return false;
    if (t == '?' && (n == ':' || n == '.')) return false;
    lexer->mark_end(lexer);
    lexer->result_symbol = SUFFIXED_IDENTIFIER;
    return true;
  }

  return false;
}
