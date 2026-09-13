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

test('packaged browser assets parse and do not depend on remote UI libraries', () => {
  const html = read('static/index.html');
  const styles = read('static/style.css');
  assert.doesNotMatch(html, /fonts\.googleapis\.com|fonts\.gstatic\.com|threejs|cdnjs|unpkg/i);
  assert.doesNotMatch(styles, /url\(\s*https?:\/\//i);

  for (const file of ['static/desktop.js', 'static/virtual-clips.js', 'static/app.js', 'static/library.js']) {
    assert.doesNotThrow(() => new vm.Script(read(file), { filename: file }));
  }
});
