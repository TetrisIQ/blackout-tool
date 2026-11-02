(function () {
  var root = document.documentElement, key = 'kit-theme', saved = null;
  try { saved = localStorage.getItem(key); } catch (e) {}
  var dark = window.matchMedia && matchMedia('(prefers-color-scheme: dark)');
  // Without a saved choice the system decides, and follows it when it changes.
  function apply() { root.setAttribute('data-theme', saved || (dark && dark.matches ? 'dark' : 'light')); }
  apply();
  if (dark && dark.addEventListener) dark.addEventListener('change', apply);
  window.kitTheme = {
    toggle: function () {
      saved = root.getAttribute('data-theme') === 'dark' ? 'light' : 'dark';
      try { localStorage.setItem(key, saved); } catch (e) {}
      apply();
    }
  };
})();
