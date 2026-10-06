(function_definition declarator: (function_declarator declarator: (identifier) @name)) @def
(function_definition declarator: (function_declarator declarator: (qualified_identifier name: (identifier) @name))) @def
(function_definition declarator: (function_declarator declarator: (field_identifier) @name)) @def
(struct_specifier name: (type_identifier) @name) @def
(class_specifier name: (type_identifier) @name) @def
(preproc_include) @import
(call_expression function: (identifier) @call)
(type_identifier) @type_ref
