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

`serve` in `server/loop.alx` is long, and it used to be long on purpose: its shape was the fix for a memory leak. The compiler fix for that leak (port-issues #177) let most of the odd shape go; read the header comment of that file for the rules that are left.

The state is a handful of local variables:

- `docs`: URI to the text of each open buffer.
- `vers`: URI to the version number the editor gave us.
- `results`: URI to the last check of that file (an `Entry`).
- `sent`: URI to the diagnostics JSON we last published, so we don't send the same thing twice.
- `dirty`: the buffers that changed since the last check.
- `ctx`: a small map that holds one thing, a `shutdown` request id waiting for a reply.
- `index`: URI to the outline of that buffer (M2).
- `tabs`: URI to the last good type table of that buffer (M2.6).

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

- **`fresh(...)` on what outlives an iteration.** `fresh` copies a string byte by byte. A slice of the incoming message would keep the whole message, up to 100 KB, alive. So the URIs pushed onto `dirty` and the held `shutdown` id are copies, because they're still around after the message is gone. Texts stored in `docs` are not copied any more: the region analysis now sees that the map owns them and frees the old one when you replace it.
- **There used to be more.** Until the compiler fix for port-issues #177, `docs` could never be handed to a function, no local could name one of its keys or texts, and a list `uris` stood in for `docs.keys`. That's all gone: `serve` now names `uri` and `text` like any code would, and `write_overlays` and `publish` take the map or its texts directly. The soak went from 21 MB to 12 MB when the workarounds came out.
- **`answer` copies its arguments.** That one is still needed (port-issues #272, and see "The memory story" in M2 below).

If you change how `serve` holds anything, run the soak (`alx run acceptance/run.alx -k LSP`). It sends 10,000 edits of 100 KB, then 10,000 each of completion, documentSymbol and member completion requests, and fails over 150 MB. It currently peaks at about 25 MB.

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

Harder, and worth a conversation first: anything that needs to know types (hover, member completion after `x.`). The server has no syntax tree and no types; see M2 below for what it does instead.

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
- There is no hover, no references and no rename.
- Every compiler run costs about 80 KB that is never freed (port-issues #273, in os/exec). A very long session grows by that much per check.

## M2: symbols, definition, completion

Milestone 2 added three requests: `textDocument/documentSymbol`, `textDocument/definition` and `textDocument/completion`. All three answer from one thing, the outline of a file. Nothing in them type-checks.

### outline

`outline/outline.alx` is a byte scanner. It walks the text once, counts braces, and at each statement start at brace depth 0 asks "is this a declaration?": `def`, `struct`, `enum`, `error`, `interface`, `refine`, `extern def`, `import`, `test`/`bench`/`example`, or a capitalized name followed by `=` or `:` (a constant). Inside the body of a type it asks only about `def`, and those come out as kind `method` with `owner` set to the type. That's the whole list the parser's `top_decl` knows, so it's the whole list here.

The tricky part is not finding declarations, it's not finding fake ones. A `}` inside a string, a comment, a command literal, a `#{}` with a string inside it, a heredoc body, or an attribute like `#[json("a]b")]` must not count. The scanner skips each of those the way the lexer does (the functions even have the lexer's names: `interp_end`, `quoted_end`). If you change how the lexer reads a literal, change it here too, and the acceptance check below will tell you if you forgot.

It never fails. On a broken file it does what the parser's `resync` does: a declaration keyword at column 0 inside a brace that never closed is taken to end that brace. So one unfinished function doesn't hide the rest of the file from the symbol list, which is exactly when you want the list.

Why not ask the compiler? Because the owner wanted the feature logic in alx, and because a scanner is fast enough not to need a cache: an 840 KB package directory reads and scans in a few milliseconds.

### The oracle

How do we know the scanner agrees with the parser? `alx parse --decls FILE...` (driver.rs `parse_decls`) prints what the real parser found: kind, name, owner, line and byte offset of every declaration. `testdata/outline_dump.alx` prints the same from the scanner. The `LSP` acceptance group runs both over every tracked `.alx` file and requires identical output for every file that parses. Today that's all of them, with an empty allowlist. If you make the scanner smarter, that check is your safety net; if it fails, the message names the file and the first lines that differ.

### In the server

`serve` keeps one more map, `index`: URI to the outline of that buffer, rebuilt on every `didOpen` and `didChange`. It lives and is freed like `docs`.

A request is answered by `answer` in `server/nav.alx`. The loop gives it the buffer's text, its outline, and the texts of the other open buffers in the same directory. `answer` copies them before doing anything. That looks paranoid. It isn't, see below.

- **documentSymbol** turns the outline into `DocumentSymbol`s. A method becomes a child of the type before it. Ranges go through `position`, so they're in the client's encoding.
- **definition** finds the word under the cursor and the word before a `.` if there is one. `pkg.name` where the buffer imports `pkg`: look in that package's directory, under the std dir or under the module root from `alx.mod`, for a `pub` top-level name. `Type.name`: methods of `Type`. Anything else: a top-level name in the buffer, then the other open buffers of the directory, then the directory's files on disk. If that finds nothing, it's probably a method on a value whose type we don't know, so it returns every method of that name.
- **completion** offers keywords, names from the same places, imported package names, `pub` names after `pkg.`, methods after `Type.`, and package paths inside `import "`. Those last ones come with a `textEdit`, because an editor that thinks `/` ends a word would otherwise paste `net/http` over just `ht`.

Files on disk are read for each request and dropped. No cache means no cache to keep bounded or invalidate.

### The memory story, again

The first version of this leaked 2 GB in the soak. Twice, for two different reasons, and neither was visible in the code.

The scanner made a small struct (`Head`) for each declaration it looked at, and one of its fields was a string taken from a list literal (`for w in ["struct", "enum", ...]`). That alone made the compiler put every `Head` in the program region, and with it a lot of the scan. 2 MB per 100 KB scanned, forever. `Head` now holds only numbers. port-issues #271.

Then each request leaked about a megabyte, because a helper combined its arguments (`[cur] + others`). The region analysis then treats everything the caller passed as stored in more than one place and gives up on freeing it. The helpers now loop over their arguments instead of joining them, and copy the strings they keep in a map. port-issues #272.

The way to find these is `alx explain mem main.alx` (from tools/alx-lsp) and looking for "program region" or "grows without bound" in `serve` and the `nav` functions. Run the soak after any change to `nav.alx`, `members.alx` or `outline.alx`.

After the #177 fix I checked both again by putting the old code back. Both still leak, so both workarounds stay.

## M2.6: members after `.`

Type `b.` where `b` is a `strings.Builder`, and the list should hold `write_string!` and friends. The outline can't do that, because it doesn't know what `b` is. The compiler does, so we ask it. `alx check --json --types` adds two things to its report: `types`, the type of every local, parameter, `self`, `it` and receiver expression in the focused file, with their spans; and `members`, the fields and methods of each of those types. That's in `server/members.alx`.

### When we ask

Only when a completion request comes in right after a `.` on a value (`member_dot` says no for `Type.` and `pkg.`, which the outline already answers). Not on every diagnostics check: the type data can be bigger than the file, and nearly every check would throw it away.

There's a catch. When you've just typed `b.`, the buffer doesn't parse, and a buffer that doesn't parse gets no types. So the server checks a slightly different text: the same buffer with the `.` and whatever you typed after it replaced by spaces (`blank_member`). `b.wr` becomes `b   `, which parses, and every offset stays where it was. The binding of `b` is in the answer.

### Which entry is the receiver

`receiver_at` looks backward from the dot. A plain identifier (`b`, `self`, `it`) is looked up by name: the nearest binding before the dot, inside the def that holds the dot (the outline tells us where that def starts, so a `b` from another function doesn't count). Anything else, a call like `f(x).` or a chain like `a.b.`, is looked up as a `recv` entry whose span ends at the dot. Blanking out the dot means the fresh check never has that entry, so chains come only from an older table that saw the chain whole. That's the next part.

### The last good table

A check that reports no errors gives a good table, and the server keeps it per buffer in `tabs`. On every edit its spans move through the edit, like the diagnostics do (`shift_tab`). When the fresh check fails, say because there's a second broken spot elsewhere in the file, or the def you're in doesn't type-check with the blank in it, the receiver is looked up in that table instead. If neither knows it, the answer is an empty list, not a guess.

And when the last good table already knows the receiver, the server doesn't run the compiler at all. That's partly speed, and partly memory: every run of a child process leaks about 80 KB in os/exec (port-issues #273), so runs we don't need are runs we shouldn't make.

### Not decoding

The output is never decoded into structs with `from_json`, because derived decoding keeps what it decodes alive forever (port-issues #176). `type_table` walks the `types` array with the same scanner the message reader uses and keeps small structs; `members` stays a JSON string, and `member_items` pulls out one key's array when it needs it.

### Tests

`members_test.alx` covers receiver detection and the table lookups. `testdata/session_member.in` is a scripted session that runs the real compiler (the replay tool's `real:ALX` mode): a std type, a local struct, `self`, a chain from the last good table, and a buffer that doesn't parse. The soak adds 10,000 member completions; with the fake compiler's table they're answered from the last good table after the first run.

