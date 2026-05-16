scry — VS Code extension
========================

LSP-style code intel (autocomplete + jump-to-def + find-refs +
document outline) backed by the scry static binary. One persistent
`scry serve` per VS Code window; standard provider APIs surface
the results so `Ctrl+Space` / `F12` / `Shift+F12` / `Ctrl+Shift+O`
all work out of the box.

Requires VS Code 1.85+, Node 18+, and a working `scry` binary.

Install
-------

### From this checkout (developer install)

```sh
cd /path/to/scry/editors/vscode
npm install
npm run compile

# Install into your local VS Code (Linux):
mkdir -p ~/.vscode/extensions/scry-vscode-0.1.0
cp -r package.json out node_modules ~/.vscode/extensions/scry-vscode-0.1.0/
```

Restart VS Code (or run `Developer: Reload Window`).

### Packaged `.vsix` (when published)

```sh
npm install -g @vscode/vsce        # one-time
vsce package                       # produces scry-vscode-0.1.0.vsix
code --install-extension scry-vscode-0.1.0.vsix
```

Building (or pointing at) an index
----------------------------------

```sh
scry index /path/to/repo -o /mnt/agent/scry-index --workers 8
```

Then in your VS Code `settings.json`:

```json
{
  "scry.binary": "scry",
  "scry.indexDir": "/mnt/agent/scry-index"
}
```

Incremental refresh after edits:

```sh
scry index --incremental /path/to/repo -o /mnt/agent/scry-index
```

The extension reuses the same mmap'd index until you run
`scry: Restart daemon` from the command palette.

What you get
------------

| keystroke                     | provider                              | scry call    |
|-------------------------------|---------------------------------------|--------------|
| `Ctrl+Space`                  | `registerCompletionItemProvider`      | `prefix`     |
| `F12`                         | `registerDefinitionProvider`          | `def`        |
| `Shift+F12`                   | `registerReferenceProvider`           | `callers`    |
| `Ctrl+Shift+O`                | `registerDocumentSymbolProvider`      | `outline`    |
| Command palette → `scry: Go to definition` | `scry.def`                | `def`        |
| Command palette → `scry: Find callers`     | `scry.callers`            | `callers`    |
| Command palette → `scry: Outline current file` | `scry.outline`        | `outline`    |
| Command palette → `scry: Show daemon stats`    | `scry.stats`          | `stats`      |
| Command palette → `scry: Restart daemon`       | `scry.restart`        | (respawn)    |

Configuration
-------------

| setting                       | default                              | meaning                                          |
|-------------------------------|--------------------------------------|--------------------------------------------------|
| `scry.binary`                 | `"scry"`                             | binary name or absolute path                     |
| `scry.indexDir`               | `""`                                 | empty = scry's own default                       |
| `scry.socketPath`             | `""`                                 | empty = `/tmp/scry-vscode-${pid}.sock`           |
| `scry.maxCompletions`         | `50`                                 | `prefix` `--limit`                               |
| `scry.minCompletionLength`    | `2`                                  | don't fire for shorter prefixes                  |

Notes
-----

- **Bignum-safe JSON**: scry returns u64 symbol IDs that overflow
  `JSON.parse`'s `Number.MAX_SAFE_INTEGER`. The extension
  pre-rewrites those occurrences to strings before parsing
  (`"id":12345678901234567890` → `"id":"12345678901234567890"`).
  The request/response envelope `id` we assign ourselves so it
  stays a small int.
- **Provider scope**: providers are registered for every language
  scry knows about (rust / go / python / c / cpp / java / kotlin /
  typescript / typescriptreact / shellscript / proto / proto3 /
  html / css / scss / markdown / toml / yaml). Other languages
  silently skip scry; nothing breaks.
- **VS Code already has IntelliSense**: scry providers run
  alongside the built-in language servers. If you have rust-analyzer
  for `.rs` files, both contribute to the completion list and the
  user sees a merged ranked set.

Troubleshooting
---------------

- `scry request timed out`: open the command palette and run
  `scry: Restart daemon`.
- Completion shows nothing: confirm `scry --version` and the
  configured `scry.indexDir` actually has an index. Run
  `scry: Show daemon stats` — it should report a non-zero
  `symbols`.

Headless verification
---------------------

```sh
cd /path/to/scry
./editors/tests/e2e_vscode.sh
```

Drives the compiled `ScryClient` class directly via node, no VS
Code runtime needed. Exits 0 when all 7 assertions pass (stats,
prefix, def, callers, outline, fuzzy, plus a u64-id-precision
check that catches JSON parser regressions).
