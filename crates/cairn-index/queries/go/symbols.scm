(function_declaration name: (identifier) @name) @def
(method_declaration name: (field_identifier) @name) @def
(type_declaration (type_spec name: (type_identifier) @name)) @def
(import_spec) @import
(call_expression function: (identifier) @call)
(call_expression function: (selector_expression field: (field_identifier) @call))
(type_identifier) @type_ref
