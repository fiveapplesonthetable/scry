scry editor bindings
====================

Single-binary, sub-10 ms autocomplete + jump-to-def + find-references
for any tree scry indexes. Each editor below talks to one long-lived
`scry serve` subprocess over a unix socket; the same JSON-RPC the
scry web UI uses.

| editor   | install                                 | autocomplete | jump-to-def | find-refs | outline | tested |
|----------|-----------------------------------------|:------------:|:-----------:|:---------:|:-------:|:------:|
| Emacs    | [emacs/README.md](emacs/README.md)      | ✓ CAPF       | ✓ xref      | ✓ xref    | ✓       | 8/8    |
| Vim      | [vim/README.md](vim/README.md)          | ✓ omnifunc   | ✓ :ScryDef  | ✓         | ✓       | 8/8    |
| VS Code  | [vscode/README.md](vscode/README.md)    | ✓ LSP-style  | ✓ F12       | ✓ Sh+F12  | ✓       | 7/7    |

The protocol every plugin implements is in
[common/PROTOCOL.md](common/PROTOCOL.md). It is the entire surface
plugins use; nothing in scry's CLI shape leaks through, so a UI for
a different editor is a one-day port.

Linux is the supported platform. macOS should work end-to-end
(same APIs); Windows lacks the unix-socket transport scry serve
uses, so plugins would need a TCP fallback (not yet implemented;
it would just be a `--listen tcp:...` switch in each plugin's
spawn args).

How to verify your install
--------------------------

```
cd /path/to/scry
./editors/tests/run_all.sh
```

Builds the scry binary if missing, builds a small index of the
scry repo itself, runs each editor's headless e2e suite. Exits 0
when all three suites are green. The suites cover the full
request set every plugin uses (stats, prefix, def, callers,
outline, fuzzy, plus per-editor integration points like CAPF /
omnifunc / ScryClient).
