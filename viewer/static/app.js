'use strict';

const invoke = window.__TAURI__.core.invoke;
const el = selector => document.querySelector(selector);
let store = { hosts: [], last_host_id: null };

function setStatus(message, error = false) {
  const status = el('#status');
  status.textContent = message;
  status.classList.toggle('error', error);
}

function escapeHtml(value) {
  const node = document.createElement('span');
  node.textContent = value;
  return node.innerHTML;
}

async function refresh() {
  store = await invoke('list_hosts');
  el('#last-host').textContent = store.last_host_id ? 'Last connection is remembered' : '';
  const root = el('#hosts');
  if (!store.hosts.length) {
    root.innerHTML = '<p class="empty">No Tailnet hosts saved yet.</p>';
    return;
  }
  root.innerHTML = store.hosts.map(host => `
    <article class="host ${host.id === store.last_host_id ? 'last' : ''}">
      <div><h3>${escapeHtml(host.name)}</h3><p>${escapeHtml(host.endpoint)} · ${escapeHtml(host.edition)}</p></div>
      <div class="actions"><button data-connect="${escapeHtml(host.id)}">Connect</button><button class="secondary" data-delete="${escapeHtml(host.id)}">Remove</button></div>
    </article>`).join('');
  root.querySelectorAll('[data-connect]').forEach(button => button.addEventListener('click', () => connect(button.dataset.connect)));
  root.querySelectorAll('[data-delete]').forEach(button => button.addEventListener('click', () => remove(button.dataset.delete)));
}

async function test() {
  const endpoint = el('#host-endpoint').value;
  setStatus('Testing Tailnet identity and Curator protocol…');
  try {
    const probe = await invoke('test_host', { endpoint });
    setStatus(`Connected to ${probe.edition} library ${probe.instance_id.slice(0, 8)}.`);
  } catch (error) {
    setStatus(String(error), true);
  }
}

async function save() {
  const name = el('#host-name').value;
  const endpoint = el('#host-endpoint').value;
  setStatus('Validating and saving host…');
  try {
    await invoke('save_host', { name, endpoint });
    el('#host-name').value = '';
    el('#host-endpoint').value = '';
    setStatus('Host saved.');
    await refresh();
  } catch (error) {
    setStatus(String(error), true);
  }
}

async function connect(id) {
  setStatus('Connecting…');
  try { await invoke('connect_host', { id }); } catch (error) { setStatus(String(error), true); }
}

async function remove(id) {
  try { await invoke('delete_host', { id }); await refresh(); } catch (error) { setStatus(String(error), true); }
}

el('#test').addEventListener('click', test);
el('#save').addEventListener('click', save);
refresh().catch(error => setStatus(String(error), true));
