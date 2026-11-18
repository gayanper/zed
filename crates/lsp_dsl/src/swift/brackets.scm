("(" @open
  ")" @close)

("[" @open
  "]" @close)

("{" @open
  "}" @close)

(type_arguments
  "<" @open
  ">" @close)

(type_parameters
  "<" @open
  ">" @close)

(("\"" @open
  "\"" @close)
  (#set! rainbow.exclude))
