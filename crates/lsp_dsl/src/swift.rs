crate::language! {
    name: "swift",
    grammar: tree_sitter_swift::LANGUAGE,
}

crate::lsp_server! {
    name: "sourcekit-lsp",
    languages: ["Swift"],
}
