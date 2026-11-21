// Fork: meeting bar (src-tauri/src/meeting_automation/bar.rs).
// Query: ?mode=recording|stopped&app=<name>&since=<epoch ms>&compact=0|1&key=<shortcut>&after=0|1
(function () {
  var q = new URLSearchParams(location.search);
  var mode = q.get('mode') || 'recording';
  var app = q.get('app') || '';
  var since = Number(q.get('since')) || Date.now();
  var compact = q.get('compact') === '1';
  var key = q.get('key') || '';
  var afterMeeting = q.get('after') === '1';
  function invoke(cmd, args) {
    try { return window.__TAURI_INTERNALS__.invoke(cmd, args || {}); } catch (e) { console.error(e); }
  }
  var $ = function (id) { return document.getElementById(id); };
  $('open').onclick = function () { invoke('meeting_bar_open'); };
  $('stop').onclick = function () { invoke('meeting_bar_stop'); };
  $('close').onclick = function () { invoke('meeting_bar_close'); };

  // Small view (dot + timer) <-> full bar. Click the small one to enlarge it again.
  function setCompact(on, save) {
    compact = on;
    $('bar').className = on ? 'bar hidden' : 'bar';
    $('mini').className = on ? 'mini' : 'mini hidden';
    if (save) invoke('meeting_bar_compact', { compact: on });
  }
  $('min').onclick = function () { setCompact(true, true); };

  // Drag the window from anywhere that is not a button; a click without moving on the small
  // view enlarges it again.
  var down = null;
  document.addEventListener('mousedown', function (e) {
    if (e.button !== 0 || e.target.closest('button')) return;
    down = { x: e.screenX, y: e.screenY };
  });
  document.addEventListener('mousemove', function (e) {
    if (down && (Math.abs(e.screenX - down.x) > 3 || Math.abs(e.screenY - down.y) > 3)) {
      down = null;
      invoke('meeting_bar_drag');
    }
  });
  document.addEventListener('mouseup', function (e) {
    if (down && compact && e.target.closest('#mini')) setCompact(false, true);
    down = null;
  });
  setCompact(compact, false);

  if (mode === 'stopped') {
    $('dot').className = 'dot off';
    $('title').textContent = 'Meeting ended';
    $('sub').textContent = 'Transcript saved.' + (afterMeeting ? ' The after-meeting work starts shortly.' : '') +
      (key ? ' ' + key + ' to record again.' : '');
    $('stop').className = 'hidden';
    $('min').className = 'hidden';
    $('mdot').className = 'dot off';
    $('mclock').textContent = 'Saved';
    if (afterMeeting) {
      $('open').textContent = 'Status';
      $('open').onclick = function () { invoke('post_meeting_open_jobs'); invoke('meeting_bar_close'); };
    } else {
      $('open').className = 'hidden';
    }
    return;
  }
  $('sub').textContent = (app ? 'Meeting detected (' + app + ')' : 'Started by you') +
    (key ? ' · ' + key + ' to stop' : ' · the transcript is in Meetily');
  if (key) $('stop').title = key;
  function tick() {
    var s = Math.max(0, Math.floor((Date.now() - since) / 1000));
    var m = Math.floor(s / 60), r = s % 60;
    var t = (m < 10 ? '0' : '') + m + ':' + (r < 10 ? '0' : '') + r;
    $('clock').textContent = t;
    $('mclock').textContent = t;
  }
  tick(); setInterval(tick, 1000);
})();
