# alx-lsp: a language server for alx, written in alx (M1: diagnostics in Neovim)

Date: 2026-10-06
Depends on: None (R12 recursive types is on main; rebuild `target/release/alx` before any probe, the main checkout's binary predates R12)

---

## Context

alx has no editor support beyond tree-sitter highlighting. The goal is the owner's daily editing, in `std/` packages and in standalone scripts and programs, in Neovim: errors and warnings as they type, on unsaved buffers. Milestone 1 is diagnostics only.

Decisions (interview, 2026-10-06):

- **The compiler is the only source of diagnostics.** The server is alx; it drives `alx check --json` as a subprocess, one per check, no daemon. No alx-written parser in M1 (cut after review: a third grammar, hand-synced against a parser that changed 78 times in 30 days, for a gain of "more than one syntax error").
- **Syntax errors:** the Rust parser records one error per top-level declaration and continues. Neovim's tree-sitter `ERROR`/`MISSING` nodes give instant multi-error squiggles from a small Lua snippet, independent of the server.
- **Type errors:** one per checked root (see 1.4), and uncalled defs are checked ("check all, fix std as found").
- **Trigger:** on change, with unsaved buffers as overlays. While a buffer has a syntax error the last semantic diagnostics are kept, positions mapped through edits, until the next successful check.
- **Unit selection:** heuristic with override (1.3).
- **Memory:** probe first; gate the server on its result (Stream 0.1).

Verified facts that shape the design (reviews, 2026-10-06; line numbers drift, find code by name):

- The checker is instantiation-driven: `check_program` checks `main` and what it reaches through `World::instance`; `check_library` checks `pub` fully typed defs through `check_exports`. An uncalled def is never checked (`def f(x: Int) -> Str { x }` alone passes `alx check`).
- A failed instance stays memoized with `funcs[id] = None`; `World.fatal` latches the first error and every later `check_fn` returns it. A caller of a failed instance with an inferred return type gets a false "`f` is recursive: declare its return type"; `front.rs` `expect("every instance checked")` panics on a `None` func.
- `enter_pkg`/`leave_pkg` are hand-paired (about 21 sites); `check_exports` and `add_consts` return with `?` between them, leaving the package context wrong.
- Methods are defs named `Owner.m` with a typed `self`; methods of generic owners have empty `d.tparams`, so `type_from` fails on them. Derive-generated defs are defs too.
- `front::load_with` takes `read`/`list` closures; `load_tests` and `embed::resolve` read the disk directly. std imports are served by `std_files()` (a `OnceLock`, virtual `$std/...` paths, all 925 files read when `ALX_STD_DIR` is set) before the `read` closure is consulted, and std diagnostics are shown as `std/<rel>`.
- All checker state is thread-local; a subprocess per check shares nothing. `catch_unwind` works (no `panic = "abort"`).
- Region rules: a value sent on a `Chan` or stored into a parameter's storage lives in the program region, which is never freed (R1; port-issues #169 open). Blocking stdin reads freeze the pinned worker (port-issues #166 open). `os/exec` has no timeout or kill.
- Latency is not the problem: `alx check` 0.01 s embedded, 0.03 s with `ALX_STD_DIR` (stale binary; package units unmeasured).

Not known: how many pre-existing errors check-all surfaces in std; whether the server's memory stays flat in any shape; package-unit latency on large packages.

---

## Stream 0: Prerequisites and probes (compiler, std)

Ordered; 0.1 and 0.2 gate the rest.

### 0.1 Memory and stdin probe (done 2026-10-06)

Probe: 10k framed messages of 100 KB on stdin, each stored by replacing one `Map[Str, Str]` entry; `alx build --release`, macOS `time -l`.

| Shape | max RSS |
|---|---|
| single task, `read_msg` loop, store in the loop's function | 14.6 MB |
| reader task → `Chan[Str]` → owner task | **2,566 MB** |

A value sent on a `Chan` lives in the program region and is never freed (R1, port-issues #169 territory). **Decision: the server is a single-task loop with no `Chan` and no `spawn`.** Debounce and compiler runs happen in that loop (2.4, 0.5). The probe is `docs/notes/lsp-probe-memory.alx`.

### 0.2 Checker recovery spike

Throwaway branch: 1.3's unit selection plus root-level recovery, run on `std/strings`, `std/net/http`, and a package with generic structs. Report: latency per unit, the count and kinds of new diagnostics check-all surfaces in each, how invasive 1.4's instance-level recovery is. If instance-level recovery is invasive, M1 ships root-level recovery.

### 0.3 `enter_pkg` drop guard

`enter_pkg` returns a guard that restores the previous package context on drop; replace the hand-paired `leave_pkg` calls. Behaviour-neutral for normal compiles; covered by the existing acceptance suite.

### 0.4 std overlays and real paths

- `std_files()` consults the `read` closure first, so an unsaved `std/strings` buffer is seen by every package that imports it; with `ALX_STD_DIR` set, std is read lazily through `read`/`list` instead of 925 files per process.
- `SourceMap` file names carry real absolute paths for files that exist on disk (std included, `$std/rel` mapped back to `ALX_STD_DIR/rel`); synthetic files (`<builtin>`, the test runner) keep their names and are reported on the focused file.
- `load_tests` and `embed::resolve` take the same `read`/`list`.

### 0.5 Timed readability wait and process control (std, runtime)

- `os.File.wait_readable(timeout: Duration) -> ~Bool`: true when a read would not block (data, EOF or error pending), false on timeout. An alx extension (Go has `SetReadDeadline`); documented in the `os` header. On the main thread it is `poll(2)` with a timeout; in a task it parks on the poller. The server's debounce is `if buffered == 0 && !stdin.wait_readable(150ms)`, with bytes already in the `bufio.Reader` checked first. Runtime function in `alx.c` (the JIT links the same C); the oracle and wasm either implement it or refuse it as they do other FFI.
- `os/exec` gets `Cmd.timeout` (a `Duration`; the stages are SIGKILLed and reaped when it expires and the run fails with `ExecError.Timeout`), documented in the package header (Go: `exec.CommandContext`). The server uses the timeout to survive a hung compiler. `Process.kill` is not built: there is no start/wait split, so no handle to a running process exists; `timeout` is the kill mechanism and `os/signal`'s `kill(pid, sig)` covers a known pid.
- Blocking stdin reads inside tasks (port-issues #166) are fixed only if cheap; the server is single-task and does not depend on it.

Port-issues #166 and #169 are updated with what is found, whatever the outcome.

---

## Stream 1: Compiler analysis mode (Rust)

**File(s)**: `compiler/src/main.rs`, `driver.rs`, `front.rs`, `check.rs`, `parser.rs`, `diag.rs`

### 1.1 `alx check --json`

One JSON object on stdout; exit 0 whenever the JSON is complete (errors are data):

```json
{"unit": "package", "root": "/abs/dir",
 "diagnostics": [{"file": "/abs/dir/a.alx", "phase": "check", "severity": "error",
   "lo": 812, "hi": 815, "line": 40, "col": 8, "end_line": 40, "end_col": 11,
   "message": "...", "notes": ["..."]}]}
```

**M2.5 (type data, compiler/src/typemap.rs):** two more keys after `diagnostics`, for member completion. Old clients ignore them.

```json
"types": [{"lo": 412, "hi": 413, "name": "r", "type": "bufio.Reader", "kind": "local"}],
"members": {"bufio.Reader": [{"name": "read_line!", "kind": "method", "sig": "-> ~Str?"},
                             {"name": "cell", "kind": "field", "type": "[bufio.ReaderState]"}]}
```

- `types`: the focused file only, sorted by `lo`. `kind`: `local` | `param` (span = the name) | `self` (span = the method's def) | `it` (an implicit block parameter `it`/`_1`; span = the block) | `recv` (an expression written before `.` or `?.`: a method call's or field access's receiver; `name` is its source text, e.g. `make(1)`, `a.b`, `p` of `p.move!(1)`).
- `type` is the source spelling: `Str`, `[Int]`, `Map[Str, Int]`, `T?`, `~T`, `Unit`, another package's type by its import name (`tls.Config`, not `crypto/tls.Config`). A `!` method's `self` is `T`, not the `[T]` view.
- Only instances that checked are walked: a def that failed reports nothing; the rest still do. A parse or load failure gives `"types":[],"members":{}`.
- Generics: every instance is walked; a span whose instances disagree on the type is dropped (a generic def used at one type reports that type).
- `members`: one key per distinct `type` in `types` and per named type nested in it (`[net.Conn]` also gives `net.Conn`). Structs: fields (`type`) then methods; enums: methods; interfaces: their methods (`sig` from types: `(ResponseWriter, Request)`); builtins (`Str`, `[T]`, `Map`, ints, `T?`, `~T`, `Error`): the names check.rs's suggestion lists hold (`check::builtin_method_names`), `sig` `""`. A method's `sig` is its source text after the name, whitespace collapsed (`(dx: Int)`, `-> Int`, `""` for none). Another package's methods only when `pub`; static methods (`def self.m`) are left out.
- Binding spans are found in the text near the node that introduces the local (the typed tree keeps no name spans), at a position where a name is bound (`x =`, `x, y =`, `x: T =`, `|x|`, `for x in`, `Variant(x) =>`, `->(x: T)`); a local not found that way is left out.

`line` 0-based, `col` a byte offset in the line; the server converts to the client's encoding. `phase`: `load | parse | check | prove | warning | internal`. Warnings share the list. Diagnostics are in emission order; the first is the one fail-fast mode would have reported.

### 1.2 Overlays

`--overlays FILE|-`: JSON map `{absolute path: text}`. An overlay shadows a disk file; it never adds one. Built on 0.4.

### 1.3 Unit selection

`front::check_unit(path, read, list, force: Auto|Script|Package)`:

- **Auto**: the focused file is a `*_test.alx`, or its directory has no file with top-level statements → **package** (the directory); otherwise → **script**. A new or empty file is a script.
- **package**: load as `load_tests` does, with `only = the focused test file` for test files so the check covers one test file, not every test in the package (`std/math/big` tests are 3.1 MB). A package without tests gets an empty runner.
- **script**: `check_program`.
- A server setting (`alx.unit`) and a CLI flag force `script` or `package`.
- **Roots** for both: everything the unit reaches, plus every def declared in the unit's own files that is non-generic, has all parameters typed, and is not derive-generated (1.4 lists what is skipped).

### 1.4 Recovery

A collect mode on `World`, off by default so normal compiles are unchanged. It is "don't latch `fatal`, record the diagnostic":

- **Declarations** (`add_consts`, `add_structs`, `add_iface_sigs`, `add_refines`): a failed item records its `Diag` and the loop continues. If any declaration failed, bodies are not checked (no placeholder types, no cascades).
- **Roots**: a root whose check fails records its `Diag`; the next root runs. `fatal` is drained per root.
- **Instances**: a `failed: HashSet<FuncId>` is consulted at the `instance(` call sites; a caller of a failed instance is abandoned silently, whether or not the return type is declared, so there is no second, misleading diagnostic. The failed instance's own diagnostic is recorded once.
- **Skipped as roots**: defs with an untyped parameter, methods of generic owners, derive-generated defs, and generic defs. Their errors appear only through instances something creates (documented gap).
- `prove::prove` errors are recorded per function. The `front.rs` `expect("every instance checked")` and the `var_inits` / `message_instances` / `iface_eq_instances` failure points record and skip in collect mode.
- A file that fails to lex ends its unit's check with one `parse` diagnostic. A parse error in one declaration of a file does not (1.5).
- Invariant tests: for every negative acceptance case (`.expected_error`), the first diagnostic in collect mode equals the fail-fast one; and **every std package reports zero diagnostics** (check-all on std becomes an acceptance gate; each new error found is fixed or logged in `docs/notes/port-issues.md`).

### 1.5 Parser: one syntax error per top-level declaration

In `Parser::module`'s declaration loop, record the error, skip to the next top-level declaration keyword (`def`, `struct`, `enum`, `interface`, `import`, `pub`, `error`, `refine`, `test`, `example`, `extern`, `#[`, `#![`) at brace depth 0, and continue; the parse is returned as an error list. The default entry points still return the first error, so every existing caller is unchanged. Speculative re-parses (`self.pos = save`) happen inside one declaration and are unaffected. Tree-sitter grammar: no change (no syntax change).

### 1.6 Panics

`check --json` runs under `catch_unwind`; a panic becomes one `internal` diagnostic and exit 3.

---

## Stream 2: Server (alx), `tools/alx-lsp/`

**File(s)**: `tools/alx-lsp/alx.mod`, `main.alx`, packages `jsonrpc`, `protocol`, `position`, `server`, `compiler`. Shape depends on 0.1.

### 2.1 Transport

`Content-Length` framing over `bufio`/`io.read_full` on stdin; one writer behind a `Mutex`. Envelopes are `json.Value` (ids may be numbers or strings); params the server uses are `#[derive(Json)]` structs in `protocol`.

### 2.2 Protocol subset

`initialize`, `initialized`, `shutdown`, `exit`; `didOpen`, `didChange` (full sync), `didSave`, `didClose`; `publishDiagnostics`; `window/logMessage`. Unknown requests get `MethodNotFound`; unknown notifications are dropped.

### 2.3 Positions

`positionEncoding: "utf-8"` when the client offers it (Neovim does); otherwise byte columns are converted to UTF-16 in `position`, the only place that knows about encodings. Kept semantic diagnostics are shifted through edits (line/column mapping of the changed range).

### 2.4 Store and scheduler

Per 0.1, no `Chan` and no `spawn`. One loop on the main task: read a message; handle it; when stdin has nothing buffered or pending for 150 ms (0.5) and a document changed since the last check, run the compiler (synchronously, with a timeout) and publish. Messages that arrive during a run queue in the pipe and are handled next; the result is published only if the checked versions are still current, else the check reruns. Per unit the server remembers which URIs it last published to, so diagnostics in files that are not open are cleared when they go away.

**Which units recheck:** the edited file's unit; and every other open buffer whose unit imports the edited package (the server tracks imports from the last successful check's file list).

### 2.5 Compiler client

Finds `alx` (setting `alx.path`, else beside the server binary, else `PATH`). Sets `ALX_STD_DIR` when the workspace is the alexandrite repo (has `std/` and `compiler/`; overridable by `alx.stdDir`) or any std buffer is open. Sends every open buffer as an overlay. Output that isn't valid JSON becomes one error diagnostic at the top of the focused file plus a `window/logMessage` with stderr. Behind an interface so tests substitute a fake.

---

## Stream 3: Client, tests, install

### 3.1 Neovim

`docs/editors/neovim.md`: filetype detection for `*.alx`; `vim.lsp.config('alx', { cmd = { 'alx-lsp' }, filetypes = { 'alexandrite' }, root_markers = { 'alx.mod', '.git' } })`; a Lua snippet turning tree-sitter `ERROR`/`MISSING` nodes into `vim.diagnostic` entries (own namespace, shown beside the server's).

### 3.2 Build and install

`alx build --release -o <bin>/alx-lsp tools/alx-lsp/main.alx`. The server and the compiler it drives are built separately; the server reports the compiler's `alx --version` in the log.

### 3.3 Tests

- unit tests in each server package (framing, positions, scheduler against the fake compiler);
- scripted sessions: a test spawns the server, replays framed messages, compares published diagnostics with an expected file;
- a soak test per 0.1 (RSS bounded over 10k `didChange`);
- acceptance group `LSP` (`-k LSP`): builds the server, runs the sessions, the 1.4 invariants, the 1.5 recovery cases.

---

## Milestones

- **M1a**: streams 0, 1, 2, 3. Usable end to end.
- **Later, not planned here**: navigation (`documentSymbol`, folding; needs a syntax tree in-process, the evidence that would revive an alx-written parser, as std packages with a homogeneous `Node {kind, span, kids}`), `definition` from a def/use table the compiler dumps, hover and completion (needs types at a cursor in files that don't compile), formatting through `alx fmt`.

Order: 0.1 and 0.2 first (riskiest), then 0.3–0.5, then streams 1 and 2 in parallel (the JSON schema in 1.1 is the contract), then 3.

## Sequence integration

No plan index exists. Touch points:

- `check.rs` is edited constantly by parallel work; collect mode is a small optional path and 0.3 lands first and alone.
- `driver.rs` has uncommitted changes in the main checkout; work in a worktree and use its own `target/release/alx`.
- Open items in `docs/notes/parser-lexer-review.md` (modifier `if`, `~T?`, `+=` validation) change `parser.rs` independently of 1.5.
- std is embedded in the compiler: after editing `std/`, rebuild, or use `ALX_STD_DIR`.
- Full acceptance takes 30+ minutes; this plan runs `-k LSP` plus the packages it touches, and the merger runs the rest.

## Risks

- **Server leaks or stalls by construction** (program region never freed; stdin blocks a pinned worker). Mitigation: 0.1 gates Stream 2; the soak test stays in the suite.
- **Recovery in an instantiation-driven checker.** Mitigation: 0.2 spike, drop guard (0.3), `failed` set, root-level fallback.
- **Check-all surfaces pre-existing std errors** (noise on day one). Mitigation: it is the exit criterion; each is fixed or logged; the first run's count is the spike's report.
- **Unit selection misfires** on mixed directories. Mitigation: package only without top-level statements; override setting.
- **Compiler panics on half-edited code.** Mitigation: subprocess per check, `catch_unwind` (1.6); every panic found is a compiler bug with a reproducer.

## Open decisions

- **O1**: server home `tools/alx-lsp/` with its own `alx.mod` (proposed).
- **O2**: `alx.unit` / `alx.path` / `alx.stdDir` as `initializationOptions` (proposed) vs. workspace configuration.

## Status (2026-10-06)

Built on branch `lsp-m1` (merge of `lsp-rt`, `lsp-cli`, `lsp-checker`, `lsp-server`):

| Item | State |
|---|---|
| 0.1 probe | done: single-task loop 8.6-14.6 MB, `Chan` shape 2.5 GB |
| 0.2 spike | folded into 1.3/1.4; latencies per unit: net/http 0.8 s, crypto/tls 0.64 s, math/big 0.09 s, small packages 0.02 s |
| 0.3 `PkgGuard` | done |
| 0.4 std overlays, real paths | done (overlays reach std files under `ALX_STD_DIR`, not embedded std) |
| 0.5 `wait_readable`, `Cmd.timeout` | done; `Process.kill` not built (see 0.5) |
| 1.1-1.6 | done (`alx check --json`, `analyze`, collect mode, parser recovery, panic wrapper) |
| 1.4 exit criterion | met for std packages (`std_packages_report_no_diagnostics`); 14 std `*_test.alx` files still report errors, port-issues rows |
| 2.x server | done; deviation: overlays go through a temp file (`--overlays FILE`) because `os/exec` pins what a `Cmd` reaches |
| 2.6 / 3.3 bounded memory | done: 10k edits x 100 KB peak 22 MB (limit 150), ~1 KB/edit residual; server restructured around port-issues #177 (compiler limitation still open) |
| 3.x | done: `docs/editors/neovim.md`, acceptance group `LSP` |

---

# M2: navigation and completion, alx-first (2026-10-06)

Decision (owner): **feature logic goes in alx.** Rust provides data and test oracles; where alx can't host a feature, fix the language. The Rust `alx symbols` dump proposed earlier is dropped. A review measured the alx route (probes in the session scratchpad: `outline.alx` 170 lines, 262/262 top-level declarations on std/net/http matching Rust, ~1 ms per pass over 840 KB; `memidx*.alx` index-in-a-Map memory).

| # | Step | Size | Gate |
|---|---|---|---|
| M2.1 | `tools/alx-lsp/outline/`: outline scanner (byte loop, brace depth; skips strings, `'...'`, command literals, `#{}`, comments, `#[...]` with nested brackets/strings, heredoc bodies). `Sym {kind, name, owner, lo, hi, line}`; methods as `Owner.m`. Works on files that don't parse (the parser's own `resync` idea) | ~250 alx | none |
| M2.2 | Test oracle: `alx parse --decls FILE` (Rust, wraps `parser::parse_recovering`; prints kind, name, line per top-level decl and method) and an `LSP` acceptance check: the outline equals the oracle on every tracked `.alx` file that parses | ~60 Rust + 40 alx | gates M2.3 |
| M2.3 | Server: per-URI `index: Map[Str, [Sym]]` rebuilt on didOpen/didChange (same memory discipline as `docs`); `textDocument/documentSymbol`; `textDocument/definition` for top-level names (open buffers + the unit's files on disk + std when resolvable through `import`); `textDocument/completion` = keywords + index names + `pkg.` names of imported packages + import paths (std dirs, alx.mod); JSON built by hand (#176); soak burst of 10k completions | ~400 alx | M2.2 |
| M2.4 | Compiler, port-issues #177: an index/map-get view aliases the receiver's nodes only (not the key argument); pointer-free locals ignored in `owners`; `for k in m.keys` yields fresh keys. Then the server drops `uris` and the `fresh` copies that only worked around it | 50-100 Rust + mem cases | parallel |
| M2.5 | (done: see 1.1, "M2.5") `alx check --json` adds `types: [{lo, hi, name, type}]` for the focused file's locals, params and fields (a tast walk; data only), and `methods: {Type: [name...]}` for types reachable from the focused file if cheap | ~150 Rust | none |
| M2.6 | Member completion after `.` in alx: receiver type from the last successful check's `types` (shifted through edits), members from the outline index / `methods`; fall back to nothing rather than noise | ~200 alx | M2.1, M2.5 |
