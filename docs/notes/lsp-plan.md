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

### 0.1 Memory and stdin probe

A 50-line program: reader task reads framed messages from stdin, sends them over a `Chan` to an owner task that replaces an entry in a `Map[Str, Str]` and runs a debounce `select`; drive it with 10k × 100 KB messages. Record `max_rss`, `alx explain mem` output, and whether the owner stalls when both tasks land on one worker. Outcomes:

- flat and responsive → server shape as in 2.4;
- leaks or stalls → pick one before Stream 2 starts: a single-task loop with no `Chan` (store created in the loop function, R3 shape), or fix port-issues #169 / #166 in `regions.rs` / the runtime. Record the choice in this plan.

### 0.2 Checker recovery spike

Throwaway branch: 1.3's unit selection plus root-level recovery, run on `std/strings`, `std/net/http`, and a package with generic structs. Report: latency per unit, the count and kinds of new diagnostics check-all surfaces in each, how invasive 1.4's instance-level recovery is. If instance-level recovery is invasive, M1 ships root-level recovery.

### 0.3 `enter_pkg` drop guard

`enter_pkg` returns a guard that restores the previous package context on drop; replace the hand-paired `leave_pkg` calls. Behaviour-neutral for normal compiles; covered by the existing acceptance suite.

### 0.4 std overlays and real paths

- `std_files()` consults the `read` closure first, so an unsaved `std/strings` buffer is seen by every package that imports it; with `ALX_STD_DIR` set, std is read lazily through `read`/`list` instead of 925 files per process.
- `SourceMap` file names carry real absolute paths for files that exist on disk (std included, `$std/rel` mapped back to `ALX_STD_DIR/rel`); synthetic files (`<builtin>`, the test runner) keep their names and are reported on the focused file.
- `load_tests` and `embed::resolve` take the same `read`/`list`.

### 0.5 stdin wait and process control (std)

- `os.File` read on fd 0 parks the task on the poller (as `alx_sys_poll2`; the `alx_fd_wait` extern exists in `net.alx`) instead of blocking the worker. Only needed if 0.1 shows the stall, or for any stdin-reading server shape.
- `os/exec` gets `Cmd.kill` and a timeout (Go's `Process.Kill`, `exec.CommandContext` shape), documented in the package header. Needed to cancel superseded checks and survive a hung compiler.

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

Per 0.1: either one task owning all state with a reader task and `select` over message and result channels plus `time.after` (150 ms debounce), or a single-task loop. Either way: one compiler process in flight; results carry the document versions they were computed from; stale results are dropped and the check reruns if anything changed meanwhile; a superseded run is killed (0.5). Per unit the server remembers which URIs it last published to, so diagnostics in files that are not open are cleared when they go away.

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
