/**
 * @file Alexandrite grammar for tree-sitter
 * @author Alexandrite Authors
 * @license Apache-2.0 WITH LLVM-exception
 */

/// <reference types="tree-sitter-cli/dsl" />
// @ts-check

const PREC = {
  command: 1,
  assign: 2,
  modifier: 3,
  ternary: 4,
  range: 5,
  or: 6,
  and: 7,
  equality: 8,
  comparison: 9,
  bitor: 10,
  bitand: 11,
  shift: 12,
  additive: 13,
  multiplicative: 14,
  unary: 15,
  power: 16,
  call: 17,
  member: 18,
  spawn: 19,
};

module.exports = grammar({
  name: 'alexandrite',

  extras: $ => [
    /\s/,
    $.comment,
  ],

  conflicts: $ => [
    [$.block, $.map_literal],
    [$.constant, $.type_identifier],
    [$.scoped_type_identifier, $._primary_expression],
    [$.command_argument_list],
    [$.case_expression, $.map_literal],
    [$._pattern, $.map_literal],
    [$._type, $.optional_type],
    [$.parenthesized_expression, $.argument_list],
    [$.tuple_literal, $.argument_list],
    [$.result_type],
    [$.expression_statement],
    [$.command_statement],
    [$.declaration_statement],
    [$.multi_assignment_statement],
    [$.function_definition],
    [$.parenthesized_type, $.function_type],
    [$.function_type, $.tuple_type],
    [$._type, $.generic_type],
    [$.command_statement, $._primary_expression],
    [$.multi_assignment_statement, $._primary_expression],
    [$._statement_modifier, $.if_expression],
    [$._statement_modifier, $.unless_expression],
    [$._primary_expression, $.block_call],
    [$._no_block_primary_expression, $.block_call],
    [$._no_block_call_expression, $.call_expression],
    [$._primary_expression, $._no_block_primary_expression],
    [$._no_block_binary_expression, $.binary_expression],
    [$.command_statement, $._primary_expression, $.block_call],
  ],

  word: $ => $.identifier,

  rules: {
    source_file: $ => repeat($._top_level_item),

    _top_level_item: $ => choice(
      $.directive,
      $.import_statement,
      $.const_definition,
      $.struct_definition,
      $.enum_definition,
      $.error_definition,
      $.interface_definition,
      $.refinement_definition,
      $.function_definition,
      $.test_declaration,
      $._statement
    ),

    // --- Directives and Attributes ---
    directive: $ => seq(
      '#![',
      field('name', $.identifier),
      optional(seq('(', field('argument', $.identifier), ')')),
      ']'
    ),

    attribute: $ => seq(
      '#[',
      field('name', $.identifier),
      optional(seq(
        '(',
        optional($.attribute_arguments),
        ')'
      )),
      ']'
    ),

    attribute_arguments: $ => sepBy1(',', choice(
      $.identifier,
      $.type_identifier,
      $.string,
      seq($.identifier, ':', choice($.identifier, $.type_identifier, $.string, $.integer, $.boolean))
    )),

    // --- Imports ---
    import_statement: $ => seq(
      choice('import', 'require'),
      optional(field('alias', $.identifier)),
      field('path', $.string)
    ),

    // --- Constants ---
    const_definition: $ => seq(
      optional('pub'),
      field('name', $.constant),
      optional(seq(':', field('type', $._type))),
      '=',
      field('value', $._expression)
    ),

    // --- Structs ---
    struct_definition: $ => seq(
      repeat($.attribute),
      optional('pub'),
      'struct',
      field('name', $.type_identifier),
      optional(field('type_parameters', $.type_parameters)),
      field('body', $.struct_body)
    ),

    struct_body: $ => seq(
      '{',
      repeat(choice(
        $.field_declaration,
        $.method_definition
      )),
      '}'
    ),

    field_declaration: $ => seq(
      repeat($.attribute),
      field('name', choice($.identifier, $.type_identifier)),
      ':',
      field('type', $._type),
      optional(',')
    ),

    // --- Enums & Errors ---
    enum_definition: $ => seq(
      repeat($.attribute),
      optional('pub'),
      'enum',
      field('name', $.type_identifier),
      optional(field('type_parameters', $.type_parameters)),
      field('body', $.enum_body)
    ),

    error_definition: $ => seq(
      repeat($.attribute),
      optional('pub'),
      'error',
      field('name', $.type_identifier),
      optional(field('type_parameters', $.type_parameters)),
      field('body', $.enum_body)
    ),

    enum_body: $ => seq(
      '{',
      repeat(choice(
        $.variant_declaration,
        $.method_definition
      )),
      '}'
    ),

    variant_declaration: $ => seq(
      repeat($.attribute),
      field('name', $.type_identifier),
      optional(field('parameters', $.variant_parameters)),
      optional(',')
    ),

    variant_parameters: $ => seq(
      '(',
      sepBy(',', $.variant_parameter),
      optional(','),
      ')'
    ),

    variant_parameter: $ => seq(
      repeat($.attribute),
      optional(seq(field('name', $.identifier), ':')),
      field('type', $._type)
    ),

    // --- Interfaces ---
    interface_definition: $ => seq(
      optional('pub'),
      'interface',
      field('name', $.type_identifier),
      field('body', $.interface_body)
    ),

    interface_body: $ => seq(
      '{',
      repeat($.interface_method_definition),
      '}'
    ),

    interface_method_definition: $ => seq(
      repeat($.attribute),
      'def',
      field('name', $._method_name),
      optional(field('type_parameters', $.type_parameters)),
      optional(field('parameters', $.parameters)),
      optional(field('return_type', $.return_type)),
      optional(field('body', $.block))
    ),

    // --- Refinements ---
    refinement_definition: $ => seq(
      optional('pub'),
      'refine',
      field('name', $.type_identifier),
      'for',
      field('target', $._type),
      field('body', $.refinement_body)
    ),

    refinement_body: $ => seq(
      '{',
      repeat($.method_definition),
      '}'
    ),

    // --- Functions & Methods ---
    function_definition: $ => seq(
      repeat($.attribute),
      optional('pub'),
      choice(
        seq(
          'extern',
          field('keyword', choice('def', 'fn', 'ƒ')),
          field('name', $._method_name),
          optional(field('type_parameters', $.type_parameters)),
          optional(field('parameters', $.parameters)),
          optional(field('return_type', $.return_type)),
          optional(seq('=', field('link_name', $.string)))
        ),
        seq(
          field('keyword', choice('def', 'fn', 'ƒ')),
          field('name', $._method_name),
          optional(field('type_parameters', $.type_parameters)),
          optional(field('parameters', $.parameters)),
          optional(field('return_type', $.return_type)),
          choice(
            field('body', $.block),
            seq('=', field('link_name', $.string))
          )
        )
      )
    ),

    method_definition: $ => seq(
      repeat($.attribute),
      optional('pub'),
      field('keyword', choice('def', 'fn', 'ƒ')),
      field('name', $._method_name),
      optional(field('type_parameters', $.type_parameters)),
      optional(field('parameters', $.parameters)),
      optional(field('return_type', $.return_type)),
      field('body', $.block)
    ),

    _method_name: $ => choice(
      $.identifier,
      $.operator_name,
      seq('self', '.', choice($.identifier, $.operator_name))
    ),

    operator_name: $ => choice(
      '+', '-', '*', '/', '%', '==', '<=>', '[]', '<<', '>>', '**'
    ),

    // --- Tests ---
    test_declaration: $ => seq(
      field('kind', choice('test', 'bench', 'example')),
      field('name', $.string),
      field('body', $.block),
      optional(seq(
        'outputs',
        field('expected_output', choice(
          $.string,
          seq($.heredoc, $.heredoc_body)
        ))
      ))
    ),

    // --- Type Parameters & Parameters ---
    type_parameters: $ => seq(
      '[',
      sepBy1(',', $.type_parameter),
      optional(','),
      ']'
    ),

    type_parameter: $ => seq(
      field('name', $.type_identifier),
      optional(seq(':', field('bound', choice(
        seq('like', $.type_identifier),
        $._type
      ))))
    ),

    parameters: $ => seq(
      '(',
      sepBy(',', $.parameter),
      optional(','),
      ')'
    ),

    parameter: $ => seq(
      field('name', choice($.identifier, '_')),
      optional(seq(':', field('type', $._type)))
    ),

    return_type: $ => choice(
      seq('->', field('type', $._type)),
      '~'
    ),

    // --- Types ---
    _type: $ => choice(
      $.primitive_type,
      $.type_identifier,
      $.scoped_type_identifier,
      $.generic_type,
      $.array_type,
      $.fixed_array_type,
      $.handle_type,
      $.optional_type,
      $.result_type,
      $.function_type,
      $.tuple_type,
      $.parenthesized_type
    ),

    parenthesized_type: $ => seq('(', $._type, ')'),

    primitive_type: $ => choice(
      'Int', 'I8', 'I16', 'I32', 'I64',
      'U8', 'U16', 'U32', 'U64',
      'Byte', 'Rune', 'Float', 'Str', 'Bool',
      'Ptr', 'Unit', 'Error'
    ),

    scoped_type_identifier: $ => seq(
      field('scope', $.identifier),
      '.',
      field('name', $.type_identifier)
    ),

    generic_type: $ => seq(
      field('name', choice($.primitive_type, $.type_identifier, $.scoped_type_identifier)),
      field('type_arguments', $.type_arguments)
    ),

    type_arguments: $ => seq(
      token.immediate('['),
      sepBy1(',', $._type),
      optional(','),
      ']'
    ),

    array_type: $ => seq(
      '[',
      field('element', $._type),
      ']'
    ),

    fixed_array_type: $ => seq(
      '[',
      field('element', $._type),
      ';',
      field('length', $._expression),
      ']'
    ),

    handle_type: $ => seq(
      '@',
      field('name', choice($.type_identifier, $.scoped_type_identifier)),
      optional(field('type_arguments', $.type_arguments))
    ),

    optional_type: $ => seq(
      choice(
        $.primitive_type,
        $.type_identifier,
        $.scoped_type_identifier,
        $.generic_type,
        $.array_type,
        $.fixed_array_type,
        $.handle_type,
        $.function_type,
        $.tuple_type,
        $.parenthesized_type
      ),
      '?'
    ),

    result_type: $ => seq(
      '~',
      field('type', $._type),
      optional(seq(
        '<',
        sepBy1('|', choice($.type_identifier, $.scoped_type_identifier)),
        '>'
      ))
    ),

    function_type: $ => seq(
      '(',
      sepBy(',', $._type),
      ')',
      '->',
      field('return', $._type)
    ),

    tuple_type: $ => seq(
      '(',
      $._type,
      repeat1(seq(',', $._type)),
      optional(','),
      ')'
    ),

    // --- Statements ---
    _statement: $ => choice(
      $.declaration_statement,
      $.multi_assignment_statement,
      $.while_statement,
      $.for_statement,
      $.loop_statement,
      $.break_statement,
      $.next_statement,
      $.return_statement,
      $.defer_statement,
      $.using_statement,
      $.command_statement,
      $.expression_statement,
      $.empty_statement
    ),

    empty_statement: $ => ';',

    multi_assignment_statement: $ => seq(
      choice($.identifier, $.index_expression, $.member_expression),
      repeat1(seq(',', choice($.identifier, $.index_expression, $.member_expression))),
      '=',
      sepBy1(',', $._expression),
      optional($._statement_modifier)
    ),

    loop_statement: $ => seq(
      'loop',
      field('body', $.block)
    ),

    _statement_modifier: $ => prec.dynamic(-1, choice(
      seq('if', field('condition', $._no_block_expression)),
      seq('unless', field('condition', $._no_block_expression))
    )),

    declaration_statement: $ => seq(
      field('name', $.identifier),
      ':',
      field('type', $._type),
      '=',
      field('value', $._expression),
      optional($._statement_modifier)
    ),

    while_statement: $ => seq(
      'while',
      field('condition', $._no_block_expression),
      field('body', $.block)
    ),

    for_statement: $ => seq(
      'for',
      sepBy1(',', field('variable', choice($.identifier, '_'))),
      'in',
      field('collection', $._no_block_expression),
      field('body', $.block)
    ),

    break_statement: $ => prec.right(seq(
      'break',
      optional($._expression),
      optional($._statement_modifier)
    )),

    next_statement: $ => prec.right(seq(
      'next',
      optional($._statement_modifier)
    )),

    return_statement: $ => prec.right(seq(
      'return',
      optional(sepBy1(',', $._expression)),
      optional($._statement_modifier)
    )),

    defer_statement: $ => seq(
      'defer',
      field('expression', $._expression)
    ),

    using_statement: $ => seq(
      'using',
      field('name', choice(
        $.type_identifier,
        seq($.type_identifier, '.', $.type_identifier),
        seq($.identifier, '.', $.type_identifier)
      ))
    ),

    command_statement: $ => seq(
      field('function', $.identifier),
      field('arguments', $.command_argument_list),
      optional($._statement_modifier)
    ),

    expression_statement: $ => seq(
      $._expression,
      optional($._statement_modifier)
    ),

    // --- Blocks and Lambdas ---
    block: $ => seq(
      '{',
      optional(field('parameters', $.block_parameters)),
      repeat($._statement),
      '}'
    ),

    block_parameters: $ => seq(
      '|',
      sepBy(',', field('name', choice($.identifier, '_'))),
      optional(','),
      '|'
    ),

    lambda_expression: $ => seq(
      '->',
      optional(field('parameters', $.parameters)),
      optional(field('return_type', $.return_type)),
      field('body', $.block)
    ),

    // --- Case & Select Expressions ---
    case_expression: $ => seq(
      'case',
      optional(field('subject', $._no_block_expression)),
      '{',
      repeat($.case_arm),
      '}'
    ),

    case_arm: $ => seq(
      field('pattern', sepBy1('|', $._pattern)),
      '=>',
      field('body', choice($.block, $._statement)),
      optional(choice(',', ';'))
    ),

    _pattern: $ => choice(
      '_',
      $.variant_pattern,
      $._expression
    ),

    variant_pattern: $ => seq(
      field('name', choice(
        $.type_identifier,
        $.scoped_type_identifier,
        seq($.type_identifier, '.', $.type_identifier)
      )),
      optional(seq(
        '(',
        sepBy(',', choice($.identifier, '_')),
        optional(','),
        ')'
      ))
    ),

    select_expression: $ => seq(
      'select',
      '{',
      repeat($.select_arm),
      '}'
    ),

    select_arm: $ => seq(
      choice(
        seq(
          'when',
          optional(seq(field('binding', $.identifier), '=')),
          field('channel_op', $._expression)
        ),
        'else'
      ),
      '=>',
      field('body', choice($.block, $._statement)),
      optional(';')
    ),

    // --- If & Unless Expressions ---
    if_expression: $ => prec.right(seq(
      'if',
      field('condition', $._no_block_expression),
      field('consequence', $.block),
      repeat($.elsif_clause),
      optional($.else_clause)
    )),

    unless_expression: $ => prec.right(seq(
      'unless',
      field('condition', $._no_block_expression),
      field('consequence', $.block),
      repeat($.elsif_clause),
      optional($.else_clause)
    )),

    elsif_clause: $ => seq(
      choice('elsif', seq('else', 'if')),
      field('condition', $._no_block_expression),
      field('consequence', $.block)
    ),

    else_clause: $ => seq(
      'else',
      field('body', $.block)
    ),

    // --- Spawn Expression ---
    spawn_expression: $ => prec(PREC.spawn, seq(
      'spawn',
      choice(
        $.block,
        $._expression
      )
    )),

    // --- Expressions ---
    _expression: $ => choice(
      $._primary_expression,
      $.unary_expression,
      $.binary_expression,
      $.ternary_expression,
      $.assignment_expression,
      $.range_expression,
      $.block_call,
      $.if_expression,
      $.unless_expression,
      $.case_expression,
      $.select_expression
    ),

    _no_block_expression: $ => choice(
      $._no_block_primary_expression,
      alias($._no_block_unary_expression, $.unary_expression),
      $._no_block_binary_expression,
      $.ternary_expression,
      $.cond_assignment_expression,
      alias($._no_block_range_expression, $.range_expression)
    ),

    cond_assignment_expression: $ => prec.right(PREC.assign, seq(
      field('left', choice($.identifier, $.index_expression, $.member_expression)),
      field('operator', choice(
        '=', '+=', '-=', '*=', '/=', '%=', '**=',
        '&=', '|=', '^=', '&^=', '<<=', '>>=',
        '+%=', '-%=', '*%='
      )),
      field('right', choice(
        $._no_block_primary_expression,
        alias($._no_block_unary_expression, $.unary_expression),
        $.binary_expression,
        $.ternary_expression
      ))
    )),

    assignment_expression: $ => prec.right(PREC.assign, seq(
      field('left', choice($.identifier, $.index_expression, $.member_expression)),
      field('operator', choice(
        '=', '+=', '-=', '*=', '/=', '%=', '**=',
        '&=', '|=', '^=', '&^=', '<<=', '>>=',
        '+%=', '-%=', '*%='
      )),
      field('right', $._expression)
    )),

    _primary_expression: $ => choice(
      $.identifier,
      $.constant,
      $.integer,
      $.float,
      $.string,
      $.raw_string,
      $.heredoc,
      $.command_literal,
      $.symbol,
      $.block_symbol,
      $.boolean,
      $.nil,
      $.none,
      $.parenthesized_expression,
      $.tuple_literal,
      $.array_literal,
      $.array_repeat,
      $.map_literal,
      $.lambda_expression,
      $.spawn_expression,
      $.type_application,
      $.member_expression,
      $.index_expression,
      $.call_expression
    ),

    _no_block_primary_expression: $ => choice(
      $.identifier,
      $.constant,
      $.integer,
      $.float,
      $.string,
      $.raw_string,
      $.heredoc,
      $.command_literal,
      $.symbol,
      $.block_symbol,
      $.boolean,
      $.nil,
      $.none,
      $.parenthesized_expression,
      $.tuple_literal,
      $.array_literal,
      $.array_repeat,
      $.type_application,
      $.member_expression,
      $.index_expression,
      $._no_block_call_expression
    ),

    _no_block_call_expression: $ => prec(PREC.call, seq(
      field('function', choice(
        $.identifier,
        $.constant,
        $.member_expression,
        $.type_application
      )),
      field('arguments', $.argument_list)
    )),

    type_application: $ => prec(PREC.call, seq(
      field('type', choice($.type_identifier, $.scoped_type_identifier)),
      field('type_arguments', $.type_arguments)
    )),

    parenthesized_expression: $ => seq(
      '(',
      $._expression,
      ')'
    ),

    tuple_literal: $ => seq(
      '(',
      $._expression,
      ',',
      sepBy1(',', $._expression),
      optional(','),
      ')'
    ),

    array_literal: $ => seq(
      '[',
      sepBy(',', $._expression),
      optional(','),
      ']'
    ),

    array_repeat: $ => seq(
      '[',
      field('value', $._expression),
      ';',
      field('count', $._expression),
      ']'
    ),

    map_literal: $ => seq(
      '{',
      sepBy(',', choice(
        seq(field('key', $._expression), '=>', field('value', $._expression)),
        seq(field('key', choice($.identifier, $.string)), ':', field('value', $._expression))
      )),
      optional(','),
      '}'
    ),

    // Member access: expr.prop, expr?.prop, expr.~prop
    member_expression: $ => prec.left(PREC.member, seq(
      field('object', choice($._primary_expression, $.block_call)),
      field('operator', choice('.', '?.', '.~')),
      field('property', choice($.identifier, $.constant))
    )),

    // Subscript: expr[index]
    index_expression: $ => prec.left(PREC.call, seq(
      field('collection', $._primary_expression),
      token.immediate('['),
      field('index', $._expression),
      ']'
    )),

    // Slice: expr[lo..hi], expr[lo...hi], expr[lo..], expr[..hi]
    slice_expression: $ => prec.left(PREC.call, seq(
      field('collection', $._primary_expression),
      '[',
      optional(field('start', $._expression)),
      choice('..', '...'),
      optional(field('end', $._expression)),
      ']'
    )),

    // Call: expr(...) [block]
    call_expression: $ => prec.right(PREC.call, seq(
      field('function', choice(
        $.identifier,
        $.constant,
        $.member_expression,
        $.type_application
      )),
      field('arguments', $.argument_list),
      optional(field('block', $.block))
    )),

    // Block call: expr { block }
    block_call: $ => seq(
      field('function', choice(
        $.identifier,
        $.constant,
        $.member_expression,
        $.type_application
      )),
      field('block', $.block)
    ),

    argument_list: $ => seq(
      token.immediate('('),
      sepBy(',', choice($._expression, $.keyword_argument)),
      optional(','),
      ')'
    ),

    keyword_argument: $ => seq(
      field('name', choice($.identifier, $.constant)),
      ':',
      field('value', $._expression)
    ),

    command_argument_list: $ => sepBy1(',', $._expression),

    range_expression: $ => prec.left(PREC.range, choice(
      seq(field('start', choice($._primary_expression, $.unary_expression, $.binary_expression)), choice('..', '...'), field('end', choice($._primary_expression, $.unary_expression, $.binary_expression))),
      seq(field('start', choice($._primary_expression, $.unary_expression, $.binary_expression)), choice('..', '...')),
      seq(choice('..', '...'), field('end', choice($._primary_expression, $.unary_expression, $.binary_expression))),
      choice('..', '...')
    )),

    _no_block_range_expression: $ => prec.left(PREC.range, choice(
      seq(field('start', choice($._no_block_primary_expression, alias($._no_block_unary_expression, $.unary_expression), $._no_block_binary_expression)), choice('..', '...'), field('end', choice($._no_block_primary_expression, alias($._no_block_unary_expression, $.unary_expression), $._no_block_binary_expression))),
      seq(field('start', choice($._no_block_primary_expression, alias($._no_block_unary_expression, $.unary_expression), $._no_block_binary_expression)), choice('..', '...')),
      seq(choice('..', '...'), field('end', choice($._no_block_primary_expression, alias($._no_block_unary_expression, $.unary_expression), $._no_block_binary_expression))),
      choice('..', '...')
    )),

    // No-block binary expression (used in conditions)
    _no_block_binary_expression: $ => {
      const table = [
        ['**', PREC.power, 'right'],
        ['*', PREC.multiplicative, 'left'],
        ['/', PREC.multiplicative, 'left'],
        ['%', PREC.multiplicative, 'left'],
        ['*%', PREC.multiplicative, 'left'],
        ['+', PREC.additive, 'left'],
        ['-', PREC.additive, 'left'],
        ['+%', PREC.additive, 'left'],
        ['-%', PREC.additive, 'left'],
        ['<<', PREC.shift, 'left'],
        ['>>', PREC.shift, 'left'],
        ['&', PREC.bitand, 'left'],
        ['&^', PREC.bitand, 'left'],
        ['|', PREC.bitor, 'left'],
        ['^', PREC.bitor, 'left'],
        ['<', PREC.comparison, 'left'],
        ['<=', PREC.comparison, 'left'],
        ['>', PREC.comparison, 'left'],
        ['>=', PREC.comparison, 'left'],
        ['<=>', PREC.comparison, 'left'],
        ['==', PREC.equality, 'left'],
        ['!=', PREC.equality, 'left'],
        ['&&', PREC.and, 'left'],
        ['||', PREC.or, 'left'],
      ];

      const operand = choice(
        $._no_block_primary_expression,
        alias($._no_block_unary_expression, $.unary_expression),
        $._no_block_binary_expression,
        $.ternary_expression
      );

      return choice(...table.map(([operator, precedence, assoc]) => {
        const fn = assoc === 'right' ? prec.right : prec.left;
        return fn(precedence, seq(
          field('left', operand),
          // @ts-ignore
          field('operator', operator),
          field('right', operand)
        ));
      }));
    },

    // Binary expressions
    binary_expression: $ => {
      const table = [
        ['**', PREC.power, 'right'],
        ['*', PREC.multiplicative, 'left'],
        ['/', PREC.multiplicative, 'left'],
        ['%', PREC.multiplicative, 'left'],
        ['*%', PREC.multiplicative, 'left'],
        ['+', PREC.additive, 'left'],
        ['-', PREC.additive, 'left'],
        ['+%', PREC.additive, 'left'],
        ['-%', PREC.additive, 'left'],
        ['<<', PREC.shift, 'left'],
        ['>>', PREC.shift, 'left'],
        ['&', PREC.bitand, 'left'],
        ['&^', PREC.bitand, 'left'],
        ['|', PREC.bitor, 'left'],
        ['^', PREC.bitor, 'left'],
        ['<', PREC.comparison, 'left'],
        ['<=', PREC.comparison, 'left'],
        ['>', PREC.comparison, 'left'],
        ['>=', PREC.comparison, 'left'],
        ['<=>', PREC.comparison, 'left'],
        ['==', PREC.equality, 'left'],
        ['!=', PREC.equality, 'left'],
        ['&&', PREC.and, 'left'],
        ['||', PREC.or, 'left'],
      ];

      const operand = choice(
        $._primary_expression,
        $.unary_expression,
        $.binary_expression,
        $.ternary_expression,
        $.assignment_expression
      );

      return choice(...table.map(([operator, precedence, assoc]) => {
        const fn = assoc === 'right' ? prec.right : prec.left;
        return fn(precedence, seq(
          field('left', operand),
          // @ts-ignore
          field('operator', operator),
          field('right', choice(operand, $.block_call, $.case_expression, $.if_expression, $.unless_expression))
        ));
      }));
    },

    // Unary expressions
    unary_expression: $ => prec(PREC.unary, seq(
      field('operator', choice('-', '!', '^', '~')),
      field('operand', choice($._primary_expression, $.unary_expression))
    )),

    _no_block_unary_expression: $ => prec(PREC.unary, seq(
      field('operator', choice('-', '!', '^', '~')),
      field('operand', choice($._no_block_primary_expression, $._no_block_unary_expression))
    )),

    // Ternary expression: cond ? then : else
    ternary_expression: $ => prec.right(PREC.ternary, seq(
      field('condition', $._expression),
      '?',
      field('consequence', $._expression),
      ':',
      field('alternative', $._expression)
    )),

    // --- Literals ---
    integer: $ => token(choice(
      /0[xX][0-9a-fA-F_]+/,
      /0[oO][0-7_]+/,
      /0[bB][01_]+/,
      /[0-9][0-9_]*/
    )),

    float: $ => token(choice(
      /[0-9][0-9_]*\.[0-9][0-9_]*([eE][+-]?[0-9][0-9_]*)?/,
      /[0-9][0-9_]*[eE][+-]?[0-9][0-9_]*/
    )),

    boolean: $ => choice('true', 'false'),

    nil: $ => 'nil',

    none: $ => 'none',

    symbol: $ => token(seq(
      ':',
      choice(
        /[a-z_][a-zA-Z0-9_]*[?!]?/,
        /[A-Z][a-zA-Z0-9_]*/,
        '**', '<=>', '==', '+', '-', '*', '/', '%', '<', '>'
      )
    )),

    block_symbol: $ => token(seq(
      '&:',
      choice(
        /[a-z_][a-zA-Z0-9_]*[?!]?/,
        /[A-Z][a-zA-Z0-9_]*/,
        '**', '<=>', '==', '+', '-', '*', '/', '%', '<', '>'
      )
    )),

    string: $ => seq(
      '"',
      repeat(choice(
        $.string_content,
        $.escape_sequence,
        $.interpolation
      )),
      token.immediate('"')
    ),

    string_content: $ => token.immediate(prec(1, choice(
      /[^"\\#\r\n]+/,
      /#+[^{"\\#\r\n]+/,
      /#+/
    ))),

    escape_sequence: $ => token.immediate(seq(
      '\\',
      choice(
        /[#nrt0abfve\\'"]/,
        /x[0-9a-fA-F]{2}/,
        /u[0-9a-fA-F]{4}/,
        /U[0-9a-fA-F]{8}/,
        /./
      )
    )),

    interpolation: $ => seq(
      token.immediate(prec(2, '#{')),
      $._expression,
      '}'
    ),

    raw_string: $ => seq(
      "'",
      repeat(choice(
        token.immediate(prec(1, /[^'\r\n\\]+/)),
        token.immediate(seq('\\', /./))
      )),
      token.immediate("'")
    ),

    command_literal: $ => seq(
      '`',
      repeat(choice(
        $.command_content,
        $.escape_sequence,
        $.interpolation,
        $.splice
      )),
      token.immediate('`')
    ),

    command_content: $ => token.immediate(prec(1, choice(
      /[^`\\#\r\n]+/,
      /#+[^{`\\#\r\n]+/,
      /#+/
    ))),

    splice: $ => seq(token.immediate(prec(3, '#{*')), $._expression, '}'),

    heredoc: $ => token(seq('<<~', /[a-zA-Z_][a-zA-Z0-9_]*/)),

    heredoc_body: $ => seq(
      repeat($.heredoc_content),
      $.heredoc_end
    ),

    heredoc_content: $ => token(prec(-1, /.+/)),

    heredoc_end: $ => 'EOS',

    constant: $ => token(/[A-Z][a-zA-Z0-9_]*/),

    type_identifier: $ => token(/[A-Z][a-zA-Z0-9_]*/),

    identifier: $ => token(/[a-z_][a-zA-Z0-9_]*[?!]?/),

    comment: $ => token(prec(-1, seq('#', choice(
      /[^!\[\r\n].*/,
      seq('!', /[^\[\r\n].*/),
      ''
    )))),
  }
});

function sepBy(sep, rule) {
  return optional(sepBy1(sep, rule));
}

function sepBy1(sep, rule) {
  return seq(rule, repeat(seq(sep, rule)));
}
