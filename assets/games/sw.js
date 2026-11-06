// Service worker of the game hub. {{REV}} changes whenever the installed games change, so the
// browser replaces this file and the hub shell together. The games themselves are stored by the
// page ("Save offline") in one cache per game: kit-game-<id>.
const SHELL = 'kit-hub-shell-{{REV}}';
const SHELL_FILES = ['./', 'index.html', 'manifest.webmanifest', 'icon-192.png', 'icon-512.png'];

self.addEventListener('install', (e) => {
  e.waitUntil(caches.open(SHELL).then((c) => c.addAll(SHELL_FILES)).then(() => self.skipWaiting()));
});

self.addEventListener('activate', (e) => {
  e.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((k) => k.startsWith('kit-hub-shell-') && k !== SHELL).map((k) => caches.delete(k))))
      .then(() => self.clients.claim())
  );
});

function isShell(url) {
  const scope = new URL(self.registration.scope).pathname;
  const rel = url.pathname.slice(scope.length);
  return rel === '' || rel === 'index.html' || SHELL_FILES.includes(rel);
}

self.addEventListener('fetch', (e) => {
  const req = e.request;
  const url = new URL(req.url);
  if (req.method !== 'GET' || url.origin !== location.origin) return;
  if (isShell(url)) {
    // Network first, so a changed game list shows up at once; the cache answers when offline.
    e.respondWith(
      fetch(req).then((res) => {
        if (res.ok) { const copy = res.clone(); caches.open(SHELL).then((c) => c.put(req, copy)); }
        return res;
      }).catch(() => caches.match(req, { ignoreSearch: true }).then((r) => r || caches.match('index.html')))
    );
    return;
  }
  if (url.pathname.includes('/play/')) {
    // Saved games come from the cache and keep working without the laptop.
    const withIndex = url.pathname.endsWith('/') ? new Request(url.pathname + 'index.html') : req;
    e.respondWith(caches.match(withIndex, { ignoreSearch: true }).then((r) => r || fetch(req)));
  }
});
