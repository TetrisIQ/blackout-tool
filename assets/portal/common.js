(function () {
  var tag = document.getElementById('net'), box = document.getElementById('launcher');
  function net() { if (tag) tag.lastChild.textContent = navigator.onLine ? 'Internet available · everything also works offline' : 'Offline · all services local'; }
  window.addEventListener('online', net); window.addEventListener('offline', net); net();
  document.addEventListener('mousedown', function (e) { if (!box.contains(e.target)) box.open = false; });
  document.addEventListener('keydown', function (e) { if (e.key === 'Escape') box.open = false; });
  var rnd = document.getElementById('random');
  if (rnd) rnd.addEventListener('click', function () {
    var books = rnd.dataset.books.split(',');
    var book = books[Math.floor(Math.random() * books.length)];
    rnd.href = rnd.dataset.prefix + '/random?content=' + book + '&r=' + Date.now();
  });
  var dlg = document.getElementById('providers'), open = document.getElementById('providers-open');
  if (dlg && open && dlg.showModal) {
    open.addEventListener('click', function () { dlg.showModal(); });
    dlg.addEventListener('click', function (e) { if (e.target === dlg || e.target.hasAttribute('data-close')) dlg.close(); });
  }
  var tb = document.getElementById('theme-toggle');
  if (tb && window.kitTheme) {
    var label = function () { tb.lastChild.textContent = document.documentElement.getAttribute('data-theme') === 'dark' ? 'Light theme' : 'Dark theme'; };
    tb.addEventListener('click', function () { kitTheme.toggle(); label(); });
    label();
  }
})();
