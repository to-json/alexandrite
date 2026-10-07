# alexandrite in Neovim (0.11+)

Three pieces: filetype detection, the language server (`alx-lsp`: the compiler's
errors and warnings on unsaved buffers, document symbols, go to definition,
completion), and tree-sitter (highlighting and
instant syntax-error squiggles).

## 1. Build and install the server

```sh
alx build --release -o ~/.local/bin/alx-lsp tools/alx-lsp/main.alx
```

The server drives the compiler (`alx check --json`), so `alx` must be new
enough to have that mode. It finds it from the `alx.path` setting, else next
to the `alx-lsp` binary, else on `PATH`. The server logs the compiler's
`alx --version` on start (`:LspLog`).

## 2. Filetype and server

```lua
vim.filetype.add({ extension = { alx = 'alexandrite' } })

vim.lsp.config('alx', {
  cmd = { 'alx-lsp' },
  filetypes = { 'alexandrite' },
  root_markers = { 'alx.mod', '.git' },
  -- all optional
  init_options = {
    alx = {
      path = nil,    -- the alx binary
      stdDir = nil,  -- ALX_STD_DIR for the compiler (set automatically in the alexandrite repo)
      unit = nil,    -- 'script' or 'package' to override the guess
    },
  },
})
vim.lsp.enable('alx')
```

Neovim offers `utf-8` positions; the server uses them. Clients that do not
get UTF-16 columns.

What it does:

- **Diagnostics** (full-text sync). It checks 150 ms after the last edit;
  while a buffer has a syntax error, the last type errors stay (moved along
  with your edits) until the next successful check.
- **Document symbols** (`gO`, or a picker's symbol list): every top-level
  declaration, with methods nested under their struct, enum, error,
  interface or refinement. Works on files that don't parse.
- **Go to definition** (`gd` with `vim.lsp.buf.definition`, or `<C-]>`):
  top-level names and `Type.method` in the buffer, the other open buffers of
  its directory and the directory's files on disk; `pkg.name` for an
  `import "x/y"` in the buffer, when the package is in the std dir (found
  automatically inside the alexandrite repo; else set `alx.stdDir` or
  `ALX_STD_DIR`) or under your module (the nearest `alx.mod`'s `module`
  path). A method called on a value (`x.area`) lists every method of that
  name, since the server doesn't know `x`'s type. Required modules (the
  module cache) are not searched.
- **Completion** (`<C-x><C-o>`, or a completion plugin): keywords, the
  names of the buffer and its package directory, imported package names;
  after `pkg.` the package's `pub` names; after `Type.` its methods; inside
  `import "` the std and module package paths. No member completion on
  values yet (`x.` offers nothing).

None of this type-checks: names come from a scan of the declarations (the
compiler's own check only drives diagnostics). Hover, references and rename
are not there yet.

Memory: the server's peak RSS stays bounded however long the session (the
soak test: 10,000 edits of a 100 KB document, then 10,000 completion and
10,000 document-symbol requests, peak at about 22 MB).

## 3. Tree-sitter: parser and syntax errors

Register the parser from this repository (`tree-sitter-alexandrite/`):

```lua
vim.api.nvim_create_autocmd('User', {
  pattern = 'TSUpdate',
  callback = function()
    require('nvim-treesitter.parsers').alexandrite = {
      install_info = {
        path = vim.fn.expand('~/code/alexandrite/tree-sitter-alexandrite'),
        queries = 'queries',
      },
    }
  end,
})
-- then: :TSInstall alexandrite
vim.treesitter.language.register('alexandrite', 'alexandrite')
```

Without nvim-treesitter, build the parser yourself (`tree-sitter build -o
parser/alexandrite.so` in that directory, copy it to
`~/.config/nvim/parser/`) and copy `queries/*.scm` to
`~/.config/nvim/queries/alexandrite/`.

Turn `ERROR` and `MISSING` nodes into diagnostics, in a namespace of their own
(they show beside the server's, and neither clears the other):

```lua
local ns = vim.api.nvim_create_namespace('alx-treesitter')

local function syntax_diagnostics(buf)
  local ok, parser = pcall(vim.treesitter.get_parser, buf, 'alexandrite')
  if not ok or not parser then return end
  local tree = parser:parse()[1]
  local diags = {}
  local function walk(node)
    if node:type() == 'ERROR' or node:is_missing() then
      local sr, sc, er, ec = node:range()
      if node:is_missing() then ec = ec + 1 end -- give a zero-width node a column
      diags[#diags + 1] = {
        lnum = sr, col = sc, end_lnum = er, end_col = ec,
        severity = vim.diagnostic.severity.ERROR,
        source = 'tree-sitter',
        message = node:is_missing() and ('missing ' .. node:type()) or 'syntax error',
      }
      if node:type() == 'ERROR' then return end -- one squiggle per error region
    end
    for child in node:iter_children() do walk(child) end
  end
  walk(tree:root())
  vim.diagnostic.set(ns, buf, diags)
end

vim.api.nvim_create_autocmd({ 'BufEnter', 'TextChanged', 'InsertLeave', 'TextChangedI' }, {
  pattern = '*.alx',
  callback = function(args) syntax_diagnostics(args.buf) end,
})
```

Both the grammar and the compiler's parser can disagree on odd input; the
compiler is the authority (its error is the one the server shows). The
tree-sitter squiggles are for instant feedback and for several errors at once.
