; Alexandrite (alx) highlights. Capture names follow nvim-treesitter.
; Where several patterns match a node the later one wins (tree-sitter's
; highlighter, Neovim), so general patterns come first.

(identifier) @variable
(type_identifier) @type
(constant) @type

; --- Keywords ---------------------------------------------------------------

[
  "def"
  "fn"
  "ƒ"
] @keyword.function

[
  "return"
  "fail"
] @keyword.return

[
  "if"
  "unless"
  "else"
  "elsif"
  "case"
  "select"
  "when"
] @keyword.conditional

[
  "while"
  "for"
  "in"
  "break"
] @keyword.repeat

(next_statement) @keyword.repeat

"import" @keyword.import

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

"defer" @keyword

"spawn" @keyword.coroutine

[
  "like"
  "is"
] @keyword.operator

[
  "test"
  "bench"
  "example"
  "outputs"
] @keyword

; --- Literals ---------------------------------------------------------------

(boolean) @boolean
(nil) @constant.builtin
(none) @constant.builtin

(integer) @number
(float) @number.float
(imaginary) @number.float

(string) @string
(raw_string) @string
(string_content) @string
(command_literal) @string.special
(command_content) @string.special
(heredoc) @string
(heredoc_body) @string
(heredoc_content) @string
(heredoc_end) @label

(escape_sequence) @string.escape
(symbol) @string.special.symbol
(block_symbol) @string.special.symbol

(interpolation
  "#{" @punctuation.special
  "}" @punctuation.special) @none
(splice
  "#{*" @punctuation.special
  "}" @punctuation.special) @none

(comment) @comment

; --- Attributes -------------------------------------------------------------

(directive
  "#![" @punctuation.special
  name: (identifier) @attribute
  "]" @punctuation.special)
(directive
  argument: (identifier) @constant)
(attribute
  "#[" @punctuation.special
  name: (identifier) @attribute
  "]" @punctuation.special)

; --- Types ------------------------------------------------------------------

((type_identifier) @type.builtin
  (#any-of? @type.builtin
    "Int" "I8" "I16" "I32" "I64" "U8" "U16" "U32" "U64" "Byte" "Rune"
    "Float" "F32" "F64" "Complex" "Str" "Bool" "Ptr" "Unit" "Error"
    "Map" "Chan" "Mutex" "Atomic"))

(type_parameter
  name: (type_identifier) @type.parameter)

(scoped_type_identifier
  scope: (identifier) @module)

(variant_declaration
  name: (type_identifier) @constant)

(variant_pattern
  (constant) @constant)

; `Point.new`, `Color.Red`: a capitalized name in an expression is a type or
; a variant; ALL_CAPS names are constants.
((constant) @constant
  (#match? @constant "^[A-Z][A-Z0-9_]*$"))

; --- Definitions ------------------------------------------------------------

(function_definition
  name: (identifier) @function)
(function_definition
  name: (operator_name) @operator)
(extern_definition
  name: (identifier) @function)
(method_definition
  name: (identifier) @function.method)
(method_definition
  name: (operator_name) @operator)
(interface_method_definition
  name: (identifier) @function.method)
(interface_method_definition
  name: (operator_name) @operator)

(const_definition
  name: (constant) @constant)

(test_declaration
  name: (string) @string.special)

; --- Variables and fields ---------------------------------------------------

((identifier) @variable.builtin
  (#any-of? @variable.builtin "self" "it"))

"self" @variable.builtin

(parameter
  name: (identifier) @variable.parameter)
(block_parameters
  name: (identifier) @variable.parameter)
(variant_parameter
  name: (identifier) @variable.member)
(keyword_argument
  name: (identifier) @variable.parameter)
(field_declaration
  name: (identifier) @variable.member)
(pair
  key: (identifier) @variable.member)
(member_expression
  property: (identifier) @variable.member)
(member_expression
  property: (constant) @type)

(import_statement
  alias: (identifier) @module)

; --- Calls ------------------------------------------------------------------

(call_expression
  function: (identifier) @function.call)
(call_expression
  function: (member_expression
    property: (identifier) @function.method.call))
(command_call
  function: (identifier) @function.call)

((command_call
  function: (identifier) @function.builtin)
  (#any-of? @function.builtin
    "puts" "print" "p" "panic" "assert" "assert_eq" "assert_panics"))

((call_expression
  function: (identifier) @function.builtin)
  (#any-of? @function.builtin
    "puts" "print" "p" "panic" "assert" "assert_eq" "assert_panics"
    "format" "sprintf" "copy" "caller_location"))

; `loop { ... }` is a call of the builtin `loop`.
(call_expression
  function: (identifier) @keyword.repeat
  (#eq? @keyword.repeat "loop"))

; --- Operators and punctuation ----------------------------------------------

[
  "+" "-" "*" "/" "%" "**" "+%" "-%" "*%"
  "<<" ">>" "&" "|" "^" "&^" "~" "!"
  "==" "!=" "<" "<=" ">" ">=" "<=>" "&&" "||"
  "=" "+=" "-=" "*=" "/=" "%=" "**=" "&=" "|=" "^=" "&^=" "<<=" ">>="
  "+%=" "-%=" "*%="
  ".." "..."
  "?"
  "@"
] @operator

(operator_name) @operator

[
  ","
  ";"
  "."
  "?."
  ".~"
  ":"
] @punctuation.delimiter

[
  "("
  ")"
  "["
  "]"
  "{"
  "}"
] @punctuation.bracket

(block_parameters
  "|" @punctuation.bracket)

[
  "=>"
  "->"
] @punctuation.special
