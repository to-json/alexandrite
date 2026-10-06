# Working on alexandrite (for agents)

alexandrite (`alx`) is a compiled language: Go's capabilities and semantics, with Ruby-flavoured syntax and no garbage collector (memory lives in regions). The compiler is Rust (`compiler/`). Its standard library is a port of Go's (`std/`). Read this file first; it should save you most of the exploring.

## Commands

```sh
cargo build --release -q 2>&1 | grep -A9 '^error'   # build; prints only errors
target/release/alx run f.alx                         # JIT (Cranelift)
target/release/alx run --release f.alx               # via C (clang -O2)
target/release/alx test std/<pkg>                    # package tests, JIT
target/release/alx test --release std/<pkg>          # package tests, C
target/release/alx fmt --check f.alx                 # formatter (gofmt-like, no options)
target/release/alx run acceptance/run.alx -k <filter>  # acceptance groups whose name contains <filter>
```

- **Rebuild after editing `std/`:** std is embedded in the compiler at build time. To skip that while iterating, `ALX_STD_DIR=$PWD/std target/release/alx test std/<pkg>` reads std from disk.
- **In a worktree:** use *its* `target/release/alx`, not the main repo's.
- **Full acceptance** (`alx run acceptance/run.alx`, no `-k`) takes 30+ minutes and starts servers on fixed ports. The person merging your branch runs it. You run your packages' tests and the relevant `-k` groups, unless your task says otherwise.
- **One acceptance run per checkout at a time:** each run clears `acceptance/cases/.alx-cache` when it starts, so a second run (even `-k`) breaks the first.
- **Browser backend:** `cd web && ./build.sh && node test.mjs`. It covers the language, not std; run it only when you change the language or the runtime.

## Keep your context small

- **Redirect long output:** `> $TMP/x.log 2>&1`, then read `tail -3` and `grep FAIL`. Acceptance PASS lines are already terse; `-v` shows everything.
- **Search, don't read:** never cat whole compiler files (check.rs is 6,400 lines) or generated files (`vec_*_test.alx`, `std/unicode/range_tables.alx`, testdata). Use `grep -n` and read a range with `sed -n 'a,bp'`.
- **Go's sources:** read the specific files you're porting at `$(go env GOROOT)/src/<pkg>`.
- **Scratch files:** your own scratch directory or the worktree, never another session's.

## alexandrite in Y minutes

```ruby
import "strings"                     # Go's import paths; the last element names the package
import "os/exec"

# Values ------------------------------------------------------------------
x = 42                               # Int (= I64). Also I8..I64, U8..U64, Byte (= U8), Rune (= I32)
f = 2.5                              # Float (= F64)
s = "héllo #{x}"                     # Str: immutable UTF-8 bytes; #{} interpolates
s.size; s[0]; s[1...3]               # byte length; a Byte; a substring by bytes (... excludes the end)
s.runes; s.bytes; Str.from_bytes(bs) # [Rune], [Byte], and back
b = 7.as_u8                          # truncating conversion (Go's uint8(x)); .to_u8 checks instead
y = x +% 1                           # wrapping arithmetic; plain + - * panic on overflow
# `#![overflow(wrap)]` as the FIRST line of a file makes all of its arithmetic wrap (hashes, crypto)

# Collections --------------------------------------------------------------
xs = [1, 2, 3]                       # [Int]: a Go slice (shared storage)
xs << 4                              # append
ys = xs[1...3]                       # shares storage, like Go
zs: [Byte] = [0; 16]                 # 16 zeros; a typed declaration gives literals their element type
fixed: [U32; 4] = [0; 4]             # fixed-size array
m: Map[Str, Int] = {"a" => 1}        # insertion-ordered map
copy(dst, src)                       # Go's copy
xs.map { it * 2 }.select { it > 2 }  # Enumerable; `it` is the block's single parameter
xs.each { |v| puts v }

# Optionals and errors ------------------------------------------------------
def find(k: Str) -> Int? {           # T? is (T, bool) in Go
  return none if k == ""
  k.size
}
if n = find("ab") { puts n }         # narrowing
v = find("") || 0                    # default; opt?.field chains

pub error ParseErr {                 # an error type: an enum with a message
  Bad(pos: Int)
  def message -> Str {
    case self { Bad(pos) => "bad input at #{pos}" }
  }
}
def parse(t: Str) -> ~Int<ParseErr> { # ~T is (T, error) in Go; <ParseErr> declares the error set
  fail ParseErr.Bad(pos: 0) if t == ""
  t.size
}
def first(k: Str) -> ~Int {
  n = find(k) || fail ParseErr.Bad(pos: 1)   # opt || jump (S8): also || return v, || break, || next
  case n { 0 => return 0; _ => n * 2 }       # a bare jump can be a case arm (=> fail e, => break)
}
n = ~parse("x")                      # ~ propagates an error to the caller; it attaches to the NEXT call
m2 = obj.~method(1)                  # ~ on a method call goes after the dot
r = parse("")                        # without ~: a ~Int value (r.ok?, r.err, r.unwrap)
if e = r.err {                       # a ParseErr? (S7: the def declares one type; else an open Error?)
  case e { Bad(p) => puts p }        # match variants (also ParseErr.Bad(p), pkg.ParseErr.Bad(p))
}                                    # `~r` re-raises it in a def declaring ParseErr; e.message, e.wrap work

# Structs, methods, interfaces --------------------------------------------------------
struct Point {
  x: Int
  y: Int
  def sum -> Int { x + y }                    # fields are in scope; `self` is the receiver
  def move!(dx: Int) { self.x += dx }         # `!` methods change the receiver
  def self.origin -> Point { Point.new() }    # a static method: Point.origin
}
p = Point.new(x: 1, y: 2)            # omitted fields are zero
interface Shape { def area -> Float }         # structural, like Go
interface Seeker { def seek!(off: Int) -> Int }
struct Box[T] { items: [T] }                  # generics; def max[T: like Int](xs: [T]) -> T
struct Node { kids: [Node]; next: Node? }     # a type may hold itself through [T], Map, a T? field, closures (R12)
struct Scaled { inner: Shape; k: Float }      # may hold a Shape and be one (R13); `==` works on interface values
def rewind[R](r: R) -> R {                    # `if R is Iface`: decided per instance (S6); the
  x = r                                       #   branch an instance skips isn't checked
  x.seek!(0) if R is Seeker                   # (also `T is like Int`, `T is Str`, as a Bool)
  x
}
# State shared across copies (Go's pointer receivers): keep the mutable state in a
# one-element slice field (`st: [State]`); see std/bytes Buffer and std/bufio.

# Control flow ---------------------------------------------------------------
for v in xs { }                      # also: for i in 0...n, while c { }, loop { break if done }
w = case x { 0 => "zero"; 1..9 => "small"; _ => "big" }   # an expression
z = c ? 1 : 2
defer { cleanup() }

# Concurrency ----------------------------------------------------------------
t = spawn { work() }; r = t.~wait    # M:N tasks (8 MiB stack each; ALX_TASK_STACK)
ch = Chan[Int].new(4); ch << 1; v = ch.recv
mu = Mutex.new(0); mu.lock { |v| v += 1 }   # assigning v updates it; the block's value is lock's result
pub HITS = Atomic.new(0)             # top level: package state only as Atomic[T] (any T; load copies) or Mutex[T],
REG = Mutex[Map[Str, Int]].new({})   #   set before main runs; read as HITS / pkg.HITS (R11)

# Lambdas, shell, FFI, tests --------------------------------------------------------
add = ->(a: Int, b: Int) -> Int { a + b }; add.call(1, 2)
def info(msg: Str, tags: [Str] = [], level: Int = 0) { }   # defaults (S8): per call, in the callee's package
info("hi"); info("hi", level: 2)              # arguments by name too (after the positional ones). No variadics
# A package's own def named like a builtin (`print`, `sprintf`, `copy`) shadows it there (S12)
out = `ls -l #{dir}`.~output                    # a command literal (no shell): os/exec.Cmd
extern def c_getpid() -> I32 = "getpid"       # C FFI; std uses it only for OS access
test "adds" { assert_eq 1 + 1, 2; assert x > 0, "why" }   # in *_test.alx beside the package
assert_panics("out of bounds") { xs[9] }      # body runs as a task (gets copies of locals); fails unless it panics
example "hi" { puts "hi" } outputs "hi\n"
# Warnings (unused locals/imports) are errors under `alx test`; name a local `_x` to silence it.
```

**Missing on purpose:** reflection (compile-time derives such as `#[derive(Json)]` instead), nil, inheritance, exceptions, a GC.

Every line above compiles (checked 2026-10-05). The design decisions and their reasons are in GO-VS-RUBY.md, one row each (`D12`, `S3`, `R5`...).

## Where things live

- **`compiler/src/`**, in the order a program goes through:
  - `lexer.rs` → `parser.rs` (`struct_def`, `case_rest`, `type_expr`, `call_args`, `primary`)
  - → `front.rs` (loads packages; `check_program`)
  - → `check.rs`, the type checker:
    - `binary` (operators, `||` on optionals), `coerce`, `case`, `implement` (interfaces), `bind` / `bind_tparams` / `instance` (generics);
    - `const_call` / `const_call_named` (`Type.method`), `call_def`, `format` (the `%` verbs), `type_from`.
  - → `tast.rs` (the typed AST). `Ty` is a structural tree, but a struct's or enum's field list is an `Rc<Vec<..>>`: cloning a type is cheap, so build one with `Rc::new(fields)` and never deep-copy fields (`fs.to_vec()`) on a hot path. Tree walks: `prove::each_child` already enters blocks and `Seq`s; don't also walk their statements yourself (that doubles the work per nesting level).
  - → `lower.rs`, to LIR (`lir.rs`): `fmt_piece`, `iface_result`, `to_s`, `binary`, `shift`, `drop_idle_regions`.
  - → backends: `cgen.rs` (C), `jit.rs` (Cranelift), `rgen.rs` (a Rust "oracle" the acceptance tests compare against), `web/src/wasmgen.rs` (browser).
  - Also: `regions.rs` (where each allocation lives), `prove.rs` (removes checks it can prove), `derive.rs` (`#[derive(Json)]`), `fmt.rs` (formatter), `driver.rs` (the CLI and `alx test`).
- **Runtime:** C in `compiler/runtime/alx.c` / `alx.h`; the JIT links the same C. Rust oracle: `compiler/runtime/prelude.rs`. Browser: `web/src/rt.rs`.
- **`acceptance/`:** `run.alx` (the harness; register cases in `case_groups`), `cases/<name>.alx` + `.expected` (output) or `.expected_error` (a compile error, registered in the negative list).
- **`std/<go import path>/`:** each package's main file opens with a header mapping Go's API onto alx's and listing every difference.

## Changing the language or the compiler

- **Every backend:** a change goes into C, JIT, oracle and wasm (or wasm refuses it with an error, as it does for most FFI). Add an acceptance case and a GO-VS-RUBY.md row.
- **Problems you don't fix:** add them to `docs/notes/port-issues.md`.
- **Porting a Go package:** the rules and "done means" are in `docs/notes/porting-std.md`. Status of every package: STDLIB.md (regenerate with `alx run tools/stdlib_status.alx`).

## Git

- **Merging:** merge, never rebase (`git merge --no-edit main`).
- **Staging:** stage files by name, never whole directories; the person you work for has uncommitted work in `web/` and `PROGRESS.md`.
- **Commits:** commit in your worktree. Push only when told to.
