# A walk through alx-lsp

This covers everything in `tools/alx-lsp`. The test files and `testdata/` aren't covered; they're small, and `server_test.alx` shows how to drive `serve` with the fake.

## What the thing does

An editor starts `alx-lsp` and talks to it over stdin and stdout. Every time you type, the editor sends the whole buffer. The server writes the unsaved buffers to a temp file, runs `alx check --json` on the file you're in, and sends the diagnostics back. That's all it does today.

The compiler stays in Rust and the server never type-checks anything itself. If you want to know why an error appears, the answer is in the compiler. If you want to know why it appears late, or twice, or at the wrong column, the answer is here.

## main.alx

Thirty lines. It builds a buffered reader on stdin and a buffered writer on stdout, makes a temp directory for the overlay file, and calls `server.serve`.

Notice the `ALX_LSP_FAKE` branch. If that variable is set, the server answers from a canned file instead of running `alx`. Nearly all the tests use it. When you add a feature, you can usually write the test against the fake first.

The exit code is whatever `serve` returns: 0 after a clean `shutdown` and `exit`, 1 if the client left without shutting down, 2 if something failed hard.

## serve, from the top

`serve` in `server/loop.alx` is long, and it's long on purpose. That isn't an accident of growth. Read the header comment of that file before you try to tidy it, because the shape is the fix for a memory leak.

The state is a handful of local variables:

- `docs`: URI to the text of each open buffer.
- `uris`: the same URIs as a list.
- `vers`: URI to the version number the editor gave us.
- `results`: URI to the last check of that file (an `Entry`).
- `sent`: URI to the diagnostics JSON we last published, so we don't send the same thing twice.
- `dirty`: the buffers that changed since the last check.
- `ctx`: a small map that holds one thing, a `shutdown` request id waiting for a reply.

Before the loop, `handshake` answers requests until `initialize` arrives and returns a `Session` (position encoding, settings, workspace root, which `alx` to run). Anything else that arrives first gets "server not initialized".

## The loop: two halves

Each trip through the loop does two jobs, in this order.

**First: should we check now?** If `dirty` has something in it, and either a shutdown is waiting or `input_ready` says nothing more arrived in the last 150 ms, we run a check. That's the debounce. `input_ready` returns true right away when bytes are already buffered, otherwise it waits on stdin for up to 150 ms. Typing fast keeps returning true, so we keep reading messages and never check. When you stop, the wait times out, it returns false, and we check.

Inside the check, for each file that needs one (`plan` decides, more below):

1. Build the overlay JSON from every open buffer and write it to `overlays.json` in the temp dir.
2. Call `client.check!(req)`. This blocks. Nothing else runs while the compiler does.
3. Merge the result with what we had before (`merge`).
4. Publish diagnostics for every file the old or new result mentions.

**Second: read one message.** `read_frame` blocks until a whole message arrives. Parse it, then branch: requests get an answer (or an error, since we only support `initialize` and `shutdown`), notifications update the state.

The four notifications that matter are `didOpen`, `didChange`, `didSave` and `didClose`. Each one stores or removes the text and marks the file dirty. `didChange` also does something I want you to notice: before it replaces the stored text, it computes the edit between old and new (`position.diff`) and moves every stored diagnostic through that edit (`shift_diags`). So the squiggles stay put while you type, instead of jumping around until the next check lands.

## Why it's one task with no channel

We tried the obvious design first, a reader task feeding a channel. On 10,000 messages of 100 KB it went to 2.5 GB, because anything sent on a channel lives in the program region and is never freed. The single loop stayed under 15 MB. That measurement is in `docs/notes/lsp-probe-memory.alx` and the plan. So there is no `spawn` and no `Chan` anywhere in this server, and you shouldn't add one.

A consequence you'll feel: while the compiler runs, we're not reading. Messages wait in the pipe. That's fine, because a check takes tens of milliseconds, and the next loop trip sees them.

## The strange-looking parts, and why

Some of this code looks like it was written by someone who didn't trust the language. It was written by someone who measured it.

- **`fresh(...)` everywhere.** `fresh` copies a string byte by byte. A slice of the incoming message would keep the whole message, up to 100 KB, alive. Copy what you keep.
- **`docs` is never passed to a function.** The compiler only gives `docs` a region of its own, one that frees the old text when you replace it, while no function receives it and no named local holds one of its keys or texts. So `publish` takes a copy of one text, not the map. And `uris` exists so that no loop has to say `docs.keys`.
- **`uri_of(msg.params)` repeated instead of a local.** Same reason. A named local tying a key to the loop brings the leak back.

If you add a variable that holds a key or a text from `docs`, run the soak (`alx run acceptance/run.alx -k LSP`). It sends 10,000 edits of 100 KB and fails over 150 MB. It currently peaks at 22 MB. The underlying compiler limitation is port-issues #177. If someone fixes it, most of this care can go.

## state.alx

Pure helpers. They take values and return values, which is why they can be tested without a server.

`merge` is the one to understand. When the file you're editing has a syntax error, the compiler can't type-check the unit, so it reports only the syntax error. If we published that alone, every semantic error would vanish the moment you typed an unbalanced brace. So `merge` keeps the previous semantic diagnostics (already shifted through your edits) next to the new syntax errors. When the syntax is clean again, the fresh result replaces everything.

`plan` decides which open buffers to recheck after a change. The changed buffer, plus any other open buffer whose last check was in the same directory, or listed the changed file. That's how editing a package file refreshes the test file you have open next to it.

`lsp_diags` converts the compiler's diagnostics to LSP ones. For an open buffer it trusts the byte offsets (they were shifted through edits) and recomputes line and column from the text. For a file that isn't open it uses the line and column the compiler gave. Encoding matters here: Neovim takes UTF-8 columns, other clients want UTF-16, and `position` does the conversion.

`std_dir_for` decides whether to set `ALX_STD_DIR` for the compiler. If you're inside the alexandrite repo, or editing a std file, it points at the repo's `std/` so the compiler sees your unsaved std edits.

## Where to start if you're going to write parts

Small and safe places to begin:

- **A new setting.** Add a field to `Opts`, read it in `parse_opts`, pass it down in `compiler.Request`. The tests in `state_test.alx` show the shape.
- **A new notification.** A new arm in the `case method` block. Write it so it doesn't name locals that hold `docs` keys or texts, then run the soak.
- **A new request**, like `textDocument/formatting`. You'll need a new `Session` capability, an arm in the request branch, and some thought about the memory shape, because the answer comes from a subprocess like `alx fmt`.

Harder, and worth a conversation first: anything that wants an in-process syntax tree (document symbols, folding). The plan deliberately left that out. The compiler is the only parser the server talks to.

## The other packages

I've now read these too. Short versions, with the one thing in each worth knowing.

**compiler.** `Client` is an interface with one method, `check!(req) -> Outcome`. `ExecClient` runs `alx check --json --overlays <file> [--unit u] <focus>`, with `ALX_STD_DIR` set when the request carries one. The overlays go through a file, not the child's stdin. That's deliberate: anything reachable from a `Cmd` gets pinned in memory for good, and a 100 KB text per check can't afford that. `Request` stays small for the same reason.

A check that can't produce a report comes back as an `Outcome` with a `failure` string and the child's stderr, and the server turns that into one diagnostic at the top of the file plus a log line. The one place the child is run is `run`, and it sets a 20 second timeout (`CHECK_TIMEOUT_NS`). `FakeClient` answers from a list of rules: if the focused file's overlay text contains a rule's string, you get that rule's canned report. `$FILE` in a rule means the focused file.

**jsonrpc.** `read_frame` reads headers until the blank line, takes `Content-Length`, then reads exactly that many bytes. `parse` finds `id`, `method` and the raw text of `params` without building a tree. `params` stays a string, and callers dig into it with `scan.alx`'s `string_at`, `int_at` and `last_text` (the last entry of `contentChanges`, which is the whole text under full sync). That scanner exists because `encoding/json` pins its buffers to the program region. It assumes well-formed JSON from the client: on bad input a lookup returns `none`, it doesn't panic. Ids are kept as raw JSON text so a number or a string comes back as it went in.

**protocol.** The structs the server reads and writes, with `#[derive(Json)]`, plus `uri_to_path` and `path_to_uri` (percent-encoding included) and the builders for `publishDiagnostics` and `logMessage`. If you add a capability to the `initialize` reply, it's `ServerCaps` here and the construction in `handshake`.

**position.** `Index` holds a text and its line starts so converting a batch of offsets costs one scan; it's the only place that knows about UTF-16. `diff(old, new)` finds the edit as the common prefix and suffix, which is all full-text sync lets us know, and `Edit.shift` moves an old offset to the new text: before the edit it stays, after it it moves by the size change, inside it lands at the start. That's the whole mechanism behind squiggles that stay put while you type. It's approximate on purpose. Two edits in one message are treated as one bigger edit.

## Things that are true today and might surprise you

- A check that takes longer than 20 seconds is killed and shows up as a "cannot run alx" diagnostic.
- The server checks one file at a time, in order. Two buffers changed together get two sequential runs.
- A unit with an uncalled function reports errors in it. That's deliberate (the plan's "check all"), and it's why the compiler's own std had to be cleaned up first.
- There are no hover, completion or go-to-definition. Diagnostics only.
