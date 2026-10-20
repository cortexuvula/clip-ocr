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
  const saveButton = () => form.querySelector('button[type=submit]');

  // Render ONLY the update region from a settings view: the read-only
  // status line (result of the last ACTUAL check — never a projection of
  // the unsaved checkbox) and the visibility of "Review update…".
  // This must never write to any form control, the save-status element,
  // focus, or selection — it is the refresh path used after a manual
  // check, where a full re-render would wipe the user's unsaved edits.
  function renderUpdateRegion(update, version) {
    const u = update || {};
    const line = document.getElementById('update-status');
    const review = document.getElementById('update-review');
    const last = u.last_checked ? `Last successful check: ${u.last_checked}.` : 'Last successful check: Never.';
    if (u.state === 'checking') {
      line.textContent = 'Checking for updates…';
    } else if (u.state === 'up_to_date') {
      line.textContent = `ClipOCR v${version} is up to date. Last checked: ${u.last_checked || 'Never'}.`;
    } else if (u.state === 'available') {
      line.textContent = `ClipOCR v${u.latest} is available. Installed: v${version}.`;
    } else if (u.state === 'failed') {
      line.textContent = `Could not check for updates: ${u.reason || 'Update service unavailable'}. ${last}`;
    } else {
      // off / not_checked / unknown: no check result to project.
      line.textContent = 'No update check has been made yet.';
    }
    // "Check for updates now" STAYS visible even when an update is
    // available — a control the user may be reaching for must not be
    // displaced by an asynchronously appearing one. [hidden] display:none
    // keeps "Review update…" out of the tab/focus order while hidden.
    review.hidden = u.state !== 'available';
  }

  // Full render — ONLY at load and after a successful save (the drafts
  // have just become the persisted values at that point).
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
    renderUpdateRegion(view.update, view.version);
    status.className = '';
    status.textContent = view.warning || (view.hotkey_active ? 'Shortcut active.' : 'Shortcut INACTIVE — use tray Capture or choose a different shortcut.');
  }
  invoke('load_settings').then(render).catch(() => { status.textContent = 'Settings unavailable.' });

  document.getElementById('update-check').addEventListener('click', () => {
    const line = document.getElementById('update-status');
    invoke('request_update_check').then(() => {
      // Request ACCEPTED is not a result: show pending state here and poll
      // the update region only — the form draft is untouched.
      line.textContent = 'Checking for updates…';
      const refresh = () => invoke('load_settings')
        .then(view => renderUpdateRegion(view.update, view.version))
        .catch(() => {});
      window.setTimeout(refresh, 4000);
      window.setTimeout(refresh, 12000);
    }).catch(() => { line.textContent = 'Check could not be started.'; });
  });
  document.getElementById('update-review').addEventListener('click', () => {
    invoke('open_update_page').catch(() => {});
  });
  form.addEventListener('submit', async event => {
    event.preventDefault(); const button = saveButton(); button.disabled = true;
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
    } catch (error) {
      // Tauri v2 rejects with an object ({ code, message }); surface the
      // real reason only — no internal ids or window URL. The user's
      // edits stay in the form untouched.
      const message = error && (error.message || error.error) ? (error.message || error.error) : '';
      status.className = 'err';
      status.textContent = `${typeof message === 'string' && message ? message : 'Could not save settings.'} Your changes have not been saved.`;
    }
    finally {
      button.disabled = false;
      button.focus();
    }
  });
}
if (section === 'selftest') {
  document.getElementById('test-capture').addEventListener('click', async () => {
    const status = document.getElementById('test-status');
    try { const out = await invoke('selftest_capture'); status.textContent = `${out.status}; ${out.chars} characters. Verify clipboard contains only the selected synthetic text.`; }
    catch (_) { status.textContent = 'Selftest unavailable (debug build only).'; }
  });
}
