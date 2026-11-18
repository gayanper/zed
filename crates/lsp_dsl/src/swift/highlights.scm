[
  (comment)
  (multiline_comment)
] @comment

(shebang_line) @preproc

(directive) @preproc

(diagnostic) @preproc

(attribute) @attribute

(simple_identifier) @variable

(type_identifier) @type

(self_expression) @variable.special

(super_expression) @variable.special

(class_declaration
  name: (type_identifier) @type)

(protocol_declaration
  name: (type_identifier) @type)

(typealias_declaration
  name: (type_identifier) @type)

(type_parameter
  (type_identifier) @type)

(function_declaration
  name: (simple_identifier) @function)

(protocol_function_declaration
  name: (simple_identifier) @function)

(macro_declaration
  (simple_identifier) @function)

(init_declaration
  "init" @constructor)

(deinit_declaration
  "deinit" @constructor)

(call_expression
  (simple_identifier) @function)

(call_expression
  (navigation_expression
    suffix: (navigation_suffix
      suffix: (simple_identifier) @function)))

(macro_invocation
  (simple_identifier) @function)

(navigation_suffix
  suffix: (simple_identifier) @property)

(property_declaration
  name: (pattern
    bound_identifier: (simple_identifier) @property))

(protocol_property_declaration
  name: (pattern
    bound_identifier: (simple_identifier) @property))

(enum_entry
  name: (simple_identifier) @constant)

(parameter
  external_name: (simple_identifier) @label)

(value_argument_label) @label

(statement_label) @label

(import_declaration
  (identifier) @namespace)

[
  (line_str_text)
  (multi_line_str_text)
  "\""
  "\"\"\""
] @string

[
  (raw_str_part)
  (raw_str_end_part)
] @string

(str_escaped_char) @string.escape

(regex_literal) @string.regex

[
  (integer_literal)
  (hex_literal)
  (oct_literal)
  (bin_literal)
  (real_literal)
] @number

(boolean_literal) @boolean

(nil_literal) @constant.builtin

(special_literal) @constant.builtin

[
  "actor"
  "as"
  "associatedtype"
  "class"
  "deinit"
  "enum"
  "extension"
  "func"
  "import"
  "init"
  "let"
  "macro"
  "operator"
  "precedencegroup"
  "protocol"
  "struct"
  "subscript"
  "typealias"
  "var"
  "is"
  "in"
  "some"
  "any"
  "each"
  "async"
  "await"
  "try"
  "consume"
  "discard"
  "get"
  "set"
  "_modify"
  "willSet"
  "didSet"
] @keyword

[
  "if"
  "guard"
  "switch"
  "case"
  "for"
  "while"
  "repeat"
  "do"
  "break"
  "continue"
  "fallthrough"
  "return"
  "yield"
] @keyword

[
  (throws)
  (throw_keyword)
  (where_keyword)
  (catch_keyword)
  (else)
  (default_keyword)
  (visibility_modifier)
  (member_modifier)
  (function_modifier)
  (property_modifier)
  (parameter_modifier)
  (inheritance_modifier)
  (mutation_modifier)
  (ownership_modifier)
  (property_behavior_modifier)
] @keyword

[
  "("
  ")"
  "["
  "]"
  "{"
  "}"
] @punctuation.bracket

[
  ","
  ":"
  ";"
  "."
] @punctuation.delimiter

[
  "!"
  "!="
  "!=="
  "%"
  "%="
  "&"
  "&&"
  "*"
  "*="
  "+"
  "++"
  "+="
  "-"
  "--"
  "-="
  "->"
  "..."
  "..<"
  "/"
  "/="
  "<"
  "<<"
  "<="
  "="
  "=="
  "==="
  ">"
  ">="
  ">>"
  "?"
  "??"
  "^"
  "|"
  "||"
  "~"
  (custom_operator)
  (bang)
] @operator
