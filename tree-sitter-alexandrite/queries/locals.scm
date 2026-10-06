; Scope and local variable tracking for Alexandrite

[
  (source_file)
  (block)
  (function_definition)
  (method_definition)
  (loop_statement)
  (for_statement)
  (while_statement)
] @local.scope

(parameter
  name: (identifier) @local.definition)

(block_parameters
  (identifier) @local.definition)

(declaration_statement
  name: (identifier) @local.definition)

(const_definition
  name: (constant) @local.definition)

(identifier) @local.reference
