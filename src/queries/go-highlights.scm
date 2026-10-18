; IntelliJ-style distinctions beyond the grammar's basic categories.
(package_identifier) @namespace
(selector_expression operand: (identifier) @namespace.candidate)
(const_spec name: (identifier) @constant.declaration)
(type_spec name: (type_identifier) @type.declaration)
(type_alias name: (type_identifier) @type.declaration)
(method_declaration receiver: (parameter_list (parameter_declaration type: (type_identifier) @type.receiver)))
(method_declaration receiver: (parameter_list (parameter_declaration type: (pointer_type (type_identifier) @type.receiver))))
(call_expression function: (identifier) @function.call)
(call_expression function: (selector_expression field: (field_identifier) @function.call))
((type_identifier) @type.builtin
 (#match? @type.builtin "^(bool|byte|complex64|complex128|error|float32|float64|int|int8|int16|int32|int64|rune|string|uint|uint8|uint16|uint32|uint64|uintptr|any|comparable)$"))
