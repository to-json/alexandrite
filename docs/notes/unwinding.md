# Unwinding: running defers when a task panics (deferred design)

Status: **not scheduled** (user, 2026-10-04). Lock release and poisoning on panic come first (GO-VS-RUBY F6). This note records what full unwinding would take, so the decision can be made when a real program needs it.

## Today

- **Main task:** a panic (index out of bounds, overflow, `panic(...)`, `.unwrap` of none, a failed `to_X`) prints the message and aborts.
- **Spawned task:** a panic ends only that task.
  - C runtime and JIT: `alx_panic` → `task_panic` longjmps back to the task's entry (`compiler/runtime/alx.c`; the JIT links the same runtime). The task's regions are freed with it, and `t.wait` returns the message as an error.
  - Rust oracle: the task body runs under `catch_unwind` (`compiler/runtime/prelude.rs`).
  - Browser (wasm): `abort_with` → `sched::task_panic`, then `stop(Stop::Abort)` throws out of the task (`web/src/rt.rs`).
- **What is skipped:** nothing between the panic and the task entry runs.
  - `defer` statements don't run. They are lowered inline at each block exit (`emit_defers` in `compiler/src/lower.rs`), so they don't exist at run time as a list anyone could walk.
  - Locks held in `mu.lock { }` stayed locked. Being fixed separately: the runtime tracks a task's held locks, releases them on panic, and poisons them.
  - Files, sockets, temp directories and child processes owned by the task stay open, or keep existing, until the program exits.

## What full unwinding means

A panic in a spawned task walks back up that task's call frames. At each frame it runs the pending defers of every open block, innermost first, exactly as a normal exit would. It also releases the frame's region (the mark/reset of light frames, and the iteration regions of open loops) before the task ends. This is Go's behaviour (defers run during a panic) and C++'s (destructors run during unwinding).

A `recover`-style catch would reuse the same machinery, stopping the walk at a chosen frame instead of the task entry. That is a separate decision; F6's answer to "recover" is task isolation, plus `assert_panics` in tests.

## Design options

1. **Defer records at run time.** Each `defer` pushes a record (function pointer plus captured environment) onto a per-task stack, and each block exit pops and runs its own. A panic runs everything left on the stack, then longjmps.
   - Cost: a push and pop on every deferred block, even when nothing panics. Defers are rare, so this is cheap overall.
   - Frame regions are already per call: the record stack can hold "release region R" entries too, or the task's region release can stay as it is (the task's memory is freed wholesale at the end anyway).
   - Every backend can do it: it's runtime calls plus closures, no platform unwinder. The browser already throws to the task entry, so it only needs to run the stack first.
   - Risk: the deferred code runs after a longjmp, so the frames it refers to are gone. Captures must be copied into the record (as `spawn` copies), not referenced, and a deferred call can't see locals changed after the `defer`. Go's deferred closures *do* see later changes, so this differs from Go and the difference would need documenting.
2. **Setjmp per deferring frame.** A function with defers sets a jump buffer at entry, and panics longjmp frame by frame. Each landing runs that frame's inline defer code, then re-raises.
   - Defers see the live frame (Go's semantics).
   - Cost: setjmp on every call of a deferring function; it's not free on arm64 (it saves ~20 registers).
   - JIT (Cranelift) and wasm don't have setjmp. The JIT could call C trampolines. wasm needs exception handling (`try`/`catch`, now widely supported) or the existing unwind/rewind machinery in `web/src/suspend.rs`, which already rewrites blocking functions to unwind a task's frames.
3. **Native unwinding tables.** C compiled with `-fexceptions` plus a personality routine; Cranelift's unwind info; wasm exception handling.
   - Zero cost when nothing panics.
   - Most complex by far: three different unwinders, region state to restore at each landing pad, and C (not C++) has no landing pads, so each deferring function would need generated cleanup code wired into the tables.

## Recommendation, if this is ever taken up

Option 1 (run-time defer records with copied captures) is the right first step: uniform across backends and cheap. It covers the realistic needs: closing files and sockets, removing temp dirs, killing children, logging. Panics are already rare, since `~` errors are the normal failure path. Option 2 is the upgrade path if Go's "defer sees later changes" semantics turns out to matter.

## Triggers for doing it

- A server or long-running program that recovers from handler panics via task isolation and leaks file descriptors or temp files as a result.
- A std port whose correctness depends on cleanup running during a panic. None has needed it so far; ports use `~` for error flow.
- The decision to add a `recover`/`catch_panic` construct for programs, which needs the same walk.

## Tests it would need

Defers running on panic, in order, across nested blocks and frames. Deferred calls that themselves panic, which should report both messages, as Go does. Defers in loops with iteration regions. Panics inside `lock` blocks interacting with poisoning. Every backend: JIT, C (release and sanitizers), Rust oracle, browser single-thread and threaded builds.
