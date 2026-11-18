(attribute) @annotation

[
  (comment)
  (multiline_comment)
] @annotation

(class_declaration
  declaration_kind: _ @context
  name: (_) @name) @item

(protocol_declaration
  declaration_kind: _ @context
  name: (_) @name) @item

(function_declaration
  "func" @context
  name: (simple_identifier) @name) @item

(protocol_function_declaration
  "func" @context
  name: (simple_identifier) @name) @item

(init_declaration
  name: _ @name) @item

(deinit_declaration
  "deinit" @name) @item

(subscript_declaration
  "subscript" @name) @item

(property_declaration
  (value_binding_pattern
    mutability: _ @context)
  name: (pattern
    bound_identifier: (simple_identifier) @name)) @item

(protocol_property_declaration
  name: (pattern
    bound_identifier: (simple_identifier) @name)) @item

(typealias_declaration
  "typealias" @context
  name: (type_identifier) @name) @item

(associatedtype_declaration
  "associatedtype" @context
  name: (_) @name) @item

(enum_entry
  "case" @context
  name: (simple_identifier) @name) @item
