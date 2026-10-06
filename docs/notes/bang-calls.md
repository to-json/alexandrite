# `!` calls: receivers by reference

A `!` method changes its receiver. Its `self` parameter is a one-element
slice `[T]` (the parser gives it that type; `self.x` reads `self[0].x`).
Until 2026-10 every call filled a fresh or per-function *cell* with a copy of
the receiver, called the method on it and copied the cell back
(port-issues #34, #41, #152). That cost a copy each way, a push into the
program region per invocation of the calling function, and made math/rand
15-20x slower than Go.

Now the method gets a **view**: a one-element slice that refers to the place
itself.

## Checker (check.rs)

`mutating_call` / `mutating_iface_call` resolve the receiver to a place
(`place`: a local, then index and field steps), evaluate arguments that call
something into temporaries first (D50: `p.push!(p.new_node!)` sees the inner
call's change), and build

    TK::Bang(root, steps, view, call)

where `call` is the `TK::Call` (or `M::IfaceCall`) whose receiver argument is
`TK::Local(view)`. `view` is a `[T]` local, shared by call sites with the same
receiver type in a function (`view_local`; a view is only live during its
call). There is no write-back in the typed tree.

## Lowering (lower.rs `bang`)

1. The step indices are evaluated once and bounds-checked (guards under `~`).
2. `LS::View { dst: view, var, steps, ty, region }`, the call, then
   `LS::Unview { dst, var, steps }`.
3. `region` is where what the method stores into the place must live, read
   back in the callee by `LE::ViewRegion` (the `Place::Into(self)` case):
   - a place inside an array (any index step): `ViewRegion` of the innermost
     array, i.e. that array's own region (R2), or the region its view carries;
   - a place in a local's own variables: the region the analysis gives the
     `Bang` node, which regions.rs registers as an allocation site aliased to
     the local (what the old cell's site was): the frame, an iteration region,
     the result's, the program region.

In a lambda's body (port-issues #164) the analysis of the function the
lambda is written in can't name a frame: a lambda has none, and the region
current when it runs is its caller's choice (its result's region), which
may be a frame that ends right after the call. regions.rs gives each site
in a lambda's body an `LPlace` of its own (`FnPlacement::lambda_sites`),
found by following what the site must outlive through the lambda's own
locals and stopping at its parameters and captures:

- nothing outside the lambda: the current region (`Cur`);
- one parameter or capture `x`: the region `x`'s storage lives in, found
  at run time (`Storage(x)`). For a `!` call on `x.f.g` that is the region
  of the place `x.f.g` when it holds a single pointer (an array, map or
  string, or a struct with one such field: `w.header` is one map although
  `w` holds more), else of `x`, else the program region (lower.rs
  `storage_region`; a null pointer gives the program region);
- anything else (several of them, the program region): the program region.

A def's `Place::Into(p)` uses the same `storage_region` (it took the
region of array and string parameters only, the program region for
everything else).

Copies are kept (a cell in that region, written back after the call) where a
view can't be used:

- the callee keeps `self` past the call: `self_escapes` (a lambda, task or
  generator capturing it, or `self` stored somewhere). Interface dispatch
  decides per implementor.
- a bare local of a narrow integer type (its C variable is an `int64_t`, the
  view's element a narrower integer).

## Backends

- **C** (cgen.rs): `dst = (Arr_T){ .ptr = &place, .len = 1, .cap = -(int64_t)region }`;
  `Unview` emits nothing. `ALX_VIEW_REGION(a)` is `a.cap < 0 ? -a.cap : alx_region_of(a.ptr)`.
  A view's negative cap never reaches array code: `self` itself is never a
  value in alx source.
- **JIT** (jit.rs): a place in memory (through an index) gets its address
  directly. A place in SSA variables (a local or its fields) is stored into a
  stack slot of its own for the call and loaded back at `Unview`: a copy, but
  no allocation.
- **Oracle and wasm**: `lir::copy_views` turns `View` into a one-element array
  literal and `Unview` into the write-back before emitting. Regions are no-ops
  there.

## Semantics

Unchanged where it can be observed by a program that doesn't panic: writes
are visible after the call (and during it, but nothing else can read the
place while the call runs: arguments are values evaluated before it).

After a panic in the middle of a `!` method, the receiver's place holds the
writes made before the panic on the C and JIT backends, as a Go pointer
receiver does; before, they were discarded with the cell, and the oracle
and wasm still copy. This is visible only in storage shared with another
task (a slice element), which `spawn` and `sharing.rs` mostly rule out; it
is left unspecified. Inside `lock { |v| v.m! }` the place is `v`, a
variable written back only when the block finishes, so the guarded value
keeps its old value and the lock is poisoned, on every backend.

Acceptance case `bangcalls` (S5) covers aliasing (arguments that read the
receiver or call `!` on it), slice elements, nested fields, interface values
in locals, fields and slices, recursion, fallible methods, an escaping
`self`, enums, generics and a panic inside `lock`.

## Numbers (release, Apple M-series, loaded machine)

| | before | after | Go |
|---|---|---|---|
| math/rand Int63 | 55-73 ns | 4.0 ns | 2.4 ns |
| math/rand Float64 | 113-131 ns | 4.2 ns | 3.8 ns |
| `self.field.m!` in a loop | 11.4 ns | 0.6-1.6 ns | |
| `self.xs[i].m!` | 11.3 ns | 1.3 ns | |
| `local.m!` | 3.6 ns | 0.2-0.5 ns | |

## C frame size (port-issues #81, #109, #133)

Two cgen changes, measured with clang's `-fstack-usage`:

- `homes`: each C local is declared at the start of the innermost block (an
  `if` arm, a loop body) holding all its uses, when the first statement there
  using it sets it whole (so it starts fresh on each entry). Clang emits
  lifetime markers for block-scoped locals and gives locals of disjoint
  blocks one slot; a function-scope local held its slot for the whole call.
- Array literals with one element (and arrays of structs) are built in place
  (`({ Arr a_ = Arr_cap(n); a_.ptr[0].f0 = ...; a_; })`) instead of from a
  stack array literal: clang keeps compound literals for the whole function,
  and every `fail` site built an Error's body (1440 bytes in a template
  program) that way.

text/template's walker went from ~32 KB to ~6 KB per nested template
(`walk!` 8.8 KB -> 1.1 KB, `walk_template!` 4.5 KB -> 1.3 KB), a synthetic
recursive descent with three arms from 6.4 KB to 0.4 KB.
