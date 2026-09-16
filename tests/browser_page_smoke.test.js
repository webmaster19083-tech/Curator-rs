const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const root = path.join(__dirname, '..');
const read = (file) => fs.readFileSync(path.join(root, file), 'utf8');

test('the browser fallback boots Explorer after the legacy media engine', () => {
  const html = read('static/index.html');
  const app = read('static/app.js');
  const library = read('static/library.js');

  assert.match(
    html,
    /<script src="\/app\.js"><\/script>\s*<script src="\/library\.js"><\/script>/,
  );
  assert.match(library, /function installExplorerUi\(\)/);
  assert.match(library, /explorer-top-navigation/);
  assert.match(library, /data-nav="search"/);
  assert.match(library, /data-nav="groups"/);
  assert.match(library, /vrPlayButton\.dataset\.playMode = 'vr'/);
  assert.match(app, /function clipMaxSeconds\(\)/);
  assert.doesNotMatch(app, /CLIP_MAX_SECONDS|THREE/);
});

test('the desktop OOBE loads its API bridge before the wizard code', () => {
  const html = read('static/oobe.html');
  assert.match(
    html,
    /<script src="\/desktop\.js"><\/script>\s*<script src="\/oobe\.js"><\/script>/,
  );
  assert.doesNotThrow(() => new vm.Script(read('static/oobe.js'), { filename: 'static/oobe.js' }));
});

test('the desktop bridge sends OOBE API requests through Tauri', async () => {
  const payload = { settings: { theme: 'system' } };
  let invocation;
  const context = {
    Response,
    Uint8Array,
    MutationObserver: class {
      observe() {}
    },
    document: {
      documentElement: {},
      addEventListener() {},
    },
    navigator: { userAgent: 'Windows' },
  };
  context.window = {
    __CURATOR_RUNTIME__: 'host',
    __TAURI__: {
      core: {
        invoke: async (...args) => {
          invocation = args;
          return {
            status: 200,
            headers: { 'content-type': 'application/json' },
            body: [...Buffer.from(JSON.stringify(payload))],
          };
        },
      },
      event: { listen: async () => {} },
    },
    fetch: async () => { throw new Error('relative API requests must use the desktop bridge'); },
  };
  context.window.window = context.window;

  new vm.Script(read('static/desktop.js'), { filename: 'static/desktop.js' }).runInNewContext(context);
  const response = await context.window.fetch('/api/oobe/status');
  assert.deepEqual(await response.json(), payload);
  assert.equal(invocation[0], 'api_request');
  assert.equal(invocation[1].path, '/api/oobe/status');
  assert.equal(invocation[1].method, 'GET');
  assert.equal(invocation[1].body, null);
});

test('packaged browser assets parse and do not depend on remote UI libraries', () => {
  const html = read('static/index.html');
  const styles = read('static/style.css');
  assert.doesNotMatch(html, /fonts\.googleapis\.com|fonts\.gstatic\.com|threejs|cdnjs|unpkg/i);
  assert.doesNotMatch(styles, /url\(\s*https?:\/\//i);

  for (const file of ['static/desktop.js', 'static/oobe.js', 'static/virtual-clips.js', 'static/app.js', 'static/library.js']) {
    assert.doesNotThrow(() => new vm.Script(read(file), { filename: file }));
  }
});
