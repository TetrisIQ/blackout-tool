// "Did you mean ...?" for the offline search, in the browser. Reads the word index that `kit spell`
// builds (`/spell/books.json` and `/spell/<book>/w<length>.tsv`, see src/spell.rs). Used by the
// result page and the assistant. No DOM access, so `node test/spell.test.mjs` can test it.
// `norm` mirrors `mapsearch::norm` in Rust; change both together.
(function (root) {
  'use strict';

  const FOLDS = [
    ['àáâãäåāăąǎ', 'a'], ['çćĉċč', 'c'], ['ďđð', 'd'], ['èéêëēĕėęě', 'e'], ['ĝğġģ', 'g'], ['ĥħ', 'h'],
    ['ìíîïĩīĭįı', 'i'], ['ĵ', 'j'], ['ķ', 'k'], ['ĺļľŀł', 'l'], ['ñńņň', 'n'], ['òóôõöøōŏő', 'o'],
    ['ŕŗř', 'r'], ['śŝşšș', 's'], ['ţťŧț', 't'], ['ùúûüũūŭůűų', 'u'], ['ŵ', 'w'], ['ýÿŷ', 'y'], ['źżž', 'z'],
  ];
  const FOLD = new Map();
  for (const [from, to] of FOLDS) for (const c of from) FOLD.set(c, to);
  const SPECIAL = { 'ß': 'ss', 'æ': 'ae', 'œ': 'oe', 'þ': 'th' };

  // Lower case, accents and ß etc. folded, anything that is not a letter or digit becomes one space.
  function norm(s) {
    let out = '';
    let space = true;
    for (const c of s.toLowerCase()) {
      if (c >= '̀' && c <= 'ͯ') continue;
      if (SPECIAL[c]) { out += SPECIAL[c]; space = false; continue; }
      if (/[\p{L}\p{N}]/u.test(c)) { out += FOLD.get(c) || c; space = false; continue; }
      if (!space) { out += ' '; space = true; }
    }
    return out.replace(/ $/, '');
  }

  const MIN_LEN = 3, MAX_LEN = 24;
  const TOKEN = /[\p{L}\p{N}̀-ͯ]+/gu;

  // Edit distance with transpositions ("teh" -> "the" is 1), or `max + 1` as soon as it is surely more.
  function distance(a, b, max) {
    if (Math.abs(a.length - b.length) > max) return max + 1;
    let prev2 = null;
    let prev = Array.from({ length: b.length + 1 }, (_, j) => j);
    for (let i = 1; i <= a.length; i++) {
      const cur = [i];
      let best = i;
      for (let j = 1; j <= b.length; j++) {
        let v = Math.min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + (a[i - 1] === b[j - 1] ? 0 : 1));
        if (prev2 && i > 1 && j > 1 && a[i - 1] === b[j - 2] && a[i - 2] === b[j - 1]) v = Math.min(v, prev2[j - 2] + 1);
        cur[j] = v;
        if (v < best) best = v;
      }
      if (best > max) return max + 1;
      prev2 = prev;
      prev = cur;
    }
    return prev[b.length];
  }

  // How far a word may be from the one meant: one slip in short words, two in long ones.
  const maxDistance = (len) => (len >= 8 ? 2 : 1);

  function matchCase(from, to) {
    if (from.length > 1 && from === from.toUpperCase() && from !== from.toLowerCase()) return to.toUpperCase();
    if (from[0] !== from[0].toLowerCase() && from.slice(1) === from.slice(1).toLowerCase()) return to[0].toUpperCase() + to.slice(1);
    return to;
  }

  function parseShard(text) {
    const words = [], counts = [], display = [];
    const has = new Set();
    for (const line of (text || '').split('\n')) {
      if (!line) continue;
      const [w, c, d] = line.split('\t');
      words.push(w);
      counts.push(parseInt(c, 10) || 1);
      display.push(d || w);
      has.add(w);
    }
    return { words, counts, display, has };
  }

  // `get(url)` resolves to the body text, or null when there is none (the pages pass a fetch wrapper).
  function create(get) {
    let books = null;
    const shards = new Map();

    async function ready() {
      if (books === null) {
        try {
          const t = await get('/spell/books.json');
          books = t ? JSON.parse(t) : [];
        } catch (e) {
          books = [];
        }
      }
      return books.length > 0;
    }

    function shard(book, len) {
      const key = book + '/' + len;
      if (!shards.has(key)) {
        shards.set(key, get('/spell/' + encodeURIComponent(book) + '/w' + len + '.tsv').then(parseShard, () => parseShard('')));
      }
      return shards.get(key);
    }

    // The best known word within reach of `word` (folded), or null; `known` when it is in the index itself.
    async function lookup(word) {
      const len = [...word].length;
      const max = maxDistance(len);
      const lo = Math.max(MIN_LEN, len - max), hi = Math.min(MAX_LEN, len + max);
      const found = new Map();
      let known = false;
      for (const b of books) {
        for (let l = lo; l <= hi; l++) {
          const s = await shard(b.book, l);
          if (l === len && s.has.has(word)) known = true;
          for (let i = 0; i < s.words.length; i++) {
            const d = distance(word, s.words[i], max);
            if (d > max) continue;
            const prior = found.get(s.words[i]);
            if (prior) prior.count += s.counts[i];
            else found.set(s.words[i], { word: s.words[i], display: s.display[i], dist: d, count: s.counts[i] });
          }
        }
      }
      if (known) return { known: true };
      let best = null;
      for (const c of found.values()) {
        if (!best || c.dist < best.dist || (c.dist === best.dist && (c.count > best.count || (c.count === best.count && c.word < best.word)))) best = c;
      }
      return best ? { known: false, to: best.display } : null;
    }

    // `text` with every unknown word replaced by the closest known one:
    // `{text, changes: [{from, to}]}`, or null when nothing needs (or can get) a correction.
    async function suggest(text) {
      if (!text || !(await ready())) return null;
      const changes = [];
      let out = '', at = 0;
      for (const m of text.matchAll(TOKEN)) {
        out += text.slice(at, m.index);
        at = m.index + m[0].length;
        let rep = m[0];
        const word = norm(m[0]);
        const len = [...word].length;
        // Numbers, words with digits and very short words are left alone.
        if (len >= MIN_LEN + 1 && len <= MAX_LEN && !/[\p{N}\s]/u.test(m[0]) && !/ /.test(word)) {
          const r = await lookup(word);
          if (r && !r.known) {
            rep = matchCase(m[0], r.to);
            changes.push({ from: m[0], to: rep });
          }
        }
        out += rep;
      }
      out += text.slice(at);
      return changes.length ? { text: out, changes } : null;
    }

    return { suggest, ready };
  }

  const api = { create, norm, distance, parseShard };
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
  else root.Spell = api;
})(typeof self !== 'undefined' ? self : this);
