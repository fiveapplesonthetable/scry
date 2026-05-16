// scry VS Code extension. Spawns one `scry serve` per window, multiplexes
// every editor-driven request (autocomplete, jump-to-def, find-refs,
// document outline) over a single persistent unix socket.
//
// The protocol is documented in editors/common/PROTOCOL.md. This
// file implements the client end.

import {ChildProcess, spawn} from 'node:child_process';
import {createConnection, Socket} from 'node:net';
import {existsSync, unlinkSync} from 'node:fs';
import {setTimeout as sleep} from 'node:timers/promises';
import * as vscode from 'vscode';

// ---------------------------------------------------------------------------
// JSON-RPC client over a unix socket. Single instance per workspace folder.
// ---------------------------------------------------------------------------

type Pending = {
  resolve: (r: unknown) => void;
  reject: (e: Error) => void;
  cmd: string;
};

class ScryClient {
  private child: ChildProcess | null = null;
  private sock: Socket | null = null;
  private pending = new Map<number, Pending>();
  private nextId = 1;
  private buf = '';
  private starting: Promise<void> | null = null;

  constructor(private cfg: {binary: string; indexDir: string; socketPath: string}) {}

  async ensure(): Promise<void> {
    if (this.sock && !this.sock.destroyed) return;
    if (this.starting) return this.starting;
    this.starting = this.start();
    try { await this.starting; } finally { this.starting = null; }
  }

  private async start(): Promise<void> {
    if (existsSync(this.cfg.socketPath)) {
      try { unlinkSync(this.cfg.socketPath); } catch { /* ignore */ }
    }
    const args = ['serve', '--listen', `unix:${this.cfg.socketPath}`,
                  '--max-conns', '4'];
    if (this.cfg.indexDir) args.push('--index', this.cfg.indexDir);
    this.child = spawn(this.cfg.binary, args, {stdio: ['ignore', 'pipe', 'pipe']});
    this.child.on('exit', (code) => {
      console.warn(`[scry] daemon exited code=${code}`);
      this.teardown(new Error(`daemon exited code=${code}`));
    });

    // Wait for the socket to appear.
    const deadline = Date.now() + 5000;
    while (!existsSync(this.cfg.socketPath)) {
      if (Date.now() > deadline) {
        throw new Error(`scry serve did not bind ${this.cfg.socketPath} within 5 s`);
      }
      await sleep(30);
    }

    await new Promise<void>((resolve, reject) => {
      this.sock = createConnection(this.cfg.socketPath, () => resolve());
      this.sock.setNoDelay(true);
      this.sock.on('error', reject);
      this.sock.on('data', (b) => this.onData(b));
      this.sock.on('close', () => this.teardown(new Error('socket closed')));
    });
  }

  private teardown(err: Error): void {
    for (const [, p] of this.pending) p.reject(err);
    this.pending.clear();
    try { this.sock?.destroy(); } catch { /* */ }
    this.sock = null;
    try { this.child?.kill(); } catch { /* */ }
    this.child = null;
  }

  async restart(): Promise<void> {
    this.teardown(new Error('restart requested'));
    await sleep(150);
    await this.ensure();
  }

  // u64 IDs from scry overflow JS Number.MAX_SAFE_INTEGER (2^53). We
  // hand-walk the JSON for any field literally named "id" *outside*
  // the top-level envelope and convert it to a string before the
  // parser sees it. The envelope's id (request/response correlation)
  // is always assigned by us and stays an int.
  private parseLine(line: string): {id?: number; result?: unknown; error?: string} {
    // Convert: "id":12345678901234567890 → "id":"12345678901234567890"
    // for occurrences that look like bare integer literals. Run once.
    const safe = line.replace(/"id":(\d+)/g, (m, digits) =>
      digits.length > 15 ? `"id":"${digits}"` : m);
    return JSON.parse(safe);
  }

  private onData(b: Buffer): void {
    this.buf += b.toString('utf8');
    while (true) {
      const nl = this.buf.indexOf('\n');
      if (nl < 0) break;
      const line = this.buf.slice(0, nl);
      this.buf = this.buf.slice(nl + 1);
      if (!line.length) continue;
      let obj: ReturnType<typeof this.parseLine>;
      try { obj = this.parseLine(line); }
      catch (e) { console.warn(`[scry] bad JSON: ${e}`); continue; }
      const id = obj.id;
      if (typeof id !== 'number') continue;
      const cell = this.pending.get(id);
      if (!cell) continue;
      this.pending.delete(id);
      if (obj.error) cell.reject(new Error(obj.error));
      else cell.resolve(obj.result);
    }
  }

  async request<T = unknown>(cmd: string, args?: Record<string, unknown>): Promise<T> {
    await this.ensure();
    const id = this.nextId++;
    const line = JSON.stringify(args ? {id, cmd, args} : {id, cmd}) + '\n';
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, {resolve: resolve as (r: unknown) => void, reject, cmd});
      const timer = setTimeout(() => {
        if (this.pending.delete(id)) reject(new Error(`scry request timed out (${cmd})`));
      }, 5000);
      const finish = (orig: typeof resolve | typeof reject, fn: typeof resolve | typeof reject) =>
        (v: unknown) => { clearTimeout(timer); (orig as (x: unknown) => void)(v); };
      // wire through cleanup
      const cell = this.pending.get(id);
      if (cell) {
        cell.resolve = finish(resolve as (x: unknown) => void, resolve as never) as (r: unknown) => void;
        cell.reject  = finish(reject as never, reject as never) as (e: Error) => void;
      }
      this.sock!.write(line, 'utf8', (err) => {
        if (err) { this.pending.delete(id); clearTimeout(timer); reject(err); }
      });
    });
  }
}

// ---------------------------------------------------------------------------
// Result row shape (what scry returns over JSON-RPC)
// ---------------------------------------------------------------------------

interface ScryRow {
  name?: string;
  kind?: string;
  ref_kind?: string;
  lang?: string;
  path?: string;
  line?: number;
  col?: number;
  scope?: string[];
  fqn?: string | null;
  resolved_to?: number | string | null;
}

interface OutlineResult {
  path: string;
  lang: string;
  symbols: ScryRow[];
  symbols_total: number;
  symbols_shown: number;
}

// ---------------------------------------------------------------------------
// LSP-ish providers backed by the client
// ---------------------------------------------------------------------------

function rowToLocation(row: ScryRow): vscode.Location | null {
  if (!row.path || !row.line) return null;
  const uri = vscode.Uri.file(row.path);
  const col = Math.max(0, (row.col ?? 1) - 1);
  const pos = new vscode.Position(row.line - 1, col);
  return new vscode.Location(uri, pos);
}

const KIND_MAP: Record<string, vscode.CompletionItemKind> = {
  class: vscode.CompletionItemKind.Class,
  iface: vscode.CompletionItemKind.Interface,
  struct: vscode.CompletionItemKind.Struct,
  enum:  vscode.CompletionItemKind.Enum,
  fn:    vscode.CompletionItemKind.Function,
  method: vscode.CompletionItemKind.Method,
  field: vscode.CompletionItemKind.Field,
  var:   vscode.CompletionItemKind.Variable,
  const: vscode.CompletionItemKind.Constant,
  module: vscode.CompletionItemKind.Module,
  ns:    vscode.CompletionItemKind.Module,
  ctor:  vscode.CompletionItemKind.Constructor,
};

const SYMBOL_KIND_MAP: Record<string, vscode.SymbolKind> = {
  class: vscode.SymbolKind.Class,
  iface: vscode.SymbolKind.Interface,
  struct: vscode.SymbolKind.Struct,
  enum:  vscode.SymbolKind.Enum,
  fn:    vscode.SymbolKind.Function,
  method: vscode.SymbolKind.Method,
  field: vscode.SymbolKind.Field,
  var:   vscode.SymbolKind.Variable,
  const: vscode.SymbolKind.Constant,
  module: vscode.SymbolKind.Module,
  ns:    vscode.SymbolKind.Namespace,
  ctor:  vscode.SymbolKind.Constructor,
};

function makeCompletionProvider(client: ScryClient, cfg: {min: number; max: number}): vscode.CompletionItemProvider {
  return {
    async provideCompletionItems(doc, position, token) {
      const range = doc.getWordRangeAtPosition(position, /[A-Za-z_][A-Za-z0-9_]*/);
      if (!range) return undefined;
      const prefix = doc.getText(range);
      if (prefix.length < cfg.min) return undefined;
      let rows: ScryRow[] = [];
      try {
        rows = await client.request<ScryRow[]>('prefix', {prefix, limit: cfg.max});
      } catch (e) {
        console.warn(`[scry] prefix failed: ${e}`);
        return undefined;
      }
      if (token.isCancellationRequested) return undefined;
      const seen = new Set<string>();
      const items: vscode.CompletionItem[] = [];
      for (const row of rows) {
        if (!row.name || seen.has(row.name)) continue;
        seen.add(row.name);
        const item = new vscode.CompletionItem(row.name,
          KIND_MAP[row.kind ?? ''] ?? vscode.CompletionItemKind.Text);
        item.detail = `[${row.kind ?? '?'} ${row.lang ?? '?'}] ${row.path ?? ''}`;
        item.range = range;
        items.push(item);
      }
      return items;
    },
  };
}

function makeDefinitionProvider(client: ScryClient): vscode.DefinitionProvider {
  return {
    async provideDefinition(doc, position, token) {
      const range = doc.getWordRangeAtPosition(position, /[A-Za-z_][A-Za-z0-9_]*/);
      if (!range) return undefined;
      const name = doc.getText(range);
      const lang = langForDoc(doc);
      const args: Record<string, unknown> = {name, limit: 25};
      if (lang) args.lang = lang;
      try {
        const rows = await client.request<ScryRow[]>('def', args);
        if (token.isCancellationRequested) return undefined;
        return rows.map(rowToLocation).filter((l): l is vscode.Location => l !== null);
      } catch (e) {
        console.warn(`[scry] def failed: ${e}`);
        return undefined;
      }
    },
  };
}

function makeReferenceProvider(client: ScryClient): vscode.ReferenceProvider {
  return {
    async provideReferences(doc, position, _ctx, token) {
      const range = doc.getWordRangeAtPosition(position, /[A-Za-z_][A-Za-z0-9_]*/);
      if (!range) return undefined;
      const name = doc.getText(range);
      try {
        const rows = await client.request<ScryRow[]>('callers', {name, limit: 200});
        if (token.isCancellationRequested) return undefined;
        return rows.map(rowToLocation).filter((l): l is vscode.Location => l !== null);
      } catch (e) {
        console.warn(`[scry] callers failed: ${e}`);
        return undefined;
      }
    },
  };
}

function makeDocumentSymbolProvider(client: ScryClient): vscode.DocumentSymbolProvider {
  return {
    async provideDocumentSymbols(doc, token) {
      try {
        const r = await client.request<OutlineResult>('outline', {path: doc.fileName, limit: 1000});
        if (token.isCancellationRequested) return undefined;
        return r.symbols.map((row) => {
          const line = (row.line ?? 1) - 1;
          const col = Math.max(0, (row.col ?? 1) - 1);
          const range = new vscode.Range(line, col, line, col + (row.name?.length ?? 0));
          return new vscode.DocumentSymbol(
            row.name ?? '?',
            row.kind ?? '',
            SYMBOL_KIND_MAP[row.kind ?? ''] ?? vscode.SymbolKind.Object,
            range, range,
          );
        });
      } catch (e) {
        console.warn(`[scry] outline failed: ${e}`);
        return undefined;
      }
    },
  };
}

function langForDoc(doc: vscode.TextDocument): string | undefined {
  const ext = doc.fileName.split('.').pop()?.toLowerCase();
  switch (ext) {
    case 'rs': return 'Rust';
    case 'go': return 'Go';
    case 'py': return 'Python';
    case 'c': return 'C';
    case 'cc': case 'cpp': case 'cxx': return 'Cpp';
    case 'h': case 'hh': case 'hpp': case 'hxx': return 'Header';
    case 'java': return 'Java';
    case 'kt': case 'kts': return 'Kotlin';
    case 'ts': case 'tsx': return 'TypeScript';
    case 'proto': return 'Proto';
    case 'sh': case 'bash': return 'Bash';
    case 'html': case 'htm': return 'Html';
    case 'css': return 'Css';
    case 'scss': return 'Scss';
    case 'md': return 'Markdown';
    case 'toml': return 'Toml';
    case 'yaml': case 'yml': return 'Yaml';
    default: return undefined;
  }
}

// ---------------------------------------------------------------------------
// Activation
// ---------------------------------------------------------------------------

export function activate(ctx: vscode.ExtensionContext): {client: ScryClient} {
  const cfg = vscode.workspace.getConfiguration('scry');
  const socketPath = cfg.get<string>('socketPath') ||
    `/tmp/scry-vscode-${process.pid}.sock`;
  const client = new ScryClient({
    binary:   cfg.get<string>('binary') || 'scry',
    indexDir: cfg.get<string>('indexDir') || '',
    socketPath,
  });

  const completionCfg = {
    min: cfg.get<number>('minCompletionLength') ?? 2,
    max: cfg.get<number>('maxCompletions') ?? 50,
  };

  // Register providers across every language scry knows about.
  const SCHEMES = [
    {scheme: 'file', language: 'rust'},
    {scheme: 'file', language: 'go'},
    {scheme: 'file', language: 'python'},
    {scheme: 'file', language: 'c'},
    {scheme: 'file', language: 'cpp'},
    {scheme: 'file', language: 'java'},
    {scheme: 'file', language: 'kotlin'},
    {scheme: 'file', language: 'typescript'},
    {scheme: 'file', language: 'typescriptreact'},
    {scheme: 'file', language: 'shellscript'},
    {scheme: 'file', language: 'proto3'},
    {scheme: 'file', language: 'proto'},
    {scheme: 'file', language: 'html'},
    {scheme: 'file', language: 'css'},
    {scheme: 'file', language: 'scss'},
    {scheme: 'file', language: 'markdown'},
    {scheme: 'file', language: 'toml'},
    {scheme: 'file', language: 'yaml'},
  ];

  ctx.subscriptions.push(
    vscode.languages.registerCompletionItemProvider(SCHEMES,
      makeCompletionProvider(client, completionCfg)),
    vscode.languages.registerDefinitionProvider(SCHEMES,
      makeDefinitionProvider(client)),
    vscode.languages.registerReferenceProvider(SCHEMES,
      makeReferenceProvider(client)),
    vscode.languages.registerDocumentSymbolProvider(SCHEMES,
      makeDocumentSymbolProvider(client)),

    vscode.commands.registerCommand('scry.def', async () => {
      const ed = vscode.window.activeTextEditor;
      if (!ed) return;
      const range = ed.document.getWordRangeAtPosition(ed.selection.active);
      if (!range) return;
      const name = ed.document.getText(range);
      const rows = await client.request<ScryRow[]>('def', {name, limit: 25});
      const locs = rows.map(rowToLocation).filter((l): l is vscode.Location => l !== null);
      if (locs.length === 1) {
        await vscode.window.showTextDocument(locs[0].uri, {selection: locs[0].range});
      } else if (locs.length > 1) {
        await vscode.commands.executeCommand('editor.action.showReferences',
          ed.document.uri, ed.selection.active, locs);
      } else {
        vscode.window.showInformationMessage(`[scry] no definitions for ${name}`);
      }
    }),

    vscode.commands.registerCommand('scry.callers', async () => {
      const ed = vscode.window.activeTextEditor;
      if (!ed) return;
      const range = ed.document.getWordRangeAtPosition(ed.selection.active);
      if (!range) return;
      const name = ed.document.getText(range);
      const rows = await client.request<ScryRow[]>('callers', {name, limit: 200});
      const locs = rows.map(rowToLocation).filter((l): l is vscode.Location => l !== null);
      if (locs.length) {
        await vscode.commands.executeCommand('editor.action.showReferences',
          ed.document.uri, ed.selection.active, locs);
      } else {
        vscode.window.showInformationMessage(`[scry] no callers for ${name}`);
      }
    }),

    vscode.commands.registerCommand('scry.outline', async () => {
      await vscode.commands.executeCommand('workbench.action.gotoSymbol');
    }),

    vscode.commands.registerCommand('scry.stats', async () => {
      try {
        const s = await client.request<Record<string, unknown>>('stats');
        vscode.window.showInformationMessage(
          `[scry] ${s.scry_version} · ${s.files_total} files · ${s.symbols} syms · ${s.refs} refs`);
      } catch (e) {
        vscode.window.showErrorMessage(`[scry] stats failed: ${e}`);
      }
    }),

    vscode.commands.registerCommand('scry.restart', async () => {
      await client.restart();
      vscode.window.showInformationMessage('[scry] daemon restarted');
    }),
  );

  return {client};
}

export function deactivate(): void {
  // ScryClient.teardown runs on process exit via the child's `exit`
  // handler in any case; nothing to do here that matters.
}

// Exported for the e2e harness (node can `require()` the compiled
// out/extension.js and use the client directly without the VS Code
// runtime).
export {ScryClient};
