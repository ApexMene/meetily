// Fork: meeting bar. Query: ?mode=recording|stopped&app=<name>&since=<epoch ms>
(function () {
  var q = new URLSearchParams(location.search);
  var mode = q.get('mode') || 'recording';
  var app = q.get('app') || '';
  var since = Number(q.get('since')) || Date.now();
  function invoke(cmd) {
    try { return window.__TAURI_INTERNALS__.invoke(cmd); } catch (e) { console.error(e); }
  }
  var $ = function (id) { return document.getElementById(id); };
  $('open').onclick = function () { invoke('meeting_pill_open'); };
  $('stop').onclick = function () { invoke('meeting_pill_stop'); };
  $('close').onclick = function () { invoke('meeting_pill_close'); };

  if (mode === 'stopped') {
    $('dot').className = 'dot off';
    $('title').textContent = 'Meeting terminato';
    $('sub').textContent = 'Trascrizione salvata. Il resoconto va in Notion appena pronto.';
    $('stop').className = 'hidden';
    $('open').textContent = 'Apri meeting';
    return;
  }
  $('sub').textContent = (app ? 'Meeting rilevato (' + app + ')' : 'Meeting rilevato') + ' · la trascrizione si apre in Meetily';
  function tick() {
    var s = Math.max(0, Math.floor((Date.now() - since) / 1000));
    var m = Math.floor(s / 60), r = s % 60;
    $('clock').textContent = (m < 10 ? '0' : '') + m + ':' + (r < 10 ? '0' : '') + r;
  }
  tick(); setInterval(tick, 1000);
})();
