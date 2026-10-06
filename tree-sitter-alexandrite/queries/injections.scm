; Injections for Alexandrite (alx).
; Interpolations are parsed in place by the grammar (strings, heredocs and
; command literals), so nothing needs re-parsing there.

; Shell-like command literals: `ls -l #{dir}`.
((command_literal) @injection.content
  (#set! injection.language "bash")
  (#set! injection.include-children))

((comment) @injection.content
  (#set! injection.language "comment"))
