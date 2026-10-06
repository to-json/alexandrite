; Scopes and local variables for Alexandrite (alx).

[
  (source_file)
  (function_definition)
  (method_definition)
  (interface_method_definition)
  (lambda_expression)
  (block)
  (for_statement)
  (case_arm)
  (select_arm)
] @local.scope

(parameter
  name: (identifier) @local.definition)

(block_parameters
  name: (identifier) @local.definition)

(for_statement
  variable: (identifier) @local.definition)

(declaration_statement
  name: (identifier) @local.definition)

(assignment_expression
  left: (identifier) @local.definition)

(multi_assignment_statement
  left: (identifier) @local.definition)

(pattern_bindings
  (identifier) @local.definition)

(const_definition
  name: (constant) @local.definition)

(identifier) @local.reference
