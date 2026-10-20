'use strict';
const invoke = (cmd, args) => window.__TAURI__.core.invoke(cmd, args);
window.addEventListener('error', event => {
  const s = document.getElementById('status');
  if (s) s.textContent = `JS error: ${event.message}`;
});
const section = location.hash.slice(1);
const allowed = ['settings', 'progress', 'selftest'];
if (allowed.includes(section)) document.getElementById(section).hidden = false;
if (section === 'progress') document.body.classList.add('pill');
if (section === 'settings') {
  const status = document.getElementById('status');
  const form = document.getElementById('form');
  function render(view) {
    document.getElementById('version').textContent = view.version;
    document.getElementById('hotkey').value = view.config.hotkey;
    document.getElementById('hotkey').placeholder = view.hotkey_display;
    document.getElementById('hotkey-label').textContent = `Global shortcut (${view.hotkey_display})`;
    document.getElementById('language').replaceChildren(...view.languages.map(language => {
      const option = document.createElement('option'); option.value = language; option.textContent = language; return option;
    }));
    document.getElementById('language').value = view.config.language;
    document.getElementById('engine').value = view.config.engine;
    document.getElementById('collapse').checked = view.config.collapse_line_breaks;
    document.getElementById('login').checked = view.config.launch_at_login;
    document.getElementById('updates-auto').checked = view.config.update_checks;
    const u = view.update || {};
    const summary = document.getElementById('update-summary');
    const review = document.getElementById('update-review');
    const check = document.getElementById('update-check');
    const last = u.last_checked ? `Last successful check: ${u.last_checked}.` : '';
    if (u.state === 'off') {
      summary.textContent = 'Automatic update checks are off. No update request is made unless you choose Check for updates.';
    } else if (u.state === 'checking') {
      summary.textContent = 'Checking…';
    } else if (u.state === 'up_to_date') {
      summary.textContent = `ClipOCR v${view.version} is up to date. Last checked: ${u.last_checked || 'Never'}.`;
    } else if (u.state === 'available') {
      summary.textContent = `ClipOCR v${u.latest} is available. Installed: v${view.version}.`;
    } else if (u.state === 'failed') {
      summary.textContent = `Could not check for updates: ${u.reason || 'Update service unavailable'}. ${last || 'Last successful check: Never.'}`;
    } else {
      summary.textContent = 'No update request has been made yet.';
    }
    const available = u.state === 'available';
    review.hidden = !available;
    check.hidden = available;
    status.className = '';
    status.textContent = view.warning || (view.hotkey_active ? 'Shortcut active.' : 'Shortcut INACTIVE — use tray Capture or choose a different shortcut.');
  }
  invoke('load_settings').then(render).catch(() => { status.textContent = 'Settings unavailable.'; });
  document.getElementById('update-check').addEventListener('click', () => {
    invoke('request_update_check').then(() => {
      status.className = '';
      status.textContent = 'Check requested — the tray menu shows the result.';
      window.setTimeout(() => invoke('load_settings').then(render).catch(() => {}), 4000);
    }).catch(() => { status.className = 'err'; status.textContent = 'Could not request a check.'; });
  });
  document.getElementById('update-review').addEventListener('click', () => {
    invoke('open_update_page').catch(() => {});
  });
  form.addEventListener('submit', async event => {
    event.preventDefault(); const button = form.querySelector('button'); button.disabled = true;
    status.textContent = 'Saving…';
    try {
      const view = await invoke('save_settings', { config: {
        hotkey: document.getElementById('hotkey').value,
        engine: document.getElementById('engine').value,
        language: document.getElementById('language').value,
        collapse_line_breaks: document.getElementById('collapse').checked,
        launch_at_login: document.getElementById('login').checked,
        update_checks: document.getElementById('updates-auto').checked
      }});
      render(view);
      status.className = 'ok';
      status.textContent = `✓ Settings saved.${view.warning ? ' ' + view.warning : ''}`;
      status.scrollIntoView({ block: 'nearest' });
    } catch (error) {
      // Tauri v2 rejects with an object ({ code, message }); surface the real
      // reason instead of the generic fallback so save failures are diagnosable.
      const message = error && (error.message || error.error) ? (error.message || error.error) : String(error);
      const ids = [...document.querySelectorAll('[id]')].map(e => e.id).join(',');
      status.className = 'err';
      status.textContent = `${typeof message === 'string' && message ? message : 'Could not save settings.'} [url=${location.href} ids=${ids}]`;
    }
    finally { button.disabled = false; }
  });
}
if (section === 'selftest') {
  document.getElementById('test-capture').addEventListener('click', async () => {
    const status = document.getElementById('test-status');
    try { const out = await invoke('selftest_capture'); status.textContent = `${out.status}; ${out.chars} characters. Verify clipboard contains only the selected synthetic text.`; }
    catch (_) { status.textContent = 'Selftest unavailable (debug build only).'; }
  });
}
