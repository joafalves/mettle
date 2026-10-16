const assert = require('node:assert/strict');
const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const test = require('node:test');

const root = path.join(__dirname, '..');
const grammar = JSON.parse(fs.readFileSync(path.join(root, 'syntaxes', 'mettle.tmLanguage.json'), 'utf8'));
const snippets = JSON.parse(fs.readFileSync(path.join(root, 'snippets', 'mettle.json'), 'utf8'));
const declarations = grammar.repository.declarations.patterns;

test('framing declarations receive the same keyword color scope as codecs', () => {
  const framing = declarations.find(pattern => pattern.match?.includes('(framing)'));
  const codec = declarations.find(pattern => pattern.match?.startsWith('\\b(codec)'));
  assert.ok(framing, 'framing declaration rule is missing');
  const expression = new RegExp(framing.match);
  assert.equal(expression.exec('framing jsonLines = delimited("\\n")')[3], 'jsonLines');
  assert.equal(expression.exec('framing records {')[3], 'records');
  assert.equal(framing.captures['1'].name, codec.captures['1'].name);
  assert.equal(framing.captures['3'].name, 'entity.name.type.framing.mettle');
});

test('framing stays an ordinary name outside a declaration', () => {
  const framing = new RegExp(declarations.find(pattern => pattern.match?.includes('(framing)')).match);
  for (const source of ['    framing = { layout: 1 }', '    framing.layout', 'flow main = framing']) {
    assert.equal(framing.exec(source), null, source);
  }
  assert.ok(!declarations.some(pattern => pattern.match?.includes('(layout)')), 'layout is no longer a keyword');
});

test('built-in framings are highlighted only as calls', () => {
  const builtin = new RegExp(declarations.find(pattern => pattern.match?.includes('lengthPrefixed')).match);
  assert.equal(builtin.exec('framing p = lengthPrefixed(u32be)')[1], 'lengthPrefixed');
  assert.equal(builtin.exec('fixed = 1'), null);
});

test('inline codec use keeps directive and declaration highlighting', () => {
  const combined = declarations.find(pattern => pattern.match?.startsWith('\\b(use)'));
  const source = 'use codec note {';
  assert.equal(new RegExp(combined.match).exec(source)[0], 'use codec note');
  assert.equal(combined.captures['1'].name, 'keyword.control.import.mettle');
  assert.equal(combined.captures['3'].name, 'storage.type.mettle');
  assert.equal(combined.captures['5'].name, 'entity.name.type.codec.mettle');
  assert.match(snippets['Complete-value codec'].body[0], /^use codec /);
});

// Expand a snippet with its default placeholder values, as VS Code inserts it.
function expand(body) {
  return body.join('\n')
    .replace(/\$\{\d+\|([^,|]*)[^|]*\|\}/g, '$1')
    .replace(/\$\{\d+:([^}]*)\}/g, '$1')
    .replace(/\$\d+/g, '')
    .replace(/\\([$}\\])/g, '$1');
}

test('declaration snippets compile with the Mettle CLI', t => {
  const binary = path.join(root, '..', '..', '..', 'target', 'debug', process.platform === 'win32' ? 'mettle.exe' : 'mettle');
  if (!fs.existsSync(binary)) {
    t.skip('build the CLI first with cargo build -p mettle-cli');
    return;
  }
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'mettle-snippets-'));
  try {
    for (const name of ['Complete-value codec', 'Delimited framing', 'Length-prefixed framing', 'Custom framing']) {
      const file = path.join(directory, 'snippet.mettle');
      fs.writeFileSync(file, `${expand(snippets[name].body)}\nflow main = 1\n`);
      const result = spawnSync(binary, ['check', file], { encoding: 'utf8' });
      assert.equal(result.status, 0, `${name}\n${fs.readFileSync(file, 'utf8')}\n${result.stdout}${result.stderr}`);
    }
  } finally {
    fs.rmSync(directory, { recursive: true, force: true });
  }
});
