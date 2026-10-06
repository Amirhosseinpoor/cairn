(function_item name: (identifier) @name) @def
(function_signature_item name: (identifier) @name) @def
(struct_item name: (type_identifier) @name) @def
(enum_item name: (type_identifier) @name) @def
(trait_item name: (type_identifier) @name) @def
(impl_item type: (type_identifier) @name) @def
(impl_item type: (generic_type type: (type_identifier) @name)) @def
(mod_item name: (identifier) @name) @def
(use_declaration) @import
(call_expression function: (identifier) @call)
(call_expression function: (scoped_identifier name: (identifier) @call))
(call_expression function: (field_expression field: (field_identifier) @call))
(type_identifier) @type_ref
