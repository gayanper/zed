# lsp_dsl

Fork-owned languages and language servers, declared with small macros and registered from `lsp_dsl::init` (called after `languages::init` in Zed, its tests, and the remote server).

## Add a language

1. Add the grammar crate (`tree-sitter-<lang>`) to `crates/lsp_dsl/Cargo.toml`.
2. Create `src/<lang>/config.toml` (with `grammar = "<lang>"`) and the `*.scm` query files, as in `crates/grammars/src/<lang>/`. Folders named `lsp_dsl_test_*` are test-only and not embedded in builds.
3. Create `src/<lang>.rs`:

```rust
crate::language! {
    name: "<lang>",                      // folder under src/, also the grammar name
    grammar: tree_sitter_<lang>::LANGUAGE,
}
```

## Add a language server

In the same `src/<lang>.rs` (one server per module; use a submodule for more):

```rust
crate::lsp_server! {
    name: "my-server",
    languages: ["MyLang"],                          // language names from config.toml
    initialization_options: json!({ ... }),         // optional
    workspace_configuration: json!({ ... }),        // optional
    adapter: MyAdapter,                             // optional: LspAdapter + LspInstaller impl
}
```

Optional fields must appear in the order shown. With `adapter:`, the declared `initialization_options` and `workspace_configuration` are ignored, and the adapter's `name()` must equal `name`.

## Register

```rust
pub fn init(languages: Arc<LanguageRegistry>, cx: &mut App) {
    register!(languages, cx, [mylang::LANGUAGE], [mylang::LSP_SERVER]);
}
```

## Enable a server

Servers have no default binary and are never downloaded or looked up on `PATH`. A server stays disabled until its binary path is set in user settings:

```json
{
  "lsp": {
    "my-server": {
      "binary": { "path": "/path/to/server", "arguments": ["--stdio"] }
    }
  }
}
```

Setting the path starts the server without a restart; removing it stops it. User `initialization_options` and `settings` merge with the declared defaults as for built-in servers.

- Only global/user settings enable a server. A path set only in a project `.zed/settings.json` does not.
- Over SSH/remote, the path is resolved on the remote host, so the binary must exist there.
- A language or server here replaces a built-in or extension one with the same name, and a warning is logged.
- Settings autocomplete may not list these servers.
