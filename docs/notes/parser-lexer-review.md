# Parser & lexer review (2026-10-06)

A full read of `compiler/src/lexer.rs` (701 lines), `parser.rs` (2163), `ast.rs` (511),
and `diag.rs` (99), with the alarming claims spot-verified by hand.

**Verdict: sane.** Disciplined recursive descent, one function per precedence level, correct
associativity everywhere, no reachable panics, no infinite loops, no non-char-boundary
slicing, uniform `Result<_, Diag>` errors. The list below is everything worth acting on.

## Status (2026-10-06)

Line numbers below are from the review and stale; find code by name.

| Item | Status |
|---|---|
| 1. `refine` and `def self.` | **already fixed** before this pass (`refine_def` only types a `self` parameter) |
| 2. cmd auto-import on any backtick in `#{}` | **fixed**: the import is added when a command literal is actually parsed (`Parser::cmd_own`, set in `primary`'s `Tok::Cmd` arm, carried out of `code_expr`). Test: parsefix (`--strict`) |
| 3. comment-blind `#{}` matcher | **fixed**: one scanner, `lexer::interp_end` + `quoted_end`, for strings, heredocs and command literals; skips nested strings/command literals (with their own `#{}`) and `#` comments; the code may span lines. Test: parsefix |
| 4. modifier `if` uses `expr()`, not `cond()` | **open** (not in this pass; needs a decision) |
| 5. auto-import ignores the file's `import "os/exec"` | **fixed**: command literals use the file's own alias unless a local hides it (then `alxexec` as before). Test: parseexec (`--strict`) |
| 6a. `#![foo(f(1))]` | **fixed** (strips one `)`) |
| 6b. dead `let _ = word_start` | **fixed** |
| 6c. `1.e5` | **stays a method call** (user decision, GO-VS-RUBY T12); "no method `e5` on Int" now says to write `1.0e5` (also for `e`, from `1.e-3`). Test: parser.bad4 |
| 6d. heredocs and `#{` | **heredocs follow Ruby** (user decision, T11): `<<~ID` / `<<~"ID"` take `#{}` and every `"..."` escape; `<<~'ID'` is raw. Tests: parsefix, heredoc, heredoc.bad1 |
| Unify the `.name` parsers | **done**: `dot_step` (with `type_args_then_dot`) serves `.`, `.~`, `?.` and the `~` chain |
| `+=` skips place/const validation | **open** (not in this pass) |
| `=` RHS on the next line | **fixed**, also `op=`, `a, b =` and place multi-assign (T10). Test: parsefix |
| Leading-dot chains | **fixed**: `postfix_from` → `leading_dot` (a newline run followed by `.`/`?.`; there is no `&.` token, and a Float needs a digit before the point, so nothing else starts with `.`). `alx fmt` indents the steps and comments between them. Test: parsefix, compiler/tests/fmt.rs |
| Speculative `TypeApp` swallows the type error | **fixed**: when the index re-parse also fails, the type's error is reported (`Stack[Int;].new`: "expected `,` or `]` after a type argument"). Test: parser.bad6 |
| `def <<`, `def []=` | **out of scope** (undecided) |
| Stale `OpAssign` comment | **fixed** |
| `kw_name` via `Debug` | **fixed**: explicit match returning `&'static str`; `describe` uses it too |
| `~T?` rejected | **open** (not in this pass) |
| `import`/`pub`/`refine`/`error` ignore `is_local` | **fixed** (and `extern`); `import 42` is a parse error. Test: parser.bad5, parsefix (`pub`/`import` as locals) |
| `had_args` backwards peek | **fixed**: `dot_step` returns whether it read parentheses (no AST change) |
| `spawn {` rewind | **fixed**: it was a no-op (`bump(); pos -= 1`), removed |
| Case-pattern qualified-name heuristic | **open** (comment not added) |
| `Op` enum | **out of scope** |
| *Added:* `fail` / `spawn` | **hard keywords** (user decision, T9): `Kw::Fail`, `Kw::Spawn`; can't name a local, param or top-level def; `fail(e)` = `fail (e)`; bare `fail` is an error; still method/field names via `kw_name`. Tests: parser.bad1-3, parsefix |

## Fix now (real bugs, cheap today)

1. **`refine` corrupts `def self.` methods' first parameter type** — `parser.rs:621-623`.
   The fix-up does `d.params.first_mut()` unconditionally, but `def self.make(x: Int)` has no
   `self` param, so `x`'s declared type gets overwritten with the refinement target (or
   `[Target]` for `!` methods). Two lines: skip when there's no `self` param, or reject
   `def self.` inside refinements.
2. **Command-literal auto-import is string-greedy** — `parser.rs:65-73`. Any backtick inside
   an interpolation code piece counts as a cmd literal: `"#{ "`" }"` adds
   `import alxexec "os/exec"`, nothing uses it, and the unused-import warning fires (an
   error under `alx test`). Fix: lex the code piece and look for `Tok::Cmd` instead of
   sniffing for `` ` ``.
3. **Interpolation brace-matcher is comment-blind** — `lexer.rs:389-412` (and the duplicated
   copy at `interp_end` for cmd literals). A `}` inside a `#` comment inside `#{...}` closes
   the interpolation early and silently mis-lexes the rest of the string. Written twice;
   make one shared scanner that knows about comments.
4. **Modifier-`if`/`unless` conditions don't use `cond()`** — `parser.rs:1122` parses the
   modifier condition with raw `expr()`, while `if_rest` at `parser.rs:1137` uses `cond()`.
   So `x = 1 if xs.any? { it > 2 }` and `if xs.any? { it > 2 } { }` — two positions that read
   identically — parse differently. User-visible syntax semantics; decide deliberately.
5. **Auto-import doesn't reuse an existing `import "os/exec"`** — `parser.rs:70-73` always
   pushes a new alias, while the sibling derive path (`parser.rs:369-375`) correctly reuses
   `encoding/json`. Benign today only because `front.rs` dedupes the load.
6. **Lexer nits, one-liners:**
   - `trim_end_matches(')')` at `lexer.rs:157` strips *all* trailing parens
     (`#![foo(f(1))]` → arg `f(1`). Strip one.
   - Dead binding `let _ = word_start;` at `lexer.rs:573` — delete.
   - `1.e5` doesn't lex (`lexer.rs:232` requires a digit after `.`) — `Int(1)` `.` `Ident(e5)`.
     Ruby accepts it; decide.
   - Heredocs silently emit literal `#{x}` (no interpolation, per milestones M2) — at least
     warn on `#{` in a heredoc body until it's supported.

## Fix soon (a day of cleanup, not urgent)

- **Unify the 3½ copies of "parse `.name` after a dot"** — `parser.rs:1580` (`postfix_step_named`),
  `parser.rs:1598` (`postfix_step`), `parser.rs:1625` (`?.`), `parser.rs:1641` (main `.` arm,
  which also carries the `pkg.Type[...]` TypeApp logic). Fix a call-syntax bug in one arm and
  you'll miss the others. Unify into one helper with flags.
- **`+=` skips the place/const validation that `=` has** — `parser.rs:1333-1339` checks
  `const_root`/`is_place`; `parser.rs:1366-1371` checks neither. The checker catches it later
  via `place()` (`check.rs:2298`) with a worse message and wrong span blame.
- **`=` RHS can't continue on the next line, but const `=` and binary RHS can** —
  `parser.rs:283`/`316` call `skip_line_continuation()`; `parser.rs:1345` doesn't. One line.
- **No leading-dot method chains across newlines** — `x.\nmap` works (`parser.rs:1596`) but
  `x\n.map { }` doesn't (`postfix_from` never skips newlines). Since `.` at line start is
  unambiguous, one `skip_newlines` does it. Ruby users will trip on this.
- **Speculative `TypeApp` parse swallows the real type error** — `parser.rs:1653-1671` and
  `1871-1891`: on `Err` from `type_expr()` they rewind and re-parse as an index expression, so
  `Stack[Int;].new` degrades into "expected `]`, found `;`" instead of the type error. Stash
  the first error and re-report it if the re-parse also fails.
- **`def` can't define `<<` (or `[]=`, `<`, `&&`...), yet `<<` is always a method call** —
  `parser.rs:467` whitelists `+ - * / % == <=> []`; `parser.rs:1457-1465` hard-desugars every
  `<<` into a method call. Extending the whitelist is nearly free at the parser (check.rs keys
  methods by name string); a language-design decision to make deliberately.
- **Stale doc comment** — `ast.rs:452-454` says the parser desugars `OpAssign` to
  `Assign(x, Binary(op, x, e))`; `check.rs:2297` has owned that since. Fix on next touch.
- **`kw_name` derives method names from `Debug` formatting** — `parser.rs:2142-2144`
  `format!("{k:?}").to_lowercase()`. Renaming a `Kw` variant silently renames callable methods.
  A `match` returning `&'static str` is the same length and honest.
- **`~T?` is silently rejected** — every `type_expr` branch except `Result` (`parser.rs:884`)
  accepts a trailing `?`. Either allow it or emit the explicit "write `(~T)?`" error the codebase
  already knows how to give (`parser.rs:512`).
- **`import`/`pub`/`refine`/`error` arms ignore `is_local`**, unlike `spawn`/`select`/`fail`/
  `using`/`test` — `parser.rs:190`, `194`, `236`, `302`. A local named `import` is un-declarable
  at top level, and `import 42` becomes a confusing check-time "call to unknown function" instead
  of a parser error.
- **`had_args` peeks already-consumed tokens as a side channel** — `parser.rs:1543-1544`
  infers "call had explicit parens" from `self.toks[self.pos - 1]`. Correct today but the only
  place the parser reasons backwards; a `Call`-node flag would be honest. Low priority.
- **`spawn {` does `bump(); pos -= 1;`** — `parser.rs:1922-1926` rewinds the cursor so
  `braced_stmts` can re-consume the `{` for its span. Harmless but the only cursor-rewind-by-
  arithmetic in the file; pass the known span into the block parser instead.
- **Case-pattern qualified-name heuristic is shape-based and lossy** — `parser.rs:1223-1248`
  reinterprets `a.b.C =>` as variant `C` with segments dropped. Probably unreachable with today's
  type grammar; leave a comment that the lossy case is intentional.

## Don't bother

- The 51 `Tok::Op("...")` string-match sites are the only structural seam. Switching `Op` to an
  enum is monotonically more annoying, but nothing is *wrong* today.
- No rescanning traps, no copy-pasted precedence table, `code_expr` re-lexing with `base` offsets
  is sound, `expand_derives` re-parsing is contained with good error wrapping.
- The `spawn {` rewind (above) is ugly but safe.

## Verified fine (so nobody re-audits)

- All `unreachable!()`/`expect` sites guarded by immediately preceding peeks; raw
  `self.toks[self.pos±k]` indices all range-safe or provably `pos ≥ 1`.
- `in_cond` save/restore correctly threaded through every nested expression entry point.
- `-x**2 = -(x**2)`, the `i64::MIN` fold guard (`parser.rs:1488`), `-2i` complex literal
  (`parser.rs:1500`) are deliberate and correct.
- Lexer is O(n), char-aligned on every advance path, interpolation re-lexing (`lex_at`) handles
  absolute offsets correctly.
