// Fork: meeting bar. Query: ?mode=recording|stopped&app=<name>&since=<epoch ms>
(function () {
  var q = new URLSearchParams(location.search);
  var mode = q.get('mode') || 'recording';
  var app = q.get('app') || '';
  var since = Number(q.get('since')) || Date.now();
  var compact = q.get('compact') === '1';
  function invoke(cmd, args) {
    try { return window.__TAURI_INTERNALS__.invoke(cmd, args || {}); } catch (e) { console.error(e); }
  }
  var $ = function (id) { return document.getElementById(id); };
  $('open').onclick = function () { invoke('meeting_pill_open'); };
  $('stop').onclick = function () { invoke('meeting_pill_stop'); };
  $('close').onclick = function () { invoke('meeting_pill_close'); };

  // Small view (dot + timer) <-> full bar. Click the small one to grow it back.
  function setCompact(on, save) {
    compact = on;
    $('pill').className = on ? 'pill hidden' : 'pill';
    $('mini').className = on ? 'mini' : 'mini hidden';
    if (save) invoke('meeting_pill_compact', { compact: on });
  }
  $('min').onclick = function () { setCompact(true, true); };

  // Drag the window from anywhere that is not a button; a click without moving on the
  // small view grows it back.
  var down = null;
  document.addEventListener('mousedown', function (e) {
    if (e.button !== 0 || e.target.closest('button')) return;
    down = { x: e.screenX, y: e.screenY };
  });
  document.addEventListener('mousemove', function (e) {
    if (down && (Math.abs(e.screenX - down.x) > 3 || Math.abs(e.screenY - down.y) > 3)) {
      down = null;
      invoke('meeting_pill_drag');
    }
  });
  document.addEventListener('mouseup', function (e) {
    if (down && compact && e.target.closest('#mini')) setCompact(false, true);
    down = null;
  });
  setCompact(compact, false);

  if (mode === 'stopped') {
    $('dot').className = 'dot off';
    $('title').textContent = 'Meeting terminato';
    $('sub').textContent = 'Trascrizione salvata. Il resoconto va in Notion appena pronto.';
    $('stop').className = 'hidden';
    $('open').textContent = 'Apri meeting';
    $('mdot').className = 'dot off';
    $('mclock').textContent = 'Salvato';
    $('min').className = 'hidden';
    return;
  }
  $('sub').textContent = (app ? 'Meeting rilevato (' + app + ')' : 'Meeting rilevato') + ' · la trascrizione si apre in Meetily';
  function tick() {
    var s = Math.max(0, Math.floor((Date.now() - since) / 1000));
    var m = Math.floor(s / 60), r = s % 60;
    var t = (m < 10 ? '0' : '') + m + ':' + (r < 10 ? '0' : '') + r;
    $('clock').textContent = t;
    $('mclock').textContent = t;
  }
  tick(); setInterval(tick, 1000);
})();
