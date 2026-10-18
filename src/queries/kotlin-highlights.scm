(identifier) @variable
(class_declaration name: (identifier) @type)
(object_declaration name: (identifier) @type)
(type_alias type: (identifier) @type)
(user_type (identifier) @type)
(function_declaration name: (identifier) @function)
(call_expression . (identifier) @function)
(call_expression . (navigation_expression (identifier) @function.method .))
(navigation_expression (identifier) @property .)
(parameter . (identifier) @variable.parameter)
(class_parameter (identifier) @variable.parameter)
(annotation (user_type (identifier) @attribute))
(package_header (qualified_identifier (identifier) @namespace))
(import (qualified_identifier (identifier) @namespace))
[(line_comment) (block_comment)] @comment
[(string_literal) (multiline_string_literal) (character_literal)] @string
(escape_sequence) @escape
[(number_literal) (float_literal)] @number
((identifier) @constant.builtin (#match? @constant.builtin "^(true|false|null)$"))
((identifier) @constant (#match? @constant "^[A-Z][A-Z_0-9]+$"))
["abstract" "actual" "annotation" "as" "by" "catch" "class" "companion" "const" "constructor" "crossinline" "data" "do" "dynamic" "else" "enum" "expect" "external" "final" "finally" "for" "fun" "get" "if" "import" "in" "infix" "init" "inline" "inner" "interface" "internal" "is" "lateinit" "noinline" "object" "open" "operator" "out" "override" "package" "private" "protected" "public" "return" "sealed" "set" "super" "suspend" "tailrec" "this" "throw" "try" "typealias" "val" "value" "var" "vararg" "when" "where" "while"] @keyword
(interpolation "${" @punctuation.special "}" @punctuation.special)

((call_expression . (identifier) @constructor) (#match? @constructor "^[A-Z]"))
(annotation (constructor_invocation (user_type (identifier) @attribute)))
