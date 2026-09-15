const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const root = path.join(__dirname, '..');
const read = (file) => fs.readFileSync(path.join(root, file), 'utf8');

test('Windows Server installer has explicit user and machine scope plus opt-in P-HAR', () => {
  const nsis = read('packaging/windows/curator-server.nsi');
  const register = read('packaging/windows/Register-CuratorServer.ps1');
  assert.match(nsis, /RequestExecutionLevel admin/);
  assert.match(nsis, /RequestExecutionLevel user/);
  assert.match(nsis, /Set up P-HAR after installation/);
  assert.match(nsis, /-Scope "\$\{SERVER_SCOPE\}"/);
  assert.match(register, /New-ScheduledTaskAction/);
  assert.match(register, /sc\.exe create CuratorServer/);
  assert.match(register, /phar-intent --enabled true/);
  assert.ok(register.indexOf('phar-intent --enabled true') < register.indexOf('& $startServer'));
});

test('Linux and macOS scope packages carry appropriate service definitions', () => {
  const systemd = read('packaging/linux/curator-server.service');
  const userSystemd = read('packaging/linux/curator-server-user.service');
  const portable = read('packaging/linux/install-current-user.sh');
  const postinst = read('packaging/linux/postinst');
  const daemon = read('packaging/macos/tech.webmaster19083.curator.server.plist');
  const agent = read('packaging/macos/tech.webmaster19083.curator.server.user.plist');
  const macPortable = read('packaging/macos/build-server-user-archive.sh');
  const macVerify = read('packaging/macos/verify-app.sh');
  const macPostinstall = read('packaging/macos/postinstall-server.sh');
  assert.match(systemd, /CURATOR_INSTALL_SCOPE=all-users/);
  assert.match(systemd, /User=curator/);
  assert.match(userSystemd, /--install-scope current-user/);
  assert.match(userSystemd, /__CURATOR_SERVER_PATH__/);
  assert.match(portable, /systemctl --user enable --now/);
  assert.match(postinst, /systemctl enable --now curator-server\.service/);
  assert.match(daemon, /<string>all-users<\/string>/);
  assert.match(agent, /<string>current-user<\/string>/);
  assert.match(macPortable, /install-current-user\.sh/);
  assert.match(macVerify, /CFBundleIdentifier/);
  assert.match(macVerify, /lipo -archs/);
  assert.match(macVerify, /Contents\/Resources/);
  assert.match(macPostinstall, /launchctl bootstrap system/);
});

test('release workflow validates once and attaches matrix artifacts from one job', () => {
  const workflow = read('.github/workflows/desktop-release.yml');
  assert.match(workflow, /validate:/);
  assert.match(workflow, /windows-server:/);
  assert.match(workflow, /build-server-installers\.ps1/);
  assert.match(workflow, /build-server-user-archive\.sh/);
  assert.match(workflow, /verify-app\.sh/);
  assert.match(workflow, /curator --docs/);
  assert.match(workflow, /--bundles deb,appimage/);
  assert.match(workflow, /bundle\/appimage\/\*\.AppImage/);
  assert.match(workflow, /path: artifacts\/macos\//);
  assert.match(workflow, /path: artifacts\/windows\//);
  assert.match(workflow, /MACOSX_DEPLOYMENT_TARGET: "11\.0"/);
  assert.match(workflow, /needs: \[windows-host-viewer, windows-server, linux, macos\]/);
  assert.equal(fs.existsSync(path.join(root, '.github/workflows/windows-release.yml')), false);
});
