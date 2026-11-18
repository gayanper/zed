(function_declaration
  body: (function_body
    "{"
    (_)* @function.inside
    "}")) @function.around

(init_declaration
  body: (function_body
    "{"
    (_)* @function.inside
    "}")) @function.around

(deinit_declaration
  body: (function_body
    "{"
    (_)* @function.inside
    "}")) @function.around

(protocol_function_declaration) @function.around

(lambda_literal
  "{"
  (_)* @function.inside
  "}") @function.around

(class_declaration
  body: (_
    "{"
    (_)* @class.inside
    "}")) @class.around

(protocol_declaration
  body: (protocol_body
    "{"
    (_)* @class.inside
    "}")) @class.around

[
  (comment)
  (multiline_comment)
] @comment.around
