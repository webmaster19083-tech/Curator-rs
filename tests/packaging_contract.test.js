const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const root = path.join(__dirname, '..');
const read = (file) => fs.readFileSync(path.join(root, file), 'utf8');

test('Host packaging selects the Tauri binary when the native preview is present', () => {
  const manifest = read('desktop/Cargo.toml');
  assert.match(manifest, /^default-run = "Curator"$/m);
  assert.match(manifest, /name = "curator-native-preview"/);
});

test('Windows Server installer has explicit user and machine scope plus opt-in P-HAR', () => {
  const nsis = read('packaging/windows/curator-server.nsi');
  const register = read('packaging/windows/Register-CuratorServer.ps1');
  const build = read('packaging/windows/build-server-installers.ps1');
  assert.match(nsis, /RequestExecutionLevel admin/);
  assert.match(nsis, /RequestExecutionLevel user/);
  assert.match(nsis, /Set up P-HAR after installation/);
  assert.match(nsis, /-Scope "\$\{SERVER_SCOPE\}"/);
  assert.match(register, /New-ScheduledTaskAction/);
  assert.match(register, /sc\.exe create CuratorServer/);
  assert.match(register, /phar-intent --enabled true/);
  assert.ok(register.indexOf('phar-intent --enabled true') < register.indexOf('& $startServer'));
  assert.match(build, /Get-Command -Name \$name -CommandType Application/);
  assert.match(build, /ProgramFilesX86/);
  assert.match(build, /NSIS\\makensis\.exe/);
});

test('Linux and macOS scope packages carry appropriate service definitions', () => {
  const systemd = read('packaging/linux/curator-server.service');
  const userSystemd = read('packaging/linux/curator-server-user.service');
  const portable = read('packaging/linux/install-current-user.sh');
  const postinst = read('packaging/linux/postinst');
  const daemon = read('packaging/macos/tech.webmaster19083.curator.server.plist');
  const agent = read('packaging/macos/tech.webmaster19083.curator.server.user.plist');
  const macPortable = read('packaging/macos/build-server-user-archive.sh');
  const macDmgVerify = read('packaging/macos/verify-dmg.sh');
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
  assert.match(macDmgVerify, /hdiutil verify/);
  assert.match(macDmgVerify, /hdiutil attach/);
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
  assert.match(workflow, /GITHUB_PATH/);
  assert.match(workflow, /build-server-user-archive\.sh/);
  assert.match(workflow, /verify-app\.sh/);
  assert.match(workflow, /verify-dmg\.sh/);
  assert.match(workflow, /APPLE_SIGNING_IDENTITY: "-"/);
  assert.match(workflow, /--bundles app,dmg/);
  assert.match(workflow, /curator --docs/);
  assert.match(workflow, /--bundles deb,appimage/);
  assert.match(workflow, /bundle\/appimage\/\*\.AppImage/);
  assert.match(workflow, /path: artifacts\/macos\//);
  assert.match(workflow, /path: artifacts\/windows\//);
  assert.match(workflow, /MACOSX_DEPLOYMENT_TARGET: "11\.0"/);
  assert.match(workflow, /needs: \[windows-host-viewer, windows-server, linux, macos\]/);
  assert.equal(fs.existsSync(path.join(root, '.github/workflows/windows-release.yml')), false);
});

test('macOS Host and Viewer bundles pin macOS 11 and use ad-hoc signing', () => {
  for (const file of ['desktop/tauri.conf.json', 'viewer/tauri.conf.json']) {
    const config = JSON.parse(read(file));
    assert.equal(config.bundle.macOS.minimumSystemVersion, '11.0');
    assert.equal(config.bundle.macOS.signingIdentity, '-');
  }
});
