# Languages and language servers

Languages and servers declared in this crate. A server is disabled until `lsp.<server key>.binary.path` is set in Zed settings.

| Language key | File suffixes | LSP server key | Source |
| --- | --- | --- | --- |
| `Swift` | `.swift` | `sourcekit-lsp` | `src/swift.rs` |

- **Language key**: the name used under `languages` in settings (from `name` in `src/<lang>/config.toml`).
- **LSP server key**: the name used under `lsp` and in `languages.<lang>.language_servers`.

## Example settings

```json
{
  "lsp": {
    "sourcekit-lsp": {
      "binary": {
        "path": "/usr/bin/sourcekit-lsp",
        "arguments": []
      }
    }
  },
  "languages": {
    "Swift": {
      "language_servers": ["sourcekit-lsp"],
      "tab_size": 4
    }
  }
}
```

On macOS, `xcrun --find sourcekit-lsp` prints the binary path.
