'use strict';
const invoke = (cmd, args) => window.__TAURI__.core.invoke(cmd, args);
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
    status.textContent = view.warning || (view.hotkey_active ? 'Shortcut active.' : 'Shortcut INACTIVE — use tray Capture or choose a different shortcut.');
  }
  invoke('load_settings').then(render).catch(() => { status.textContent = 'Settings unavailable.'; });
  form.addEventListener('submit', async event => {
    event.preventDefault(); const button = form.querySelector('button'); button.disabled = true;
    try {
      const view = await invoke('save_settings', { config: {
        hotkey: document.getElementById('hotkey').value,
        engine: document.getElementById('engine').value,
        language: document.getElementById('language').value,
        collapse_line_breaks: document.getElementById('collapse').checked,
        launch_at_login: document.getElementById('login').checked
      }});
      render(view);
    } catch (error) {
      // Tauri v2 rejects with an object ({ code, message }); surface the real
      // reason instead of the generic fallback so save failures are diagnosable.
      const message = error && (error.message || error.error) ? (error.message || error.error) : String(error);
      status.textContent = typeof message === 'string' && message ? message : 'Could not save settings.';
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
