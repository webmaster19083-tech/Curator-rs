const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const root = path.join(__dirname, '..');
const read = (file) => fs.readFileSync(path.join(root, file), 'utf8');

test('the Explorer shell keeps one bounded primary panel and an independent sidebar scroller', () => {
  const css = read('static/style.css');
  assert.match(css, /\.app-shell\s*\{[\s\S]*?height:\s*100dvh;[\s\S]*?min-height:\s*0;/);
  assert.match(css, /\.explorer-source-tree\s*\{[\s\S]*?min-height:\s*0;[\s\S]*?overflow-y:\s*auto;/);
  assert.match(css, /\.explorer-panel\s*\{[\s\S]*?min-height:\s*0;[\s\S]*?overflow-y:\s*auto;/);
  assert.match(css, /\.explorer-top-navigation\s*\{[\s\S]*?overflow-x:\s*auto;/);
});

test('modal and first-run bodies remain reachable on short dynamic-height screens', () => {
  const appCss = read('static/style.css');
  const oobeCss = read('static/oobe.css');
  assert.match(appCss, /#settings-modal \.modal\s*\{[\s\S]*?max-height:\s*calc\(100dvh - 40px\);[\s\S]*?overflow-y:\s*auto;/);
  assert.match(oobeCss, /\.oobe-shell\s*\{[\s\S]*?height:\s*100dvh;[\s\S]*?overflow:\s*hidden;/);
  assert.match(oobeCss, /\.oobe-card\s*\{[\s\S]*?max-height:\s*calc\(100dvh - 48px\);[\s\S]*?overflow-y:\s*auto;/);
  assert.match(oobeCss, /\.oobe-nav\s*\{[\s\S]*?position:\s*sticky;[\s\S]*?bottom:\s*0;/);
});

test('GTK mapping and remote runtime boundaries remain explicit', () => {
  const css = read('static/style.css');
  const desktop = read('static/desktop.js');
  const oobe = read('static/oobe.html');
  const app = read('static/app.js');
  for (const name of ['adwaita', 'yaru', 'arc', 'breeze']) {
    assert.match(css, new RegExp(`\\[data-theme\\^="${name}-"\\]`));
  }
  assert.match(desktop, /window\.__CURATOR_RUNTIME__/);
  assert.match(desktop, /curatorRuntime === 'host'/);
  assert.match(oobe, /id="phar-setup-requested-input"/);
  assert.match(app, /local_integration_settings_local_only/);
  assert.match(app, /host_integration_settings_available/);
});
