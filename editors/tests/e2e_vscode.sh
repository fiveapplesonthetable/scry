#!/usr/bin/env bash
# Headless e2e for editors/vscode/. We can't drive the VS Code
# runtime in a CI box (no Code binary, no display), so we exercise
# the client class directly via node: spawn the daemon, send each
# command, verify the parsed result shape. The provider glue is
# trivial wrapping around this client — once the client round-trips
# every command, the providers are correct by construction.

set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"

INDEX="${INDEX:-/mnt/agent/tmp/scry-self-idx}"
SCRY="${SCRY:-$root/target/release/scry}"

if [ ! -d "$INDEX" ]; then
    rm -rf "$INDEX"
    "$SCRY" index "$root" -o "$INDEX" --workers 4 > /dev/null
fi

# Make sure the extension is compiled.
if [ ! -f "$root/editors/vscode/out/extension.js" ]; then
    echo "[e2e_vscode] compiling extension..."
    (cd "$root/editors/vscode" && npx tsc -p . > /dev/null)
fi

echo "[e2e_vscode] using INDEX=$INDEX SCRY=$SCRY"

node --input-type=commonjs - <<EOF
const path = require('path');
const fs = require('fs');

// Stub vscode so the require('vscode') in extension.ts (compiled to
// extension.js) doesn't blow up.  We only consume ScryClient — the
// vscode-dependent providers stay dormant unless activate() runs.
const Module = require('module');
const origResolve = Module._resolveFilename;
Module._resolveFilename = function (req, parent, ...rest) {
  if (req === 'vscode') return path.join(__dirname, 'vscode-stub.js');
  return origResolve.call(this, req, parent, ...rest);
};
require.cache[path.join(__dirname, 'vscode-stub.js')] = {
  exports: new Proxy({}, {get: () => () => {}}),
  loaded: true,
  id: 'vscode-stub',
  filename: 'vscode-stub.js',
  paths: [],
};

const {ScryClient} = require('$root/editors/vscode/out/extension.js');

const SOCKET = '/tmp/scry-e2e-vscode-' + process.pid + '.sock';
const client = new ScryClient({
  binary: '$SCRY',
  indexDir: '$INDEX',
  socketPath: SOCKET,
});

const fails = [];
async function ck(label, fn, pred) {
  process.stdout.write('  ' + label + ' ... ');
  try {
    const r = await fn();
    if (pred(r)) {
      console.log('ok');
    } else {
      fails.push(label + ': got ' + JSON.stringify(r).slice(0, 200));
      console.log('FAIL');
    }
  } catch (e) {
    fails.push(label + ': ' + e.message);
    console.log('ERR (' + e.message + ')');
  }
}

(async () => {
  try {
    await ck('stats',
      () => client.request('stats'),
      (r) => r && r.symbols > 0);

    await ck('prefix returns rows',
      () => client.request('prefix', {prefix: 'restore', limit: 5}),
      (r) => Array.isArray(r) && r.length > 0
              && r.some((row) => /^restore/.test(row.name)));

    await ck('def lands on path:line',
      () => client.request('def', {name: 'compute_id', limit: 3}),
      (r) => Array.isArray(r) && r.length > 0
              && r[0].path && r[0].line > 0);

    await ck('callers returns RefRecords',
      () => client.request('callers', {name: 'compute_id', limit: 5}),
      (r) => Array.isArray(r) && r.length > 0
              && r[0].ref_kind);

    await ck('outline lib.rs > 10 syms',
      () => client.request('outline', {
        path: '$root/crates/scry-store/src/lib.rs', limit: 200,
      }),
      (r) => r && Array.isArray(r.symbols) && r.symbols.length > 10);

    await ck('fuzzy finds sigpipe',
      () => client.request('fuzzy', {substr: 'sigpipe', limit: 3}),
      (r) => Array.isArray(r) && r.length > 0);

    await ck('u64 IDs survive JSON parse without losing precision',
      () => client.request('def', {name: 'compute_id', limit: 1}),
      (r) => Array.isArray(r) && r.length > 0
              && typeof r[0].id === 'string'   // hand-rewritten to string
              && r[0].id.length > 15);
  } finally {
    // Shut down the daemon before exit so the harness doesn't leak.
    try { await client.restart(); } catch { /* */ }
    // No public dispose API; killing the process from outside is
    // fine since we forwarded stoponexit semantics via spawn.
  }

  if (fails.length) {
    console.log('');
    console.log('=== FAILURES ===');
    for (const f of fails) console.log('  ' + f);
    process.exit(1);
  } else {
    console.log('');
    console.log('ALL OK');
    process.exit(0);
  }
})();
EOF
