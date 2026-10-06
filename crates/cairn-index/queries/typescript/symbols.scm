(function_declaration name: (identifier) @name) @def
(generator_function_declaration name: (identifier) @name) @def
(class_declaration name: (type_identifier) @name) @def
(interface_declaration name: (type_identifier) @name) @def
(method_definition name: (property_identifier) @name) @def
(variable_declarator name: (identifier) @name) @def
(arrow_function) @def_inline
(import_statement) @import
(call_expression function: (identifier) @call)
(call_expression function: (member_expression property: (property_identifier) @call))
(type_identifier) @type_ref
