// Page translation in the reading frame. The engine (Bergamot WASM) is served from /translate/engine/,
// the models of each language pair from /translate/<src>-<tgt>/; nothing leaves the machine.
(function () {
  var btn = document.getElementById('translate-btn');
  var pairs = window.kitTranslatePairs || [];
  if (!btn || !pairs.length) return;
  var label = document.getElementById('translate-label');
  var SKIP = /^(SCRIPT|STYLE|NOSCRIPT|CODE|PRE|TEXTAREA|SVG|MATH)$/;
  var BATCH = 40;
  var state = { on: false, originals: null, busy: false };
  var KEY = 'kit-translate-lang';

  // The worker (assets/translate/translator-worker.js) answers {id, result|error} to {id, name, args}.
  var worker = null, seq = 0, waiting = {}, ready = null, loaded = {};
  function call(name, args) {
    return new Promise(function (resolve, reject) {
      if (!worker) {
        worker = new Worker('/translate/engine/translator-worker.js');
        worker.onmessage = function (e) {
          var w = waiting[e.data.id]; if (!w) return;
          delete waiting[e.data.id];
          if (e.data.error) w.reject(new Error(e.data.error.message)); else w.resolve(e.data.result);
        };
        worker.onerror = function (e) {
          Object.keys(waiting).forEach(function (k) { waiting[k].reject(new Error(e.message || 'engine failed')); delete waiting[k]; });
          worker = null; ready = null; loaded = {};
        };
      }
      var id = ++seq; waiting[id] = { resolve: resolve, reject: reject };
      worker.postMessage({ id: id, name: name, args: args });
    });
  }
  function bytes(url) {
    return fetch(url).then(function (r) {
      if (!r.ok) throw new Error(url + ': ' + r.status);
      return r.arrayBuffer();
    });
  }
  function optional(url) { return fetch(url).then(function (r) { return r.ok ? r.arrayBuffer() : null; }); }
  // Loads the files of one pair into the worker once per page view.
  function model(pair) {
    if (!ready) ready = call('initialize', [{ useNativeIntGemm: false, cacheSize: 0 }]);
    return ready.then(function () {
      if (loaded[pair]) return loaded[pair];
      var base = '/translate/' + pair + '/', ft = pair.split('-');
      return loaded[pair] = Promise.all([
        bytes(base + 'model.bin'), bytes(base + 'lex.bin'),
        optional(base + 'vocab.spm'), optional(base + 'srcvocab.spm'), optional(base + 'trgvocab.spm')
      ]).then(function (f) {
        var vocabs = f[2] ? [f[2], f[2]] : [f[3], f[4]];
        return call('loadTranslationModel', [{ from: ft[0], to: ft[1] }, { model: f[0], shortlist: f[1], vocabs: vocabs }]);
      });
    });
  }
  function run(pair, texts) {
    var ft = pair.split('-');
    return model(pair).then(function () {
      return call('translate', [{ models: [{ from: ft[0], to: ft[1] }], texts: texts.map(function (t) { return { text: t, html: false }; }) }]);
    }).then(function (res) { return res.map(function (r) { return r.target.text; }); });
  }

  function textNodes(doc) {
    var out = [], w = doc.createTreeWalker(doc.body, NodeFilter.SHOW_TEXT, {
      acceptNode: function (n) {
        if (!n.nodeValue.trim()) return NodeFilter.FILTER_REJECT;
        for (var p = n.parentNode; p && p !== doc.body; p = p.parentNode)
          if (SKIP.test(p.nodeName.toUpperCase()) || p.translate === false || p.getAttribute && p.getAttribute('translate') === 'no') return NodeFilter.FILTER_REJECT;
        return NodeFilter.FILTER_ACCEPT;
      }
    });
    while (w.nextNode()) out.push(w.currentNode);
    return out;
  }

  function sourceLang(doc) { return ((doc.documentElement.lang || '').toLowerCase().split('-')[0]) || null; }
  function pairFor(src) {
    var pref = localStorage.getItem(KEY);
    var cands = pairs.filter(function (p) { return p.split('-')[0] === src; });
    return cands.filter(function (p) { return p.split('-')[1] === pref; })[0] || cands[0] || null;
  }

  function restore(frame) {
    if (state.originals) state.originals.forEach(function (o) { o.n.nodeValue = o.t; });
    state.originals = null; state.on = false;
    btn.classList.remove('on'); label.textContent = 'Translate';
    try { frame.contentDocument.documentElement.lang = state.lang || frame.contentDocument.documentElement.lang; } catch (e) {}
  }

  async function translate(frame) {
    var doc = frame.contentDocument, src = sourceLang(doc), pair = src && pairFor(src);
    if (!pair) { label.textContent = src ? 'No ' + src + ' model' : 'Language unknown'; setTimeout(function () { label.textContent = 'Translate'; }, 2500); return; }
    state.busy = true; btn.disabled = true;
    var nodes = textNodes(doc), saved = nodes.map(function (n) { return { n: n, t: n.nodeValue }; });
    try {
      for (var i = 0; i < nodes.length; i += BATCH) {
        label.textContent = 'Translating ' + Math.min(100, Math.round(100 * i / nodes.length)) + '%';
        var part = nodes.slice(i, i + BATCH);
        var res = await run(pair, part.map(function (n) { return n.nodeValue; }));
        part.forEach(function (n, k) { if (res[k]) n.nodeValue = res[k]; });
      }
      state.originals = saved; state.on = true; state.lang = doc.documentElement.lang;
      doc.documentElement.lang = pair.split('-')[1];
      btn.classList.add('on'); label.textContent = 'Show original';
    } catch (e) {
      saved.forEach(function (o) { o.n.nodeValue = o.t; });
      label.textContent = 'Translation failed'; setTimeout(function () { label.textContent = 'Translate'; }, 2500);
    } finally { state.busy = false; btn.disabled = false; }
  }

  var current = null;
  btn.addEventListener('click', function () {
    if (!current || state.busy) return;
    if (state.on) restore(current); else translate(current);
  });

  window.kitTranslate = {
    // A new page loaded in the frame: reset, and show the button only for articles.
    onLoad: function (frame, isArticle) {
      current = frame; state.on = false; state.originals = null;
      btn.classList.remove('on'); label.textContent = 'Translate';
      var ok = false;
      try { ok = isArticle(frame.contentWindow.location.pathname) && !!frame.contentDocument.body; } catch (e) {}
      btn.hidden = !ok;
    }
  };
})();
