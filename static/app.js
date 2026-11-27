// kwartje: tiny progressive enhancements. No inline scripts (CSP).
document.documentElement.classList.add('js');

// Company switcher: submit on change.
document.addEventListener('change', function (e) {
  if (e.target.matches('.company-switcher select')) {
    e.target.form.submit();
  }
});

// Confirm before destructive-ish actions: <form data-confirm="...">.
document.addEventListener('submit', function (e) {
  var msg = e.target.getAttribute('data-confirm');
  if (msg && !window.confirm(msg)) {
    e.preventDefault();
  }
});

// Phone layout: tables become cards. Copy each column header into the cells
// as data-label (CSS shows it). Opt out with <table class="no-cards">.
function labelTables(root) {
  (root || document).querySelectorAll('main table:not(.no-cards):not(.cards)').forEach(function (t) {
    var heads = [];
    t.querySelectorAll('thead tr:last-child > *').forEach(function (th) {
      var span = parseInt(th.getAttribute('colspan') || '1', 10);
      for (var i = 0; i < span; i++) heads.push(th.textContent.trim());
    });
    t.querySelectorAll('tbody > tr').forEach(function (tr) {
      var col = 0;
      Array.prototype.forEach.call(tr.children, function (td) {
        if (heads[col] && !td.hasAttribute('data-label')) td.setAttribute('data-label', heads[col]);
        col += parseInt(td.getAttribute('colspan') || '1', 10);
      });
    });
    t.classList.add('cards');
  });
}
document.addEventListener('DOMContentLoaded', function () { labelTables(); });
document.addEventListener('htmx:afterSettle', function (e) { labelTables(e.target); });

// Close an open menu when clicking elsewhere or pressing Escape.
document.addEventListener('click', function (e) {
  document.querySelectorAll('details.menu[open]').forEach(function (d) {
    if (!d.contains(e.target)) d.removeAttribute('open');
  });
});
document.addEventListener('keydown', function (e) {
  if (e.key === 'Escape') {
    document.querySelectorAll('details.menu[open]').forEach(function (d) { d.removeAttribute('open'); });
  }
});

// Hours: the same entry on several days. A calendar (year → months with a
// count → days) writes the chosen days as jjjj-mm-dd into the hidden
// `dates` field; the server makes one entry per day. Without JavaScript the
// single date field keeps working.
(function () {
  var MONTHS = ['jan', 'feb', 'mrt', 'apr', 'mei', 'jun', 'jul', 'aug', 'sep', 'okt', 'nov', 'dec'];
  var MONTHS_LONG = ['januari', 'februari', 'maart', 'april', 'mei', 'juni', 'juli', 'augustus',
    'september', 'oktober', 'november', 'december'];
  var DAYS = ['Ma', 'Di', 'Wo', 'Do', 'Vr', 'Za', 'Zo'];
  function pad(n) { return (n < 10 ? '0' : '') + n; }
  function iso(y, m, d) { return y + '-' + pad(m + 1) + '-' + pad(d); }
  function parse(s) {
    var m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(s || '');
    return m ? { y: +m[1], m: +m[2] - 1, d: +m[3] } : null;
  }
  function daysIn(y, m) { return new Date(Date.UTC(y, m + 1, 0)).getUTCDate(); }
  function weekday(y, m, d) { return (new Date(Date.UTC(y, m, d)).getUTCDay() + 6) % 7; } // Ma = 0

  function init(box) {
    var form = box.closest('form');
    var hidden = box.querySelector('input[name="dates"]');
    var panel = box.querySelector('.multiday-panel');
    var toggle = box.querySelector('.multiday-toggle');
    var summary = box.querySelector('.multiday-summary');
    var dateInput = form.querySelector('input[name="date"]');
    var chosen = {};
    (hidden.value || '').split(',').forEach(function (s) { if (parse(s.trim())) chosen[s.trim()] = true; });
    var now = new Date();
    var todayIso = iso(now.getFullYear(), now.getMonth(), now.getDate());
    var start = parse(Object.keys(chosen).sort()[0]) || parse(dateInput && dateInput.value) || parse(todayIso);
    var view = { y: start.y, m: start.m };

    function list() { return Object.keys(chosen).sort(); }
    function sync() {
      var l = list();
      hidden.value = l.join(',');
      if (dateInput) {
        if (l.length) { dateInput.value = l[0]; dateInput.readOnly = true; dateInput.classList.add('multiday-locked'); }
        else { dateInput.readOnly = false; dateInput.classList.remove('multiday-locked'); }
      }
      if (!l.length) { summary.textContent = ''; return; }
      var parts = l.slice(0, 6).map(function (s) { var p = parse(s); return p.d + ' ' + MONTHS[p.m]; });
      summary.textContent = l.length + (l.length === 1 ? ' dag' : ' dagen') + ' gekozen: ' +
        parts.join(', ') + (l.length > 6 ? ', …' : '');
    }
    function countIn(y, m) {
      var prefix = y + '-' + pad(m + 1) + '-';
      return list().filter(function (s) { return s.indexOf(prefix) === 0; }).length;
    }
    function render() {
      var h = '<div class="md-cal"><div class="md-months">';
      h += '<div class="md-year"><button type="button" class="secondary outline" data-md="prev-year" aria-label="Vorig jaar">‹</button>' +
        '<strong>' + view.y + '</strong><button type="button" class="secondary outline" data-md="next-year" aria-label="Volgend jaar">›</button></div>';
      h += '<div class="md-month-grid">';
      for (var m = 0; m < 12; m++) {
        var c = countIn(view.y, m);
        h += '<button type="button" data-md="month" data-m="' + m + '" class="md-month' + (m === view.m ? ' is-current' : '') +
          '" aria-label="' + MONTHS_LONG[m] + ' ' + view.y + (c ? ', ' + c + ' gekozen' : '') + '">' + MONTHS[m] +
          (c ? '<span class="md-badge">' + c + '</span>' : '') + '</button>';
      }
      h += '</div></div><div class="md-days"><div class="md-head">';
      DAYS.forEach(function (d) { h += '<span>' + d + '</span>'; });
      h += '</div><div class="md-grid">';
      var lead = weekday(view.y, view.m, 1);
      var prevM = view.m === 0 ? 11 : view.m - 1, prevY = view.m === 0 ? view.y - 1 : view.y;
      var nextM = view.m === 11 ? 0 : view.m + 1, nextY = view.m === 11 ? view.y + 1 : view.y;
      var cells = [];
      for (var i = lead; i > 0; i--) cells.push({ y: prevY, m: prevM, d: daysIn(prevY, prevM) - i + 1, out: true });
      for (var d = 1; d <= daysIn(view.y, view.m); d++) cells.push({ y: view.y, m: view.m, d: d });
      var n = 1;
      while (cells.length % 7) cells.push({ y: nextY, m: nextM, d: n++, out: true });
      cells.forEach(function (c) {
        var key = iso(c.y, c.m, c.d), on = !!chosen[key];
        h += '<button type="button" data-md="day" data-date="' + key + '" aria-pressed="' + on + '" class="md-day' +
          (c.out ? ' is-out' : '') + (on ? ' is-on' : '') + (key === todayIso ? ' is-today' : '') +
          '" aria-label="' + c.d + ' ' + MONTHS_LONG[c.m] + ' ' + c.y + '">' + c.d + '</button>';
      });
      h += '</div></div></div>';
      h += '<div class="actions md-foot"><small class="muted">Klik de dagen aan waarop je deze uren wilt schrijven. Elke dag wordt een eigen registratie.</small>' +
        '<button type="button" class="quiet" data-md="clear">Wissen</button></div>';
      panel.innerHTML = h;
    }

    panel.addEventListener('click', function (e) {
      var b = e.target.closest('[data-md]');
      if (!b) return;
      var what = b.getAttribute('data-md');
      if (what === 'prev-year') view.y--;
      else if (what === 'next-year') view.y++;
      else if (what === 'month') view.m = +b.getAttribute('data-m');
      else if (what === 'day') {
        var key = b.getAttribute('data-date');
        if (chosen[key]) delete chosen[key]; else chosen[key] = true;
        var p = parse(key);
        if (p.m !== view.m || p.y !== view.y) { view.y = p.y; view.m = p.m; }
        sync();
      } else if (what === 'clear') { chosen = {}; sync(); }
      render();
    });
    toggle.addEventListener('click', function () {
      var open = panel.hidden;
      if (open && !list().length && dateInput && parse(dateInput.value)) {
        chosen[dateInput.value] = true;
        sync();
      }
      panel.hidden = !open;
      toggle.setAttribute('aria-expanded', String(open));
      if (open) render();
    });
    sync();
    if (list().length) { panel.hidden = false; toggle.setAttribute('aria-expanded', 'true'); render(); }
  }
  document.addEventListener('DOMContentLoaded', function () {
    document.querySelectorAll('[data-multiday]').forEach(init);
  });
})();

// Pop-up windows (<dialog>): links/buttons with data-dialog-open="id" open
// it; data-dialog-close closes it. Without JavaScript those links go to
// ?nieuw=1, where the server renders the window already open.
document.addEventListener('click', function (e) {
  var open = e.target.closest('[data-dialog-open]');
  if (open) {
    var d = document.getElementById(open.getAttribute('data-dialog-open'));
    if (d && d.showModal) {
      e.preventDefault();
      if (d.open) d.close();
      d.showModal();
      // Focus only with a mouse: on a phone it would pop up a picker/keyboard.
      if (window.matchMedia && matchMedia('(pointer: fine)').matches) {
        var first = d.querySelector('select[name=task_id], input:not([type=hidden]):not([readonly]), select, textarea');
        if (first) first.focus();
      }
    }
    return;
  }
  var close = e.target.closest('[data-dialog-close]');
  if (close) {
    var dlg = close.closest('dialog');
    if (dlg) { e.preventDefault(); dlg.close(); }
  }
});
document.addEventListener('DOMContentLoaded', function () {
  // A window the server rendered open (validation error, ?nieuw=1) becomes modal.
  document.querySelectorAll('dialog[data-open-modal]').forEach(function (d) {
    if (d.showModal) { d.close(); d.showModal(); }
  });
});
// Click on the dimmed background closes a window.
document.addEventListener('click', function (e) {
  if (e.target.tagName === 'DIALOG' && e.target.open) e.target.close();
});
// Selects that apply themselves (e.g. "Uren van …").
document.addEventListener('change', function (e) {
  if (e.target.matches('select[data-autosubmit]')) e.target.form.submit();
});

// Read-only account: the fields of a form can be read but not changed.
(function () {
  if (!document.body.classList.contains('read-only')) return;
  document.querySelectorAll('main form[method=post]:not([data-keep])').forEach(function (f) {
    f.querySelectorAll('input, select, textarea, button').forEach(function (el) { el.disabled = true; });
  });
})();
