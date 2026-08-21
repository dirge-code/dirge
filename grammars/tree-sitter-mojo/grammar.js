/**
 * @file Python grammar for tree-sitter
 * @author Max Brunsfeld <maxbrunsfeld@gmail.com>
 * @license MIT
 * @see {@link https://docs.python.org/2/reference/grammar.html|Python 2 grammar}
 * @see {@link https://docs.python.org/3/reference/grammar.html|Python 3 grammar}
 */

/// <reference types="tree-sitter-cli/dsl" />
// @ts-check

const PREC = {
  // this resolves a conflict between the usage of ':' in a lambda vs in a
  // typed parameter. In the case of a lambda, we don't allow typed parameters.
  lambda: -2,
  typed_parameter: -1,
  conditional: -1,

  parenthesized_expression: 1,
  parenthesized_list_splat: 1,
  or: 10,
  and: 11,
  not: 12,
  compare: 13,
  bitwise_or: 14,
  bitwise_and: 15,
  xor: 16,
  shift: 17,
  plus: 18,
  times: 19,
  unary: 20,
  power: 21,
  call: 22,
};

const SEMICOLON = ";";
const SELF = "self";

const PYTHON_KEYWORDS = [
  // https://docs.python.org/3/reference/lexical_analysis.html#keywords
  'False', 'await', 'else', 'import', 'pass',
  'None', 'break', 'except', 'in', 'raise',
  'True', 'class', 'finally', 'is', 'return',
  'and', 'continue', 'for', 'lambda', 'try',
  'as', 'def', 'from', 'nonlocal', 'while',
  'assert', 'del', 'global', 'not', 'with',
  'async', 'elif', 'if', 'or', 'yield',
];

// Mojo-specific keywords. The argument-convention soft keywords `mut`/`out`
// are reserved globally, but specific rules (type_parameter, keyword_argument,
// subscript) explicitly re-admit them as identifiers where they appear as
// ordinary names, e.g. `mut: Bool` or `Origin[mut=mut]`. `read` is deliberately
// not reserved: it is still keyword-extracted from `argument_convention`, so
// `read x` conventions parse, but it remains usable as an ordinary identifier
// (e.g. a method `def read(self)` or `UInt(read)`) without reserving it.
const MOJO_KEYWORDS = [
  'var', 'comptime', 'ref', 'deinit', 'unified', 'where',
  'mut', 'out',
  // Function-effect keywords. Reserved so they are not mistaken for a typed
  // `raises` error type, e.g. in `fn() raises capturing -> None`.
  'capturing', 'escaping', 'thin',
];

module.exports = grammar({
  name: "mojo",

  extras: ($) => [
    $.comment,
    /[\s\f\uFEFF\u2060\u200B]|\r?\n/,
    $.line_continuation,
  ],

  conflicts: ($) => [
    [$.parameter_list, $.subscript],
    [$.primary_expression, $.pattern],
    [$.primary_expression, $.list_splat_pattern],
    [$.tuple, $.tuple_pattern],
    [$.list, $.list_pattern],
    [$.with_item, $._collection_elements],
    [$.named_expression, $.as_pattern],
    [$.print_statement, $.primary_expression],
    [$.type_alias_statement, $.primary_expression],
    [$.match_statement, $.primary_expression],
    [$.transfer_expression, $.binary_operator],
    [$.transfer_expression, $.binary_operator, $.unary_operator],
    [$.transfer_expression, $.binary_operator, $.await],
    [$.type_parameter, $.list],
    [$.parameterized_alias_statement, $.primary_expression],
    [$._collection_elements, $.struct_literal],
    [$._raises_type, $.type],
    // A backtick (string) binding name may begin an assignment or, bare, be an
    // expression statement, e.g. ``` `6bit` = x ``` vs ``` `6bit` ```.
    [$.primary_expression, $.assignment],
    // `A & B` may be a `binary_operator` (expressions) or an `intersection_type`
    // (e.g. when an operand is a `function_type`).
    [$.primary_expression, $._intersection_operand],
    [$.list_splat_pattern, $.primary_expression, $._intersection_operand],
  ],

  supertypes: ($) => [
    $._simple_statement,
    $._compound_statement,
    $.expression_statement,
    $.expression,
    $.primary_expression,
    $.pattern,
    $.parameter,
  ],

  externals: ($) => [
    $._newline,
    $._indent,
    $._dedent,
    $.string_start,
    $._string_content,
    $.escape_interpolation,
    $.string_end,

    // Mark comments as external tokens so that the external scanner is always
    // invoked, even if no external token is expected. This allows for better
    // error recovery, because the external scanner can maintain the overall
    // structure by returning dedent tokens whenever a dedent occurs, even
    // if no dedent is expected.
    $.comment,

    // Allow the external scanner to check for the validity of closing brackets
    // so that it can avoid returning dedent tokens between brackets.
    "]",
    ")",
    "}",
    "except",

    // MLIR backtick-fragment interior tokens (see scanner.c). The interior of a
    // backtick MLIR fragment is tokenized into pieces so it highlights as MLIR.
    $._mlir_backtick,
    $._mlir_ident,
    $._mlir_number,
    $.mlir_punctuation,
  ],

  inline: ($) => [
    $._simple_statement,
    $._compound_statement,
    $._suite,
    $._expressions,
    $._left_hand_side,
    $.keyword_identifier,
  ],

  reserved: {
    global: _ => [...PYTHON_KEYWORDS, ...MOJO_KEYWORDS],
  },

  word: ($) => $.identifier,

  rules: {
    module: ($) => repeat($._statement),

    _statement: ($) => choice($._simple_statements, $._compound_statement),

    // Simple statements

    _simple_statements: ($) =>
      seq(
        sep1($._simple_statement, SEMICOLON),
        optional(SEMICOLON),
        $._newline,
      ),

    _simple_statement: ($) =>
      choice(
        $.future_import_statement,
        $.import_statement,
        $.import_from_statement,
        $.print_statement,
        $.assert_statement,
        $.comptime_assert_statement,
        $.expression_statement,
        $.return_statement,
        $.delete_statement,
        $.raise_statement,
        $.pass_statement,
        $.break_statement,
        $.continue_statement,
        $.global_statement,
        $.nonlocal_statement,
        $.exec_statement,
        $.type_alias_statement,
        $.parameterized_alias_statement,
      ),

    import_statement: ($) => seq("import", $._import_list),

    import_prefix: (_) => repeat1("."),

    relative_import: ($) => seq($.import_prefix, optional($.dotted_name)),

    future_import_statement: ($) =>
      seq(
        "from",
        "__future__",
        "import",
        choice($._import_list, seq("(", $._import_list, ")")),
      ),

    import_from_statement: ($) =>
      seq(
        "from",
        field("module_name", choice($.relative_import, $.dotted_name)),
        "import",
        choice(
          $.wildcard_import,
          $._import_list,
          seq("(", $._import_list, ")"),
        ),
      ),

    _import_list: ($) =>
      seq(
        commaSep1(field("name", choice(
          $.dotted_name,
          // A relative import using `import`, e.g. `import .warp`.
          $.relative_import,
          $.aliased_import,
        ))),
        optional(","),
      ),

    aliased_import: ($) =>
      seq(field("name", $.dotted_name), "as", field("comptime", $.identifier)),

    wildcard_import: (_) => "*",

    print_statement: ($) =>
      choice(
        prec(
          1,
          seq(
            "print",
            $.chevron,
            repeat(seq(",", field("argument", $.expression))),
            optional(","),
          ),
        ),
        prec(
          -3,
          prec.dynamic(
            -1,
            seq(
              "print",
              commaSep1(field("argument", $.expression)),
              optional(","),
            ),
          ),
        ),
      ),

    chevron: ($) => seq(">>", $.expression),

    assert_statement: ($) =>
      seq(optional("comptime"), "assert", commaSep1($.expression)),

    comptime_assert_statement: ($) => seq("__comptime_assert", $.expression),

    expression_statement: ($) =>
      choice(
        $.expression,
        $.tuple_expression,
        $.assignment,
        $.augmented_assignment,
        $.yield,
      ),

    tuple_expression: ($) =>
      seq($.expression, ',', optional(seq(commaSep1($.expression), optional(',')))),

    named_expression: ($) =>
      seq(
        field("name", $._named_expression_lhs),
        ":=",
        field("value", $.expression),
      ),

    _named_expression_lhs: ($) => choice($.identifier, $.keyword_identifier),

    return_statement: ($) => seq("return", optional($._expressions)),

    delete_statement: ($) => seq("del", $._expressions),

    _expressions: ($) => choice($.expression, $.expression_list),

    raise_statement: ($) =>
      seq(
        "raise",
        optional($._expressions),
        optional(seq("from", field("cause", $.expression))),
      ),

    pass_statement: (_) => prec.left("pass"),
    break_statement: (_) => prec.left("break"),
    continue_statement: (_) => prec.left("continue"),

    // Compound statements

    _compound_statement: ($) => choice(
      $.if_statement,
      $.for_statement,
      $.while_statement,
      $.try_statement,
      $.with_statement,
      $.function_definition,
      $.class_definition,
      $.trait_definition,
      $.decorated_definition,
      $.match_statement,
      $.comptime_statement,
      $.mlir_region,
      $.extension_definition,
    ),

    // An extension declaration, e.g. `__extension List:` or
    // `__extension List[T]:`.
    extension_definition: ($) =>
      seq(
        "__extension",
        field("name", choice($.identifier, $.generic_type)),
        ":",
        field("body", $._suite),
      ),

    // An MLIR region declaration, e.g.
    //   __mlir_region await_body(hdl: __mlir_type.`!co.routine`):
    //       body(hdl)
    mlir_region: ($) =>
      seq(
        "__mlir_region",
        field("name", $.identifier),
        field("parameters", $.parameters),
        ":",
        field("body", $._suite),
      ),

    // A compile-time control-flow statement, e.g. `comptime if ...:` or
    // `comptime for ... in ...:`.
    comptime_statement: ($) => seq(
      'comptime',
      choice($.if_statement, $.for_statement, $.while_statement),
    ),

    if_statement: ($) => seq(
      'if',
      field('condition', $.expression),
      ':',
      field('consequence', $._suite),
      repeat(field('alternative', $.elif_clause)),
      optional(field('alternative', $.else_clause)),
    ),

    elif_clause: ($) => seq(
      'elif',
      field('condition', $.expression),
      ':',
      field('consequence', $._suite),
    ),

    else_clause: ($) => seq(
      'else',
      ':',
      field('body', $._suite),
    ),

    match_statement: ($) => seq(
      'match',
      commaSep1(field('subject', $.expression)),
      optional(','),
      ':',
      field('body', alias($._match_block, $.block)),
    ),

    _match_block: ($) => choice(
      seq(
        $._indent,
        repeat(field('alternative', $.case_clause)),
        $._dedent,
      ),
      $._newline,
    ),

    case_clause: ($) => seq(
      'case',
      commaSep1($.case_pattern),
      optional(','),
      optional(field('guard', $.if_clause)),
      ':',
      field('consequence', $._suite),
    ),

    for_statement: ($) => seq(
      optional('async'),
      'for',
      field('left', $._left_hand_side),
      'in',
      field('right', $._expressions),
      ':',
      field('body', $._suite),
      field('alternative', optional($.else_clause)),
    ),

    while_statement: ($) => seq(
      'while',
      field('condition', $.expression),
      ':',
      field('body', $._suite),
      optional(field('alternative', $.else_clause)),
    ),

    try_statement: ($) => seq(
      'try',
      ':',
      field('body', $._suite),
      repeat($.except_clause),
      optional($.else_clause),
      optional($.finally_clause),
    ),

    except_clause: ($) => seq(
      'except',
      optional(token(prec(1, '*'))),
      optional(choice(
        seq(
          field('value', $.expression),
          optional(seq('as', field('alias', $.expression))),
        ),
        commaSep1(field('value', $.expression)),
      )),
      ':',
      $._suite,
    ),

    finally_clause: ($) => seq(
      'finally',
      ':',
      $._suite,
    ),

    with_statement: ($) => seq(
      optional('async'),
      'with',
      $.with_clause,
      ':',
      field('body', $._suite),
    ),

    with_clause: ($) => choice(
      seq(commaSep1($.with_item), optional(',')),
      seq('(', commaSep1($.with_item), optional(','), ')'),
    ),

    with_item: ($) => prec.dynamic(1, seq(
      field('value', $.expression),
    )),

    function_definition: ($) => seq(
      optional('async'),
      choice('def', 'fn'),
      optional(seq('[',
        field('name', $.identifier),
        field('type_parameters', optional($.type_parameter)),
        field('parameters', $.parameters),
        ']')),
      field('name', $.identifier),
      field('type_parameters', optional($.type_parameter)),
      field('parameters', $.parameters),
      optional($.unified_clause),
      optional($._function_effects),
      optional($.result_convention),
      optional(
        seq(
          '->',
          optional($._ref_convention),
          field('return_type', $.type),
        ),
      ),
      repeat($.where_clause),
      ':',
      field('body', $._suite),
    ),

    // A brace-delimited capture/result convention preceding the return type,
    // e.g. `def f(...) {read} -> T:`. Each convention may bind a name, as in
    // `def f() {read x, mut y}:`.
    result_convention: ($) =>
      seq(
        "{",
        commaSep1(seq($.argument_convention, optional($.identifier))),
        optional(","),
        "}",
      ),

    // A function's effect qualifiers, e.g. `raises`, `capturing`, `thin`, or
    // combinations like `raises capturing`. `raises` may carry an optional
    // error type, bound greedily so a following `->`/`:`/`|`/`.` is treated as
    // part of the type when present.
    _function_effects: ($) => repeat1(choice(
      // A typed `raises` carries an optional error type. The error type is an
      // expression-level type (parametric `Errors[X]`, unioned `A | B`, dotted
      // `mod.Err`) but never a bare `constrained_type`, whose `:` would
      // otherwise swallow the function body colon in `def f() raises HALError:`.
      seq('raises', optional(field('raises_type', alias($._raises_type, $.type)))),
      // `capturing`/`escaping` may carry an origin list, e.g. `capturing[_]`.
      seq(choice('capturing', 'escaping'), optional($.capture_list)),
      'thin',
      // An ABI qualifier, e.g. `abi("C")`.
      $.abi_specifier,
    )),

    abi_specifier: ($) => seq('abi', '(', $.string, ')'),

    _raises_type: ($) => choice(
      prec(1, $.expression),
      $.generic_type,
      $.union_type,
      $.member_type,
    ),

    // The origin list is bound tighter than a trailing subscript so that the
    // `[_]` in `def() capturing[_] -> None` is part of the effect.
    capture_list: ($) =>
      prec(PREC.call + 1,
        seq('[', commaSep1(choice($.expression, $.wildcard_origin)), optional(','), ']')),

    wildcard_origin: (_) => '_',

    parameters: ($) => seq(
      '(',
      optional($._parameters),
      ')',
    ),

    lambda_parameters: ($) => $._parameters,

    list_splat: ($) => seq(
      '*',
      $.expression,
    ),

    dictionary_splat: ($) => seq(
      '**',
      $.expression,
    ),

    global_statement: ($) => seq(
      'global',
      commaSep1($.identifier),
    ),

    nonlocal_statement: ($) => seq(
      'nonlocal',
      commaSep1($.identifier),
    ),

    exec_statement: ($) => seq(
      'exec',
      field('code', choice($.string, $.identifier)),
      optional(
        seq(
          'in',
          commaSep1($.expression),
        ),
      ),
    ),

    type_alias_statement: ($) => prec.dynamic(1, seq(
      'type',
      field('left', $.type),
      '=',
      field('right', $.type),
    )),

    // A parameterized compile-time alias, e.g.
    //   comptime Ptr[mut: Bool, //, origin: Origin[mut=mut] = Default] = Value
    parameterized_alias_statement: ($) => prec.dynamic(1, seq(
      'comptime',
      field('name', $.identifier),
      field('type_parameters', $.type_parameter),
      // An optional trait/type bound on the alias, e.g.
      //   comptime It[...]: Iterator = Self
      optional(seq(':', field('type', $.type))),
      '=',
      field('value', $._right_hand_side),
    )),

    class_definition: ($) => seq(
      choice('class', 'struct'),
      field('name', $.identifier),
      field('type_parameters', optional($.type_parameter)),
      field(
        'superclasses',
        optional(alias($.superclass_list, $.argument_list)),
      ),
      ':',
      field('body', $._suite),
    ),

    // A struct conformance list, like an argument list except each entry may
    // carry `where` constraints, e.g. `Copyable where conforms_to(T, Copyable)`.
    superclass_list: ($) =>
      seq(
        '(',
        optional(
          commaSep1(
            seq(
              choice(
                $.expression,
                $.list_splat,
                $.dictionary_splat,
                alias($.parenthesized_list_splat, $.parenthesized_expression),
                $.keyword_argument,
              ),
              repeat($.where_clause),
            ),
          ),
        ),
        optional(','),
        ')',
      ),

    // The `[...]` parameter clause of a function, struct, or alias, also reused
    // for generic-type instantiation. Empty brackets are permitted.
    type_parameter: ($) => seq(
      '[',
      optional(seq(
        commaSep1(choice(
          $.infer_separator,
          $.keyword_separator,
          $.positional_separator,
          // Argument-convention soft keywords (`mut`, `out`) used as parameter
          // names or arguments, e.g. `mut: Bool`, `mut=mut`, or a bare `mut`.
          seq(
            alias(choice('mut', 'out'), $.identifier),
            optional(seq(':', field('type', $.type))),
            optional(seq('=', field('default', $._type_parameter_default))),
          ),
          seq(
            $.type,
            optional(seq('=', field('default', $._type_parameter_default))),
            repeat($.where_clause),
          ),
        )),
        optional(','),
      )),
      ']',
    ),

    // A type-parameter default may be any expression (covering parametric
    // instantiations and call chains like `Target[x].options()`), or a bare
    // convention keyword such as `mut` referencing an origin parameter.
    _type_parameter_default: ($) =>
      choice($.expression, alias(choice('mut', 'out'), $.identifier)),

    // The `//` marker separating infer-only parameters from explicit ones.
    infer_separator: (_) => '//',

    trait_definition: ($) => seq(
      'trait',
      field('name', $.identifier),
      field('supertraits', optional($.trait_list)),
      ':',
      field('body', seq($._indent, $.block)),
    ),

    trait_list: ($) => seq(
      '(',
      optional(commaSep1($.identifier)),
      optional(','),
      ')',
    ),

    parenthesized_list_splat: ($) => prec(PREC.parenthesized_list_splat, seq(
      '(',
      choice(
        alias($.parenthesized_list_splat, $.parenthesized_expression),
        $.list_splat,
      ),
      ')',
    )),

    argument_list: ($) => seq(
      '(',
      optional(commaSep1(
        choice(
          $.expression,
          $.list_splat,
          $.dictionary_splat,
          alias($.parenthesized_list_splat, $.parenthesized_expression),
          $.keyword_argument,
        ),
      )),
      optional(','),
      ')',
    ),

    parameter_list: ($) => seq(
      '[',
      optional(commaSep1(
        choice(
          seq(optional('inferred'), $.expression),
          $.list_splat,
          $.dictionary_splat,
          alias($.parenthesized_list_splat, $.parenthesized_expression),
          $.keyword_argument,
          // A callable type argument, e.g. `val.isa[def() -> Path]()`.
          $.function_type,
        ),
      )),
      optional(','),
      ']',
    ),

    decorated_definition: ($) => seq(
      repeat1($.decorator),
      field('definition', choice(
        $.class_definition,
        $.function_definition,
        $.trait_definition,
        // A decorated comptime alias, e.g.
        //   @deprecated(use=ImplicitlyDeletable)
        //   comptime X = ImplicitlyDeletable
        seq($.assignment, $._newline),
        seq($.parameterized_alias_statement, $._newline),
      )),
    ),

    if_statement: ($) =>
      seq(
        "if",
        field("condition", $.expression),
        ":",
        field("consequence", $._suite),
        repeat(field("alternative", $.elif_clause)),
        optional(field("alternative", $.else_clause)),
      ),

    elif_clause: ($) =>
      seq(
        "elif",
        field("condition", $.expression),
        ":",
        field("consequence", $._suite),
      ),

    else_clause: ($) => seq("else", ":", field("body", $._suite)),

    match_statement: ($) =>
      seq(
        "match",
        commaSep1(field("subject", $.expression)),
        optional(","),
        ":",
        field("body", alias($._match_block, $.block)),
      ),

    _match_block: ($) =>
      choice(
        seq($._indent, repeat(field("alternative", $.case_clause)), $._dedent),
        $._newline,
      ),

    case_clause: ($) =>
      seq(
        "case",
        commaSep1($.case_pattern),
        optional(","),
        optional(field("guard", $.if_clause)),
        ":",
        field("consequence", $._suite),
      ),

    for_statement: ($) =>
      seq(
        optional("async"),
        "for",
        // The loop variable may carry a convention, e.g. `for var arg in ...`
        // or `for ref item in ...`.
        optional($.argument_convention),
        field("left", $._left_hand_side),
        "in",
        field("right", $._expressions),
        ":",
        field("body", $._suite),
        field("alternative", optional($.else_clause)),
      ),

    while_statement: ($) =>
      seq(
        "while",
        field("condition", $.expression),
        ":",
        field("body", $._suite),
        optional(field("alternative", $.else_clause)),
      ),

    try_statement: ($) =>
      seq(
        "try",
        ":",
        field("body", $._suite),
        choice(
          seq(
            repeat1($.except_clause),
            optional($.else_clause),
            optional($.finally_clause),
          ),
          seq(
            repeat1($.except_group_clause),
            optional($.else_clause),
            optional($.finally_clause),
          ),
          $.finally_clause,
        ),
      ),

    except_clause: ($) =>
      seq(
        "except",
        optional(
          seq($.expression, optional(seq(choice("as", ","), $.expression))),
        ),
        ":",
        $._suite,
      ),

    except_group_clause: ($) =>
      seq(
        "except*",
        seq($.expression, optional(seq("as", $.expression))),
        ":",
        $._suite,
      ),

    finally_clause: ($) => seq("finally", ":", $._suite),

    with_statement: ($) =>
      seq(
        optional("async"),
        "with",
        $.with_clause,
        ":",
        field("body", $._suite),
      ),

    with_clause: ($) =>
      choice(
        seq(commaSep1($.with_item), optional(",")),
        seq("(", commaSep1($.with_item), optional(","), ")"),
      ),

    with_item: ($) => prec.dynamic(1, seq(field("value", $.expression))),



    decorator: ($) => seq("@", $.expression, $._newline),

    _suite: ($) =>
      choice(
        alias($._simple_statements, $.block),
        seq($._indent, $.block),
        alias($._newline, $.block),
      ),

    block: ($) => seq(repeat($._statement), $._dedent),

    expression_list: ($) =>
      prec.right(
        seq(
          $.expression,
          choice(",", seq(repeat1(seq(",", $.expression)), optional(","))),
        ),
      ),

    dotted_name: ($) => prec(1, sep1($.identifier, ".")),

    // Match cases

    case_pattern: ($) =>
      prec(
        1,
        choice(
          alias($._as_pattern, $.as_pattern),
          $.keyword_pattern,
          $._simple_pattern,
        ),
      ),

    _simple_pattern: ($) =>
      prec(
        1,
        choice(
          $.class_pattern,
          $.splat_pattern,
          $.union_pattern,
          alias($._list_pattern, $.list_pattern),
          alias($._tuple_pattern, $.tuple_pattern),
          $.dict_pattern,
          $.string,
          $.concatenated_string,
          $.true,
          $.false,
          $.none,
          seq(optional("-"), choice($.integer, $.float)),
          $.complex_pattern,
          $.dotted_name,
          "_",
        ),
      ),

    _as_pattern: ($) => seq($.case_pattern, "as", $.identifier),

    union_pattern: ($) =>
      prec.right(
        seq($._simple_pattern, repeat1(prec.left(seq("|", $._simple_pattern)))),
      ),

    _list_pattern: ($) =>
      seq("[", optional(seq(commaSep1($.case_pattern), optional(","))), "]"),

    _tuple_pattern: ($) =>
      seq("(", optional(seq(commaSep1($.case_pattern), optional(","))), ")"),

    dict_pattern: ($) =>
      seq(
        "{",
        optional(
          seq(
            commaSep1(choice($._key_value_pattern, $.splat_pattern)),
            optional(","),
          ),
        ),
        "}",
      ),

    _key_value_pattern: ($) =>
      seq(field("key", $._simple_pattern), ":", field("value", $.case_pattern)),

    keyword_pattern: ($) => seq($.identifier, "=", $._simple_pattern),

    splat_pattern: ($) =>
      prec(1, seq(choice("*", "**"), choice($.identifier, "_"))),

    class_pattern: ($) =>
      seq(
        $.dotted_name,
        "(",
        optional(seq(commaSep1($.case_pattern), optional(","))),
        ")",
      ),

    complex_pattern: ($) =>
      prec(
        1,
        seq(
          optional("-"),
          choice($.integer, $.float),
          choice("+", "-"),
          choice($.integer, $.float),
        ),
      ),

    // Patterns

    _parameters: ($) => seq(commaSep1($.parameter), optional(",")),

    _patterns: ($) => seq(commaSep1($.pattern), optional(",")),

    parameter: ($) =>
      choice(
        $.self_parameter,
        $.identifier,
        $.typed_parameter,
        $.default_parameter,
        $.typed_default_parameter,
        $.list_splat_pattern,
        $.tuple_pattern,
        $.keyword_separator,
        $.positional_separator,
        $.dictionary_splat_pattern,
      ),

    pattern: ($) =>
      choice(
        $.identifier,
        $.keyword_identifier,
        $.subscript,
        $.attribute,
        $.list_splat_pattern,
        $.tuple_pattern,
        $.list_pattern,
      ),

    tuple_pattern: ($) => seq("(", optional($._patterns), ")"),

    list_pattern: ($) => seq("[", optional($._patterns), "]"),

    // The `ref` origin convention, optionally carrying one or more arguments,
    // e.g. `ref[origin]` or `ref[origin, address_space]`.
    _ref_convention: ($) =>
      prec(1, seq("ref", "[", commaSep1($.expression), optional(","), "]")),
    argument_convention: ($) =>
      choice(
        "borrowed",
        "inout",
        "owned",
        "out",
        "read",
        "mut",
        "var",
        "deinit",
        "ref",
        $._ref_convention,
      ),

    unified_clause: ($) =>
      seq(
        "unified",
        "{",
        commaSep1(seq($.argument_convention, $.identifier)),
        optional(","),
        "}",
      ),

    where_clause: ($) => seq("where", $.expression),

    self_parameter: ($) =>
      prec.right(seq(
        optional($.argument_convention),
        SELF,
        optional(seq(":", field("type", $.type))),
      )),

    typed_parameter: ($) =>
      prec(
        PREC.typed_parameter,
        seq(
          seq(
            optional($.argument_convention),
            choice(
              $.identifier,
              $.list_splat_pattern,
              $.dictionary_splat_pattern,
            ),
          ),
          ":",
          field("type", $.type),
        ),
      ),

    default_parameter: ($) =>
      seq(
        field("name", choice($.identifier, $.tuple_pattern)),
        "=",
        field("value", $.expression),
      ),

    typed_default_parameter: ($) =>
      prec(
        PREC.typed_parameter,
        seq(
          optional($.argument_convention),
          field("name", $.identifier),
          ":",
          field("type", $.type),
          "=",
          field("value", $.expression),
        ),
      ),

    list_splat_pattern: ($) =>
      seq(
        "*",
        choice($.identifier, $.keyword_identifier, $.subscript, $.attribute),
      ),

    dictionary_splat_pattern: ($) =>
      seq(
        "**",
        choice($.identifier, $.keyword_identifier, $.subscript, $.attribute),
      ),

    // Extended patterns (patterns allowed in match statement are far more flexible than simple patterns though still a subset of "expression")

    as_pattern: ($) =>
      prec.left(
        seq(
          $.expression,
          "as",
          field("comptime", alias($.expression, $.as_pattern_target)),
        ),
      ),

    // Expressions

    _expression_within_for_in_clause: ($) =>
      choice($.expression, alias($.lambda_within_for_in_clause, $.lambda)),

    expression: ($) =>
      choice(
        $.comparison_operator,
        $.not_operator,
        $.boolean_operator,
        $.lambda,
        $.primary_expression,
        $.conditional_expression,
        $.named_expression,
        $.as_pattern,
      ),

    primary_expression: ($) =>
      choice(
        $.await,
        $.binary_operator,
        $.identifier,
        $.keyword_identifier,
        $.string,
        $.concatenated_string,
        $.integer,
        $.float,
        $.true,
        $.false,
        $.none,
        $.unary_operator,
        $.transfer_expression,
        $.attribute,
        choice(prec.dynamic(-1, $.subscript), prec.dynamic(1, $.call)),
        $.list,
        $.list_comprehension,
        $.dictionary,
        $.dictionary_comprehension,
        $.set,
        $.set_comprehension,
        $.struct_literal,
        $.tuple,
        $.parenthesized_expression,
        $.generator_expression,
        $.ellipsis,
        alias($.list_splat_pattern, $.list_splat),
        $.mlir_type,
        $.comptime_expression,
      ),

    // `comptime` applied to a parenthesized expression in value position, e.g.
    // `result[i] = comptime (StaticString(raw[i]))`.
    comptime_expression: ($) =>
      prec(PREC.call, seq("comptime", $.parenthesized_expression)),

    // The postfix transfer/consume operator, e.g. `result^`.
    transfer_expression: ($) =>
      prec(PREC.call, seq(field("value", $.primary_expression), "^")),

    not_operator: ($) =>
      prec(PREC.not, seq("not", field("argument", $.expression))),

    boolean_operator: ($) =>
      choice(
        prec.left(
          PREC.and,
          seq(
            field("left", $.expression),
            field("operator", "and"),
            field("right", $.expression),
          ),
        ),
        prec.left(
          PREC.or,
          seq(
            field("left", $.expression),
            field("operator", "or"),
            field("right", $.expression),
          ),
        ),
      ),

    binary_operator: ($) => {
      const table = [
        [prec.left, "+", PREC.plus],
        [prec.left, "-", PREC.plus],
        [prec.left, "*", PREC.times],
        [prec.left, "@", PREC.times],
        [prec.left, "/", PREC.times],
        [prec.left, "%", PREC.times],
        [prec.left, "//", PREC.times],
        [prec.right, "**", PREC.power],
        [prec.left, "|", PREC.bitwise_or],
        [prec.left, "&", PREC.bitwise_and],
        [prec.left, "^", PREC.xor],
        [prec.left, "<<", PREC.shift],
        [prec.left, ">>", PREC.shift],
      ];

      // @ts-ignore
      return choice(
        ...table.map(([fn, operator, precedence]) =>
          fn(
            precedence,
            seq(
              field("left", $.primary_expression),
              // @ts-ignore
              field("operator", operator),
              field("right", $.primary_expression),
            ),
          ),
        ),
      );
    },

    unary_operator: ($) =>
      prec(
        PREC.unary,
        seq(
          field("operator", choice("+", "-", "~")),
          field("argument", $.primary_expression),
        ),
      ),

    _not_in: (_) => seq("not", "in"),

    _is_not: (_) => seq("is", "not"),

    comparison_operator: ($) =>
      prec.left(
        PREC.compare,
        seq(
          $.primary_expression,
          repeat1(
            seq(
              field(
                "operators",
                choice(
                  "<",
                  "<=",
                  "==",
                  "!=",
                  ">=",
                  ">",
                  "<>",
                  "in",
                  alias($._not_in, "not in"),
                  "is",
                  alias($._is_not, "is not"),
                ),
              ),
              $.primary_expression,
            ),
          ),
        ),
      ),

    lambda: ($) =>
      prec(
        PREC.lambda,
        seq(
          "lambda",
          field("parameters", optional($.lambda_parameters)),
          ":",
          field("body", $.expression),
        ),
      ),

    lambda_within_for_in_clause: ($) =>
      seq(
        "lambda",
        field("parameters", optional($.lambda_parameters)),
        ":",
        field("body", $._expression_within_for_in_clause),
      ),

    assignment: ($) =>
      seq(
        optional(choice("var", "comptime", "ref")),
        field("left", $._left_hand_side),
        choice(
          seq("=", field("right", $._right_hand_side)),
          seq(":", field("type", $.type)),
          seq(
            ":",
            field("type", $.type),
            "=",
            field("right", $._right_hand_side),
          ),
        ),
      ),

    augmented_assignment: ($) =>
      seq(
        field("left", $._left_hand_side),
        field(
          "operator",
          choice(
            "+=",
            "-=",
            "*=",
            "/=",
            "@=",
            "//=",
            "%=",
            "**=",
            ">>=",
            "<<=",
            "&=",
            "^=",
            "|=",
          ),
        ),
        field("right", $._right_hand_side),
      ),

    // A backtick-quoted (raw) identifier used as a binding name lexes as a
    // (string), e.g. ``var `6bit` = ...`` or ``comptime `\x1e` = ...``. A call
    // result may also be an assignment target, e.g. `self.get(i) = x` or
    // `node[].right() = other`.
    _left_hand_side: ($) => choice($.pattern, $.pattern_list, $.string, $.call),

    pattern_list: ($) =>
      seq(
        $.pattern,
        choice(",", seq(repeat1(seq(",", $.pattern)), optional(","))),
      ),

    _right_hand_side: ($) =>
      choice(
        $.expression,
        $.expression_list,
        $.assignment,
        $.augmented_assignment,
        $.pattern_list,
        $.yield,
        // A callable type as the value, e.g. `comptime F = def() -> None`.
        $.function_type,
      ),

    yield: ($) =>
      prec.right(
        seq(
          "yield",
          choice(seq("from", $.expression), optional($._expressions)),
        ),
      ),

    attribute: ($) =>
      prec(
        PREC.call,
        seq(
          field("object", $.primary_expression),
          ".",
          choice(
            field("attribute", choice(
              $.identifier,
              'var',
              'comptime',
              'ref',
              'read',
              'mut',
              'out',
              'deinit',
              'unified',
              'where',
            )),
            // A backtick-quoted MLIR member, e.g. the `pop.cast` in
            // ``__mlir_op.`pop.cast` ``.
            field("attribute", $.mlir_fragment),
          ),
        ),
      ),

    subscript: ($) =>
      prec(
        PREC.call,
        seq(
          field("value", $.primary_expression),
          "[",
          // Empty brackets are allowed for parametric instantiation, e.g.
          // `_CString[]`, where every parameter is inferred or defaulted.
          optional(seq(
            commaSep1(field("subscript", choice(
              $.expression,
              $.slice,
              $.keyword_argument,
              // A keyword argument whose value is a slice, e.g. `x[byte=1:n]`.
              alias($.slice_keyword_argument, $.keyword_argument),
              // A callable type argument, e.g. `Variant[def() -> Path]`.
              $.function_type,
              // A bare convention keyword used as a parameter argument, e.g.
              // the `mut` in `unsafe_mut_cast[mut]`.
              alias(choice("mut", "out"), $.identifier),
            ))),
            optional(","),
          )),
          "]",
        ),
      ),

    slice: ($) =>
      seq(
        optional($.expression),
        ":",
        optional($.expression),
        optional(seq(":", optional($.expression))),
      ),

    ellipsis: (_) => "...",

    call: ($) =>
      prec(
        PREC.call,
        seq(
          field("function", $.primary_expression),
          optional($.parameter_list),
          field("arguments", choice($.generator_expression, $.argument_list)),
        ),
      ),

    type: ($) => choice(
      prec(1, $.expression),
      $.splat_type,
      $.generic_type,
      $.called_type,
      $.union_type,
      $.intersection_type,
      $.constrained_type,
      $.member_type,
      $.function_type,
    ),

    // A parametric instantiation that is immediately called, used in type
    // position, e.g. `Device[get_device_spec[0]()]`. The generic-type reading
    // would otherwise consume `Name[...]` and strand the trailing `()`. A
    // trailing member-call chain (`get_device_spec[0]()._mlir_target()`) and a
    // dotted parametric base (`TypeList.splat[...]()`) are also supported.
    called_type: ($) => prec.right(PREC.call, seq(
      choice(
        $.generic_type,
        seq($.member_type, optional($.type_parameter)),
      ),
      $.argument_list,
      repeat(seq('.', $.identifier, optional($.type_parameter), optional($.argument_list))),
    )),
    // A callable type literal, e.g. `def(Int) raises -> Bool` or
    // `def() capturing -> Path`, usable anywhere a type is expected.
    function_type: ($) => prec.right(seq(
      'def',
      // A callable type may carry a compile-time parameter clause before its
      // value parameters, e.g. `def[width: Int, alignment: Int = 1](Coord)`.
      field('type_parameters', optional($.type_parameter)),
      // A callable type's parameters are types (optionally named or variadic),
      // e.g. `def(Int, OpaquePointer[X]) -> None`, and may carry an argument
      // convention, e.g. `def(mut Bencher, T)`.
      '(',
      optional(seq(
        commaSep1(seq(
          optional($.argument_convention),
          field('parameter', $.type),
        )),
        optional(','),
      )),
      ')',
      optional($._function_effects),
      optional($.result_convention),
      optional(seq(
        '->',
        optional($._ref_convention),
        field('return_type', $.type),
      )),
    )),
    splat_type: ($) => prec.right(1, seq(
      choice('*', '**'),
      choice(
        $.identifier,
        $.attribute,
        $.subscript,
        $.generic_type,
        $.member_type,
        $.called_type,
      ),
    )),
    generic_type: ($) => prec(1, seq(
      choice(
        $.identifier,
        alias('type', $.identifier),
      ),
      $.type_parameter,
    )),
    union_type: ($) => prec.left(seq($.type, '|', $.type)),
    // The `&` intersection/conjunction type operator combining trait/types with
    // a callable type, e.g. `Copyable & RegisterPassable & def() -> None`. A
    // trailing `function_type` is required, so a plain `A & B` of identifiers
    // still parses as a `binary_operator`; only the presence of a `def` operand
    // selects the intersection reading.
    intersection_type: ($) =>
      prec.left(PREC.bitwise_and, seq(
        $._intersection_operand,
        repeat(seq('&', $._intersection_operand)),
        '&',
        $.function_type,
      )),

    _intersection_operand: ($) => choice($.identifier, $.generic_type),
    constrained_type: ($) => prec.right(seq($.type, ':', $.type)),
    member_type: ($) => seq($.type, '.', $.identifier),

    // A subscript keyword argument whose value is a slice, e.g. `x[byte=1:n]`.
    slice_keyword_argument: ($) =>
      seq(
        field("name", choice(
          $.identifier,
          $.keyword_identifier,
          alias(choice("mut", "out"), $.identifier),
        )),
        "=",
        field("value", $.slice),
      ),

    keyword_argument: ($) =>
      seq(
        field("name", choice(
          $.identifier,
          $.keyword_identifier,
          // Argument-convention soft keywords used as parameter names, e.g.
          // the `mut` in `Origin[mut=True]`.
          alias(choice("mut", "out"), $.identifier),
        )),
        "=",
        field("value", choice(
          $.expression,
          // A convention keyword used as the argument value, e.g. `mut=mut`.
          alias(choice("mut", "out"), $.identifier),
        )),
      ),

    // Literals

    // A backtick-quoted MLIR fragment whose interior is tokenized by the
    // external scanner (see scanner.c) into typed identifiers, numbers and
    // punctuation, e.g. `pop.cast`, `!co.routine`, `0:index` or
    // `#kgen.dtype.constant<ui8> : !kgen.dtype`. This lets the interior be
    // highlighted as MLIR rather than as an opaque string.
    mlir_fragment: ($) =>
      seq(
        $._mlir_backtick,
        repeat(choice(
          alias($._mlir_ident, $.type),
          alias($._mlir_number, $.integer),
          $.mlir_punctuation,
        )),
        $._mlir_backtick,
      ),

    // MLIR type interop. A type is a plain dotted member
    // (`__mlir_type.index`), a backtick-quoted MLIR type fragment
    // (``__mlir_type.`!co.routine` ``), or a bracketed parametric type that
    // interpolates expressions between backtick fragments
    // (``__mlir_type[`!pop.array<`, size, `>`] ``). Backtick fragments are
    // lexed as (string) tokens, so arbitrary MLIR syntax inside them is opaque.
    mlir_type: ($) =>
      prec.right(
        seq(
          "__mlir_type",
          choice(
            seq(".", choice(alias($.identifier, $.type), $.mlir_fragment)),
            seq("[", commaSep1($.expression), optional(","), "]"),
          ),
        ),
      ),

    list: ($) => seq("[", optional($._collection_elements), "]"),

    set: ($) => seq("{", $._collection_elements, "}"),

    tuple: ($) => seq("(", optional($._collection_elements), ")"),

    dictionary: ($) =>
      seq(
        "{",
        optional(commaSep1(choice($.pair, $.dictionary_splat))),
        optional(","),
        "}",
      ),

    pair: ($) =>
      seq(field("key", $.expression), ":", field("value", $.expression)),

    // A struct/initializer literal, e.g. `{ ptr = p, length = n }` or
    // `{ ctx, name = value }` mixing positional and named fields.
    struct_literal: ($) =>
      prec.dynamic(-1, seq(
        "{",
        commaSep1(choice($.struct_literal_field, $.expression)),
        optional(","),
        "}",
      )),

    struct_literal_field: ($) =>
      seq(
        field("name", choice($.identifier, $.keyword_identifier)),
        "=",
        field("value", $.expression),
      ),

    list_comprehension: ($) =>
      seq("[", field("body", $.expression), $._comprehension_clauses, "]"),

    dictionary_comprehension: ($) =>
      seq("{", field("body", $.pair), $._comprehension_clauses, "}"),

    set_comprehension: ($) =>
      seq("{", field("body", $.expression), $._comprehension_clauses, "}"),

    generator_expression: ($) =>
      seq("(", field("body", $.expression), $._comprehension_clauses, ")"),

    _comprehension_clauses: ($) =>
      seq($.for_in_clause, repeat(choice($.for_in_clause, $.if_clause))),

    parenthesized_expression: ($) =>
      prec(
        PREC.parenthesized_expression,
        seq("(", choice($.expression, $.yield), ")"),
      ),

    _collection_elements: ($) =>
      seq(
        commaSep1(
          choice(
            $.expression,
            $.yield,
            $.list_splat,
            $.parenthesized_list_splat,
          ),
        ),
        optional(","),
      ),

    for_in_clause: ($) =>
      prec.left(
        seq(
          optional("async"),
          "for",
          optional($.argument_convention),
          field("left", $._left_hand_side),
          "in",
          field("right", commaSep1($._expression_within_for_in_clause)),
          optional(","),
        ),
      ),

    if_clause: ($) => seq("if", $.expression),

    conditional_expression: ($) =>
      prec.right(
        PREC.conditional,
        seq($.expression, "if", $.expression, "else", $.expression),
      ),

    concatenated_string: ($) => seq($.string, repeat1($.string)),

    string: ($) =>
      seq(
        $.string_start,
        repeat(choice($.interpolation, $.string_content)),
        $.string_end,
      ),

    string_content: ($) =>
      prec.right(
        repeat1(
          choice(
            $.escape_interpolation,
            $.escape_sequence,
            $._not_escape_sequence,
            $._string_content,
          ),
        ),
      ),

    interpolation: ($) =>
      seq(
        "{",
        field("expression", $._f_expression),
        optional("="),
        optional(field("type_conversion", $.type_conversion)),
        optional(field("format_specifier", $.format_specifier)),
        "}",
      ),

    _f_expression: ($) =>
      choice($.expression, $.expression_list, $.pattern_list, $.yield),

    escape_sequence: (_) =>
      token.immediate(
        prec(
          1,
          seq(
            "\\",
            choice(
              /u[a-fA-F\d]{4}/,
              /U[a-fA-F\d]{8}/,
              /x[a-fA-F\d]{2}/,
              /\d{1,3}/,
              /\r?\n/,
              /['"abfrntv\\]/,
              /N\{[^}]+\}/,
            ),
          ),
        ),
      ),

    _not_escape_sequence: (_) => token.immediate("\\"),

    format_specifier: ($) =>
      seq(
        ":",
        repeat(
          choice(
            token(prec(1, /[^{}\n]+/)),
            alias($.interpolation, $.format_expression),
          ),
        ),
      ),

    type_conversion: (_) => /![a-z]/,

    integer: (_) =>
      token(
        choice(
          seq(choice("0x", "0X"), repeat1(/_?[A-Fa-f0-9]+/), optional(/[Ll]/)),
          seq(choice("0o", "0O"), repeat1(/_?[0-7]+/), optional(/[Ll]/)),
          seq(choice("0b", "0B"), repeat1(/_?[0-1]+/), optional(/[Ll]/)),
          seq(
            repeat1(/[0-9]+_?/),
            choice(
              optional(/[Ll]/), // long numbers
              optional(/[jJ]/), // complex numbers
            ),
          ),
        ),
      ),

    float: (_) => {
      const digits = repeat1(/[0-9]+_?/);
      const exponent = seq(/[eE][\+-]?/, digits);

      return token(
        seq(
          choice(
            seq(digits, ".", optional(digits), optional(exponent)),
            seq(optional(digits), ".", digits, optional(exponent)),
            seq(digits, exponent),
          ),
          optional(/[jJ]/),
        ),
      );
    },

    identifier: (_) => /[_\p{XID_Start}][_\p{XID_Continue}]*/,

    keyword_identifier: ($) =>
      choice(
        prec(
          -3,
          alias(choice("print", "exec", "async", "await"), $.identifier),
        ),
        // `mut`/`out` used as ordinary names or values, e.g. `mut == False`.
        prec(-3, alias(choice("mut", "out"), $.identifier)),
        alias(choice("type", "match"), $.identifier),
      ),

    true: (_) => "True",
    false: (_) => "False",
    none: (_) => "None",

    await: ($) => prec(PREC.unary, seq("await", $.primary_expression)),

    comment: (_) => token(seq("#", /.*/)),

    line_continuation: (_) =>
      token(seq("\\", choice(seq(optional("\r"), "\n"), "\0"))),

    positional_separator: (_) => "/",
    keyword_separator: (_) => "*",
  },
});

module.exports.PREC = PREC;

/**
 * Creates a rule to match one or more of the rules separated by a comma
 *
 * @param {RuleOrLiteral} rule
 *
 * @returns {SeqRule}
 */
function commaSep1(rule) {
  return sep1(rule, ",");
}

/**
 * Creates a rule to match one or more occurrences of `rule` separated by `sep`
 *
 * @param {RuleOrLiteral} rule
 *
 * @param {RuleOrLiteral} separator
 *
 * @returns {SeqRule}
 */
function sep1(rule, separator) {
  return seq(rule, repeat(seq(separator, rule)));
}
