// Loaded in <head> without defer, so the chosen theme is set before the page
// paints (no flash). Theme: light / dark / system, remembered per device.
// Also remembers a collapsed sidebar.
(function () {
  var root = document.documentElement;
  var THEME = 'kwartje-theme', SIDEBAR = 'kwartje-sidebar';
  var LABELS = { system: 'Thema: systeem', light: 'Thema: licht', dark: 'Thema: donker' };
  var NEXT = { system: 'light', light: 'dark', dark: 'system' };
  function get(k) { try { return localStorage.getItem(k); } catch (e) { return null; } }
  function set(k, v) { try { localStorage.setItem(k, v); } catch (e) { /* private mode */ } }
  function apply(pref) {
    if (pref === 'light' || pref === 'dark') root.setAttribute('data-theme', pref);
    else { root.removeAttribute('data-theme'); pref = 'system'; }
    root.setAttribute('data-theme-pref', pref);
    document.querySelectorAll('[data-theme-toggle]').forEach(function (b) {
      b.title = LABELS[pref] + ' (klik: ' + LABELS[NEXT[pref]].replace('Thema: ', '') + ')';
      b.setAttribute('aria-label', b.title);
    });
  }
  apply(get(THEME));
  if (get(SIDEBAR) === 'collapsed') root.classList.add('sidebar-collapsed');
  document.addEventListener('DOMContentLoaded', function () { apply(root.getAttribute('data-theme-pref')); });
  document.addEventListener('click', function (e) {
    var t = e.target.closest && e.target.closest('[data-theme-toggle]');
    if (t) {
      var next = NEXT[root.getAttribute('data-theme-pref') || 'system'];
      set(THEME, next);
      apply(next);
      return;
    }
    var c = e.target.closest && e.target.closest('[data-sidebar-toggle]');
    if (c) set(SIDEBAR, root.classList.toggle('sidebar-collapsed') ? 'collapsed' : 'open');
  });
})();
