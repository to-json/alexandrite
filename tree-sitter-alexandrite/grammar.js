/**
 * @file Alexandrite (alx) grammar for tree-sitter
 * @author Alexandrite Authors
 * @license Apache-2.0 WITH LLVM-exception
 *
 * The compiler's hand-written parser (compiler/src/lexer.rs, parser.rs) is the
 * definition of the language; this grammar follows it. Context-sensitive parts
 * live in src/scanner.c:
 *   - statement-ending newlines (a newline ends a statement only where one may
 *     end; a line starting with `.`/`?.`, `else`/`elsif` continues it, also
 *     across comment lines),
 *   - `{` after a callee in a condition (`if xs.any? { |x| x } {`): a block
 *     only when `|` follows, as the compiler's in_cond rule,
 *   - names ending in `?`/`!` (`empty?`, but `x?.f`, `x!= y`),
 *   - `puts -x` (a command argument) vs `a - x`,
 *   - heredocs (`<<~ID`, `<<~"ID"`, `<<~'ID'`, several per line).
 */

/// <reference types="tree-sitter-cli/dsl" />
// @ts-check

const PREC = {
  command: -1,
  assign: 1,
  ternary: 2,
  range: 3,
  or: 4,
  and: 5,
  not: 6,
  is: 7,
  equality: 8,
  comparison: 9,
  bitor: 10,
  pattern: 11,
  bitand: 12,
  shift: 13,
  additive: 14,
  multiplicative: 15,
  unary: 16,
  power: 17,
  postfix: 18,
  call: 19,
};

const ASSIGN_OPS = [
  '=', '+=', '-=', '*=', '/=', '%=', '**=',
  '&=', '|=', '^=', '&^=', '<<=', '>>=',
  '+%=', '-%=', '*%=',
];

// Words that start a declaration or statement only in their position (T9);
// elsewhere they are ordinary names.
const SOFT_KEYWORDS = [
  'import', 'pub', 'error', 'refine', 'extern', 'using', 'select',
  'test', 'bench', 'example',
];

module.exports = grammar({
  name: 'alexandrite',

  externals: $ => [
    $._newline,
    $._suffixed_identifier,
    $._block_lbrace,
    $._body_lbrace,
    $._unary_minus,
    $._binary_minus,
    $._unary_caret,
    $._binary_caret,
    $.heredoc,
    $._heredoc_body_start,
    $.heredoc_content,
    $._heredoc_escape,
    $.heredoc_end,
    $._error_sentinel,
  ],

  extras: $ => [
    /\s/,
    $.comment,
    $.heredoc_body,
  ],

  word: $ => $.identifier,

  conflicts: $ => [
    [$._ident, $._variant_path, $._package_type_path, $._scoped_constant],
    [$.return_statement],
    [$.break_statement],
    [$._return_jump],
    [$._primary_expression, $._type_identifier],
    [$._ident, $._scoped_constant],
    [$._primary_expression, $._variant_path],
    [$.parenthesized_type, $.function_type],
    [$.function_type, $.tuple_type],
  ],

  supertypes: $ => [],

  rules: {
    source_file: $ => seq(
      repeat(choice(
        $._declaration,
        $._terminator,
        seq($._statement, $._terminator),
      )),
      optional($._statement),
    ),

    _terminator: $ => choice($._newline, ';'),

    _declaration: $ => choice(
      $.directive,
      $.import_statement,
      $.const_definition,
      $.struct_definition,
      $.enum_definition,
      $.error_definition,
      $.interface_definition,
      $.refinement_definition,
      $.function_definition,
      $.extern_definition,
      $.test_declaration,
    ),

    // --- Names ---

    // `empty?`, `move!`: the scanner decides whether a `?`/`!` belongs to the name.
    _ident: $ => choice($.identifier, alias($._suffixed_identifier, $.identifier)),

    // A name in an expression: also the contextual words (`select(a, b)`, a local `pub`).
    _name: $ => choice(
      $._ident,
      prec(-1, alias(choice(...SOFT_KEYWORDS), $.identifier)),
    ),

    _type_identifier: $ => alias($.constant, $.type_identifier),

    // --- Directives and Attributes ---

    directive: $ => seq(
      '#![',
      field('name', $.identifier),
      optional(seq('(', field('argument', $.identifier), ')')),
      ']',
    ),

    attribute: $ => seq(
      '#[',
      field('name', $.identifier),
      optional(seq('(', optional($.attribute_arguments), ')')),
      ']',
    ),

    attribute_arguments: $ => seq(
      sepBy1(',', choice($._expression, $.keyword_argument)),
      optional(','),
    ),

    _attributes: $ => repeat1($.attribute),

    // --- Imports and constants ---

    import_statement: $ => seq(
      'import',
      optional(field('alias', $.identifier)),
      field('path', $.string),
    ),

    // `NAME = v`, `pub NAME: T = v`, `#[embed("f")] NAME: Str` (no value).
    const_definition: $ => prec.right(seq(
      optional($._attributes),
      optional('pub'),
      field('name', $.constant),
      choice(
        seq(':', field('type', $._type), optional(seq('=', field('value', $._expression)))),
        seq('=', field('value', $._expression)),
      ),
    )),

    // --- Types: structs, enums, errors, interfaces, refinements ---

    struct_definition: $ => seq(
      optional($._attributes),
      optional('pub'),
      'struct',
      field('name', $._type_identifier),
      optional(field('type_parameters', $.type_parameters)),
      field('body', $.struct_body),
    ),

    struct_body: $ => seq(
      '{',
      repeat(choice(
        $.field_declaration,
        alias($.function_definition, $.method_definition),
        $._terminator,
        ',',
      )),
      '}',
    ),

    field_declaration: $ => seq(
      optional($._attributes),
      field('name', $._ident),
      ':',
      field('type', $._type),
    ),

    enum_definition: $ => seq(
      optional($._attributes),
      optional('pub'),
      'enum',
      field('name', $._type_identifier),
      optional(field('type_parameters', $.type_parameters)),
      field('body', $.enum_body),
    ),

    error_definition: $ => seq(
      optional($._attributes),
      optional('pub'),
      'error',
      field('name', $._type_identifier),
      optional(field('type_parameters', $.type_parameters)),
      field('body', $.enum_body),
    ),

    enum_body: $ => seq(
      '{',
      repeat(choice(
        $.variant_declaration,
        alias($.function_definition, $.method_definition),
        $._terminator,
        ',',
      )),
      '}',
    ),

    variant_declaration: $ => seq(
      optional($._attributes),
      field('name', $._type_identifier),
      optional(field('parameters', $.variant_parameters)),
    ),

    variant_parameters: $ => seq(
      token.immediate('('),
      sepBy(',', $.variant_parameter),
      optional(','),
      ')',
    ),

    variant_parameter: $ => seq(
      optional($._attributes),
      optional(seq(field('name', $._ident), ':')),
      field('type', $._type),
    ),

    interface_definition: $ => seq(
      optional($._attributes),
      optional('pub'),
      'interface',
      field('name', $._type_identifier),
      field('body', $.interface_body),
    ),

    interface_body: $ => seq(
      '{',
      repeat(choice($.interface_method_definition, $._terminator, ',')),
      '}',
    ),

    interface_method_definition: $ => seq(
      optional($._attributes),
      'def',
      field('name', $._method_name),
      optional(field('type_parameters', $.type_parameters)),
      optional(field('parameters', $.parameters)),
      optional(field('return_type', $.return_type)),
      optional(field('body', $._def_body)),
    ),

    // `refine Name[T] for Type { def ... }`
    refinement_definition: $ => seq(
      optional('pub'),
      'refine',
      field('name', $._type_identifier),
      optional(field('type_parameters', $.type_parameters)),
      'for',
      field('target', $._type),
      field('body', $.refinement_body),
    ),

    refinement_body: $ => seq(
      '{',
      repeat(choice(alias($.function_definition, $.method_definition), $._terminator)),
      '}',
    ),

    // --- Functions ---

    function_definition: $ => seq(
      optional($._attributes),
      optional(seq('pub', optional($._attributes))),
      field('keyword', choice('def', 'fn', 'ƒ')),
      field('name', $._method_name),
      optional(field('type_parameters', $.type_parameters)),
      optional(field('parameters', $.parameters)),
      optional(field('return_type', $.return_type)),
      field('body', $._def_body),
    ),

    // `#[link("c")] pub extern def f(x: I32) -> I32 = "symbol"`
    extern_definition: $ => prec.right(seq(
      optional($._attributes),
      optional('pub'),
      'extern',
      field('keyword', choice('def', 'fn')),
      field('name', $._ident),
      optional(field('parameters', $.parameters)),
      optional(field('return_type', $.return_type)),
      optional(seq('=', field('link_name', $.string))),
    )),

    _def_body: $ => alias($._def_block, $.block),

    _def_block: $ => seq('{', ...statements($), '}'),

    // A method may be named by a keyword (`def next`), an operator (`def +(o)`),
    // or be static (`def self.origin`).
    _method_name: $ => choice(
      $._ident,
      $.operator_name,
      seq('self', '.', choice($._ident, $.operator_name)),
    ),

    operator_name: $ => choice(
      '+', '-', '*', '/', '%', '==', '<=>', '<<', '>>', '**',
      seq('[', ']'),
    ),

    // --- Tests ---

    test_declaration: $ => seq(
      field('kind', choice('test', 'bench', 'example')),
      field('name', $.string),
      field('body', alias($.test_body, $.block)),
      optional(seq(
        'outputs',
        field('expected_output', choice($.string, $.heredoc)),
      )),
    ),

    // `test "x" { |t| ... }`
    test_body: $ => seq(
      '{',
      optional(field('parameters', $.block_parameters)),
      ...statements($),
      '}',
    ),

    // --- Type parameters and parameters ---

    type_parameters: $ => seq(
      token.immediate('['),
      sepBy1(',', $.type_parameter),
      optional(','),
      ']',
    ),

    type_parameter: $ => seq(
      field('name', $._type_identifier),
      optional(seq(':', field('bound', choice($.like_type, $._type)))),
    ),

    // `like Int`: a type parameter's bound, or a type test (`T is like Int`).
    like_type: $ => seq('like', $._type_identifier),

    parameters: $ => seq(
      token.immediate('('),
      sepBy(',', $.parameter),
      optional(','),
      ')',
    ),

    // `name: T = default` (S8)
    parameter: $ => seq(
      field('name', $._ident),
      optional(seq(':', field('type', $._type))),
      optional(seq('=', field('default', $._expression))),
    ),

    return_type: $ => choice(
      seq('->', field('type', $._type)),
      '~',
    ),

    // --- Types ---

    _type: $ => choice(
      $._type_identifier,
      $.scoped_type_identifier,
      $.generic_type,
      $.array_type,
      $.fixed_array_type,
      $.handle_type,
      $.optional_type,
      $.result_type,
      $.function_type,
      $.tuple_type,
      $.parenthesized_type,
    ),

    scoped_type_identifier: $ => seq(
      field('scope', $.identifier),
      '.',
      field('name', $._type_identifier),
    ),

    generic_type: $ => seq(
      field('name', choice($._type_identifier, $.scoped_type_identifier)),
      field('type_arguments', $.type_arguments),
    ),

    type_arguments: $ => seq(
      token.immediate('['),
      sepBy1(',', $._type),
      optional(','),
      ']',
    ),

    array_type: $ => seq('[', field('element', $._type), ']'),

    fixed_array_type: $ => seq(
      '[',
      field('element', $._type),
      ';',
      field('length', $._expression),
      ']',
    ),

    handle_type: $ => seq(
      '@',
      field('name', choice($._type_identifier, $.scoped_type_identifier)),
      optional(field('type_arguments', $.type_arguments)),
    ),

    optional_type: $ => prec(2, seq($._type, token.immediate('?'))),

    // `~T`, `~T?`, `~T<E | pkg.F>`
    result_type: $ => prec.right(1, seq(
      '~',
      field('type', $._type),
      optional(field('errors', $.error_set)),
    )),

    error_set: $ => seq(
      token.immediate('<'),
      sepBy1('|', choice($._type_identifier, $.scoped_type_identifier)),
      '>',
    ),

    function_type: $ => prec.right(seq(
      '(',
      sepBy(',', $._type),
      ')',
      '->',
      field('return', $._type),
    )),

    tuple_type: $ => seq(
      '(',
      $._type,
      repeat1(seq(',', $._type)),
      optional(','),
      ')',
    ),

    parenthesized_type: $ => seq('(', $._type, ')'),

    // --- Statements ---

    _statement: $ => choice(
      $._simple_statement,
      $.if_modifier,
      $.unless_modifier,
      $.while_statement,
      $.for_statement,
    ),

    _simple_statement: $ => choice(
      $.expression_statement,
      $.declaration_statement,
      $.multi_assignment_statement,
      $.return_statement,
      $.break_statement,
      $.next_statement,
      $.fail_statement,
      $.defer_statement,
      $.using_statement,
    ),

    // `stmt if cond`: the condition may hold blocks (`return i if s.any? { it == r }`).
    if_modifier: $ => prec.dynamic(1, seq(
      field('body', $._simple_statement),
      'if',
      field('condition', $._expression),
    )),

    unless_modifier: $ => prec.dynamic(1, seq(
      field('body', $._simple_statement),
      'unless',
      field('condition', $._expression),
    )),

    expression_statement: $ => $._expression,

    // `x: T = e`
    declaration_statement: $ => seq(
      field('name', $._ident),
      ':',
      field('type', $._type),
      '=',
      field('value', $._expression),
    ),

    // `a, b = f()`, `xs[i], xs[j] = xs[j], xs[i]`
    multi_assignment_statement: $ => seq(
      field('left', $._place),
      repeat1(seq(',', field('left', $._place))),
      '=',
      field('right', $._expression),
      repeat(seq(',', field('right', $._expression))),
    ),

    _place: $ => choice($._name, $.index_expression, $.member_expression),

    // `return`, `return v`, `return a, b`
    return_statement: $ => (seq(
      'return',
      optional(seq($._expression, repeat(seq(',', $._expression)))),
    )),

    break_statement: $ => seq('break', optional($._expression)),

    next_statement: $ => 'next',

    // `fail e`, `fail(e)`: a jump, so a hard keyword (T9)
    fail_statement: $ => prec.right(seq('fail', $._expression)),

    // A jump where an expression ends (S8): `opt || return v`, `X => break`.
    _jump: $ => choice(
      alias($._return_jump, $.return_statement),
      $.break_statement,
      $.next_statement,
      $.fail_statement,
    ),

    _return_jump: $ => seq('return', optional($._expression)),

    // `defer f(x)`, `defer { stmts }`
    defer_statement: $ => seq(
      'defer',
      choice(field('body', $._cond_body), field('expression', $._expression)),
    ),

    using_statement: $ => seq(
      'using',
      field('name', choice(
        $._type_identifier,
        $.scoped_type_identifier,
      )),
    ),

    while_statement: $ => seq(
      'while',
      field('condition', $._expression),
      field('body', $._cond_body),
    ),

    for_statement: $ => seq(
      'for',
      field('variable', $._ident),
      repeat(seq(',', field('variable', $._ident))),
      'in',
      field('collection', $._expression),
      field('body', $._cond_body),
    ),

    // --- Blocks and lambdas ---

    // A block given to a call: `xs.map { |x| x * 2 }`, `mu.lock { ... }`.
    block: $ => seq(
      alias($._block_lbrace, '{'),
      optional(field('parameters', $.block_parameters)),
      ...statements($),
      '}',
    ),

    // The body of if / while / for / case / defer / spawn.
    _cond_body: $ => alias($._cond_block, $.block),
    _cond_block: $ => seq(alias($._body_lbrace, '{'), ...statements($), '}'),

    block_parameters: $ => seq(
      '|',
      sepBy(',', field('name', $._ident)),
      optional(','),
      '|',
    ),

    // `->(x: Int, y) -> Int { ... }`, `-> { ... }`
    lambda_expression: $ => seq(
      '->',
      optional(field('parameters', alias($.lambda_parameters, $.parameters))),
      optional(seq('->', field('return_type', $._type))),
      field('body', $._def_body),
    ),

    lambda_parameters: $ => seq(
      '(',
      sepBy(',', $.parameter),
      optional(','),
      ')',
    ),

    // --- case / select ---

    case_expression: $ => choice(
      seq(
        'case',
        field('subject', $._expression),
        alias($._body_lbrace, '{'),
        ...separated($, $.case_arm),
        '}',
      ),
      // `case { cond => ... }`: each arm is a condition.
      seq(
        'case',
        alias($._body_lbrace, '{'),
        ...separated($, alias($.case_condition_arm, $.case_arm)),
        '}',
      ),
    ),

    case_arm: $ => seq(
      field('pattern', $._pattern),
      repeat(seq('|', field('pattern', $._pattern))),
      '=>',
      field('body', $._arm_body),
    ),

    case_condition_arm: $ => seq(
      field('pattern', $._expression),
      '=>',
      field('body', $._arm_body),
    ),

    // `X => { stmts }`, `X => return v` / `fail e` / `break` / `next` (S8), `X => expr`
    _arm_body: $ => choice($._cond_body, $._jump, $._expression),

    _pattern: $ => choice(
      $.variant_pattern,
      prec(PREC.pattern, $._expression),
    ),

    // `Some`, `Bad(pos, _)`, `PErr.Bad(p)`, `pkg.PErr.Bad(p)`, `pkg.Type(v)`
    variant_pattern: $ => prec.dynamic(1, choice(
      seq(field('name', $._variant_path), optional(field('bindings', $.pattern_bindings))),
      // `pkg.Type(v)`: an implementor from another package (a type switch);
      // without bindings `pkg.NAME` is a value.
      seq(field('name', $._package_type_path), field('bindings', $.pattern_bindings)),
    )),

    _variant_path: $ => choice(
      $.constant,
      seq($.constant, '.', $.constant),
      seq($.identifier, '.', $.constant, '.', $.constant),
    ),

    _package_type_path: $ => seq($.identifier, '.', $.constant),

    pattern_bindings: $ => seq(
      token.immediate('('),
      sepBy(',', $._ident),
      optional(','),
      ')',
    ),

    select_expression: $ => seq(
      'select',
      alias($._body_lbrace, '{'),
      ...separated($, $.select_arm),
      '}',
    ),

    // `when v = ch.recv => ...`, `when ch.send(x) => ...`, `else => ...`
    select_arm: $ => seq(
      choice(
        seq('when', field('operation', $._expression)),
        'else',
      ),
      '=>',
      field('body', choice($._cond_body, $._expression)),
    ),

    // --- if / unless ---

    if_expression: $ => prec.right(seq(
      'if',
      field('condition', $._expression),
      field('consequence', $._cond_body),
      repeat(field('alternative', $.elsif_clause)),
      optional(field('alternative', $.else_clause)),
    )),

    unless_expression: $ => prec.right(seq(
      'unless',
      field('condition', $._expression),
      field('consequence', $._cond_body),
      repeat(field('alternative', $.elsif_clause)),
      optional(field('alternative', $.else_clause)),
    )),

    elsif_clause: $ => seq(
      choice('elsif', seq('else', 'if'), seq('else', 'unless')),
      field('condition', $._expression),
      field('consequence', $._cond_body),
    ),

    else_clause: $ => seq('else', field('body', $._cond_body)),

    // `spawn { ... }`, `spawn f(x)`
    spawn_expression: $ => prec.right(seq(
      'spawn',
      choice(field('body', $._cond_body), field('expression', $._expression)),
    )),

    // --- Expressions ---

    _expression: $ => choice(
      $._primary_expression,
      $.unary_expression,
      $.binary_expression,
      $.ternary_expression,
      $.range_expression,
      $.assignment_expression,
      $.type_test,
      $.command_call,
      $.spawn_expression,
    ),

    _primary_expression: $ => choice(
      $._name,
      $.constant,
      $.integer,
      $.float,
      $.imaginary,
      $.string,
      $.raw_string,
      $.heredoc,
      $.command_literal,
      $.symbol,
      $.boolean,
      $.nil,
      $.none,
      $.parenthesized_expression,
      $.tuple_expression,
      $.array_literal,
      $.array_repeat,
      $.map_literal,
      $.lambda_expression,
      $.member_expression,
      $.index_expression,
      $.call_expression,
      $.if_expression,
      $.unless_expression,
      $.case_expression,
      $.select_expression,
    ),

    assignment_expression: $ => prec.right(PREC.assign, seq(
      field('left', $._place),
      field('operator', choice(...ASSIGN_OPS)),
      field('right', $._expression),
    )),

    ternary_expression: $ => prec.right(PREC.ternary, seq(
      field('condition', $._expression),
      '?',
      field('consequence', $._expression),
      ':',
      field('alternative', $._expression),
    )),

    range_expression: $ => prec.left(PREC.range, seq(
      field('start', $._expression),
      field('operator', choice('..', '...')),
      field('end', $._expression),
    )),

    binary_expression: $ => {
      const minus = alias($._binary_minus, '-');
      const caret = alias($._binary_caret, '^');
      const table = [
        ['&&', PREC.and],
        ['==', PREC.equality],
        ['!=', PREC.equality],
        ['<', PREC.comparison],
        ['<=', PREC.comparison],
        ['>', PREC.comparison],
        ['>=', PREC.comparison],
        ['<=>', PREC.comparison],
        ['|', PREC.bitor],
        [caret, PREC.bitor],
        ['&', PREC.bitand],
        ['&^', PREC.bitand],
        ['<<', PREC.shift],
        ['>>', PREC.shift],
        ['+', PREC.additive],
        [minus, PREC.additive],
        ['+%', PREC.additive],
        ['-%', PREC.additive],
        ['*', PREC.multiplicative],
        ['/', PREC.multiplicative],
        ['%', PREC.multiplicative],
        ['*%', PREC.multiplicative],
      ];
      return choice(
        // `opt || return v` / `|| fail e` / `|| break` / `|| next` (S8)
        prec.left(PREC.or, seq(
          field('left', $._expression),
          field('operator', '||'),
          field('right', choice($._expression, $._jump)),
        )),
        ...table.map(([operator, precedence]) => prec.left(precedence, seq(
          field('left', $._expression),
          // @ts-ignore
          field('operator', operator),
          field('right', $._expression),
        ))),
        prec.right(PREC.power, seq(
          field('left', $._expression),
          field('operator', '**'),
          field('right', $._expression),
        )),
      );
    },

    // `!` binds looser than `==` (`!a == b` is `!(a == b)`), as in the compiler.
    unary_expression: $ => choice(
      prec(PREC.unary, seq(
        field('operator', choice(alias($._unary_minus, '-'), alias($._unary_caret, '^'), '~')),
        field('operand', $._expression),
      )),
      prec(PREC.not, seq(
        field('operator', '!'),
        field('operand', $._expression),
      )),
    ),

    // `R is io.Seeker`, `T is like Int`, `T is Str` (S6)
    type_test: $ => prec(PREC.is, seq(
      field('type', choice($._type_identifier, $.type_application)),
      'is',
      field('test', choice($.like_type, $._type)),
    )),

    // `puts x, y`: a call without parentheses (a bare name, then a space).
    command_call: $ => prec.right(PREC.command, seq(
      field('function', $._name),
      field('arguments', $.command_argument_list),
    )),

    command_argument_list: $ => prec.right(seq(
      $._expression,
      repeat(seq(',', $._expression)),
    )),

    parenthesized_expression: $ => seq('(', $._expression, ')'),

    tuple_expression: $ => seq(
      '(',
      $._expression,
      ',',
      sepBy(',', $._expression),
      optional(','),
      ')',
    ),

    array_literal: $ => seq(
      '[',
      sepBy(',', $._expression),
      optional(','),
      ']',
    ),

    // `[0; 16]`
    array_repeat: $ => seq(
      '[',
      field('value', $._expression),
      ';',
      field('count', $._expression),
      ']',
    ),

    map_literal: $ => seq(
      '{',
      sepBy(',', $.pair),
      optional(','),
      '}',
    ),

    pair: $ => choice(
      seq(field('key', $._expression), '=>', field('value', $._expression)),
      seq(field('key', $._name), ':', field('value', $._expression)),
    ),

    // `obj.name`, `opt?.name`, `obj.~name` (propagates the call's error)
    member_expression: $ => prec(PREC.postfix, seq(
      field('object', choice($._primary_expression, $.type_application)),
      field('operator', choice('.', '?.', '.~')),
      field('property', choice($._ident, $.constant)),
    )),

    // `Stack[Int]` before `.new`, `geom.Stack[Int]`, `T[U] is X`
    type_application: $ => prec.dynamic(1, seq(
      field('type', choice(
        $._type_identifier,
        alias($._scoped_constant, $.scoped_type_identifier),
      )),
      field('type_arguments', $.type_arguments),
    )),

    _scoped_constant: $ => seq(
      field('scope', $.identifier),
      '.',
      field('name', $._type_identifier),
    ),

    // `xs[i]`, `s[1...3]`, `a[2..]`, `a[..n]`
    index_expression: $ => prec(PREC.call, seq(
      field('object', $._primary_expression),
      token.immediate('['),
      field('index', choice(
        $._expression,
        alias($.open_range, $.range_expression),
      )),
      ']',
    )),

    open_range: $ => choice(
      seq(field('start', $._expression), field('operator', choice('..', '...'))),
      seq(field('operator', choice('..', '...')), field('end', $._expression)),
    ),

    // `f(x)`, `obj.m(x) { block }`, `xs.each { |x| ... }`
    call_expression: $ => prec(PREC.call, choice(
      seq(
        field('function', choice($._name, $.member_expression)),
        field('arguments', $.argument_list),
        optional(field('block', $.block)),
      ),
      seq(
        field('function', choice($._name, $.member_expression)),
        field('block', $.block),
      ),
    )),

    argument_list: $ => seq(
      token.immediate('('),
      sepBy(',', choice($._expression, $.keyword_argument, $.block_symbol)),
      optional(','),
      ')',
    ),

    // `level: 2` (a keyword names a field too: `next: n`)
    keyword_argument: $ => seq(
      field('name', $._name),
      ':',
      field('value', $._expression),
    ),

    // --- Literals ---

    integer: $ => token(choice(
      /0[xX][0-9a-fA-F_]+/,
      /0[oO][0-7_]+/,
      /0[bB][01_]+/,
      /[0-9][0-9_]*/,
    )),

    float: $ => token(choice(
      /[0-9][0-9_]*\.[0-9][0-9_]*([eE][+-]?[0-9]+)?/,
      /[0-9][0-9_]*[eE][+-]?[0-9]+/,
    )),

    // `2i`, `2.5i`, `1e3i`
    imaginary: $ => token(choice(
      /[0-9][0-9_]*i/,
      /[0-9][0-9_]*\.[0-9][0-9_]*([eE][+-]?[0-9]+)?i/,
      /[0-9][0-9_]*[eE][+-]?[0-9]+i/,
    )),

    boolean: $ => choice('true', 'false'),

    nil: $ => 'nil',

    none: $ => 'none',

    symbol: $ => token(seq(
      ':',
      choice(
        /[a-zA-Z_][a-zA-Z0-9_]*[?!]?/,
        '**', '<=>', '==', '+', '-', '*', '/', '%', '<', '>',
      ),
    )),

    // `xs.map(&:to_s)`
    block_symbol: $ => token(seq(
      '&:',
      choice(
        /[a-zA-Z_][a-zA-Z0-9_]*[?!]?/,
        '**', '<=>', '==', '+', '-', '*', '/', '%', '<', '>',
      ),
    )),

    string: $ => seq(
      '"',
      repeat(choice(
        $.string_content,
        $.escape_sequence,
        $.interpolation,
      )),
      token.immediate('"'),
    ),

    string_content: $ => token.immediate(prec(1, choice(
      /[^"\\#\r\n]+/,
      /#+[^{"\\#\r\n]+/,
      /#+/,
    ))),

    escape_sequence: $ => token.immediate(seq(
      '\\',
      choice(
        /x[0-9a-fA-F]{2}/,
        /u[0-9a-fA-F]{4}/,
        /U[0-9a-fA-F]{8}/,
        /[^xuU\r\n]/,
      ),
    )),

    interpolation: $ => seq(
      token.immediate(prec(2, '#{')),
      $._expression,
      '}',
    ),

    // '...': raw, no escapes, no interpolation
    raw_string: $ => token(seq("'", /[^'\r\n]*/, "'")),

    command_literal: $ => seq(
      '`',
      repeat(choice(
        $.command_content,
        $.escape_sequence,
        $.interpolation,
        $.splice,
      )),
      token.immediate('`'),
    ),

    command_content: $ => token.immediate(prec(1, choice(
      /[^`\\#]+/,
      /#+[^{`\\#]+/,
      /#+/,
    ))),

    // `#{*args}`
    splice: $ => seq(token.immediate(prec(3, '#{*')), $._expression, '}'),

    // The text of a heredoc follows the line that starts it (`<<~EOS`,
    // `<<~"EOS"`: escapes and #{}; `<<~'EOS'`: raw); the scanner reads it.
    heredoc_body: $ => seq(
      $._heredoc_body_start,
      repeat(choice(
        $.heredoc_content,
        alias($._heredoc_escape, $.escape_sequence),
        $.interpolation,
      )),
      $.heredoc_end,
    ),

    constant: $ => /[A-Z][a-zA-Z0-9_]*/,

    identifier: $ => /[a-z_][a-zA-Z0-9_]*/,

    comment: $ => token(prec(-1, choice(
      /#([^!\[\r\n][^\r\n]*)?/,
      /#!([^\[\r\n][^\r\n]*)?/,
    ))),
  },
});

/**
 * Statements up to a closing brace: separated by newlines or `;`; the last
 * needs no separator.
 * @param {GrammarSymbols<string>} $
 */
function separated($, rule) {
  const sep = choice($._terminator, ',');
  return [repeat(choice(sep, seq(rule, sep))), optional(rule)];
}

/**
 * @param {GrammarSymbols<string>} $
 */
function statements($) {
  return [
    repeat(choice($._terminator, seq($._statement, $._terminator))),
    optional($._statement),
  ];
}

/**
 * @param {RuleOrLiteral} sep
 * @param {RuleOrLiteral} rule
 */
function sepBy(sep, rule) {
  return optional(sepBy1(sep, rule));
}

/**
 * @param {RuleOrLiteral} sep
 * @param {RuleOrLiteral} rule
 */
function sepBy1(sep, rule) {
  return seq(rule, repeat(seq(sep, rule)));
}
