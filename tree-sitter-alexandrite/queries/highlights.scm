; ====================================================================
; Alexandrite Syntax Highlighting Queries (queries/highlights.scm)
; Standard capture groups compatible with Helix, Neovim, Zed, GitHub
; ====================================================================

; --- Keywords ---

[
  "def"
  "fn"
  "ƒ"
] @keyword.function

[
  "return"
] @keyword.return

[
  "if"
  "unless"
  "else"
  "elsif"
  "case"
] @keyword.conditional

[
  "while"
  "for"
  "loop"
  "break"
  "next"
  "in"
] @keyword.repeat

[
  "import"
  "require"
] @keyword.import

[
  "struct"
  "enum"
  "error"
  "interface"
  "refine"
  "using"
] @keyword.type

[
  "pub"
  "extern"
] @keyword.modifier

[
  "defer"
] @keyword.exception

[
  "spawn"
] @keyword.coroutine

[
  "like"
] @keyword.operator

[
  "test"
  "bench"
  "example"
  "outputs"
] @keyword

; Special soft keyword: fail
((identifier) @keyword.exception
  (#eq? @keyword.exception "fail"))

; Builtin variables
((identifier) @variable.builtin
  (#any-of? @variable.builtin "self" "it"))

; Constants & Builtins
(boolean) @boolean
(nil) @constant.builtin
(none) @constant.builtin

; Numbers
(integer) @number
(float) @number.float

; Strings & Literals
(string) @string
(raw_string) @string
(string_content) @string
(command_literal) @string
(command_content) @string
(heredoc) @string
(heredoc_content) @string
(heredoc_end) @label

(escape_sequence) @string.escape
(symbol) @string.special.symbol
(block_symbol) @string.special.symbol
(interpolation) @embedded
(splice) @embedded

; Comments
(comment) @comment

; Types
(primitive_type) @type.builtin
(type_identifier) @type
(scoped_type_identifier) @type
(type_parameter name: (type_identifier) @type.parameter)
(constant) @constant

; Functions & Definitions
(function_definition
  name: (identifier) @function)

(function_definition
  name: (operator_name) @function)

(method_definition
  name: (identifier) @function.method)

(method_definition
  name: (operator_name) @function.method)

(interface_method_definition
  name: (identifier) @function.method)

(interface_method_definition
  name: (operator_name) @function.method)

; Function & Method Calls
(call_expression
  function: (identifier) @function.call)

(call_expression
  function: (member_expression
    property: (identifier) @function.method.call))

(block_call
  function: (identifier) @function.call)

(command_statement
  function: (identifier) @function.call)

(command_statement
  function: (member_expression
    property: (identifier) @function.method.call))

; Parameters & Variables
(parameter
  name: (identifier) @variable.parameter)

(block_parameters
  (identifier) @variable.parameter)

(variant_parameter
  name: (identifier) @variable.parameter)

(keyword_argument
  name: (identifier) @variable.parameter)

(field_declaration
  name: (identifier) @variable.member)

(member_expression
  property: (identifier) @variable.member)

; Attributes & Directives
(attribute) @attribute
(directive) @attribute

; Operators
[
  "+"
  "-"
  "*"
  "/"
  "%"
  "**"
  "+%"
  "-%"
  "*%"
  "<<"
  ">>"
  "&"
  "|"
  "^"
  "&^"
  "~"
  "!"
  "=="
  "!="
  "<"
  "<="
  ">"
  ">="
  "<=>"
  "&&"
  "||"
  "="
  "+="
  "-="
  "*="
  "/="
  "%="
  "**="
  "&="
  "|="
  "^="
  "&^="
  "<<="
  ">>="
  "+%="
  "-%="
  "*%="
  ".."
  "..."
] @operator

(operator_name) @operator

; Delimiters & Punctuation
[
  ","
  ";"
  "."
  "?."
  ".~"
] @punctuation.delimiter

[
  "("
  ")"
  "["
  "]"
  "{"
  "}"
] @punctuation.bracket

[
  "=>"
  "->"
  ":"
  "?"
] @punctuation.special
