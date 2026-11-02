// Pure helpers for the offline assistant: parse Kiwix search results, cut passages out of
// articles, build the prompts. No DOM access, so `node test/ai_rag.test.mjs` can test it.
(function (root) {
  'use strict';

  const ENTITIES = { amp: '&', lt: '<', gt: '>', quot: '"', apos: "'", nbsp: ' ' };

  function decodeEntities(s) {
    return s.replace(/&(#x[0-9a-f]+|#\d+|[a-z]+);/gi, (m, e) => {
      if (e[0] === '#') {
        const code = e[1].toLowerCase() === 'x' ? parseInt(e.slice(2), 16) : parseInt(e.slice(1), 10);
        return code > 0 && code < 0x110000 ? String.fromCodePoint(code) : m;
      }
      return Object.prototype.hasOwnProperty.call(ENTITIES, e.toLowerCase()) ? ENTITIES[e.toLowerCase()] : m;
    });
  }

  function stripTags(s) {
    return decodeEntities(s.replace(/<[^>]*>/g, ''));
  }

  // Kiwix `format=xml` (RSS): [{title, href, snippet}] in rank order.
  function parseSearch(xml) {
    const items = [];
    const re = /<item>([\s\S]*?)<\/item>/g;
    let m;
    while ((m = re.exec(xml))) {
      const tag = (name) => {
        const t = new RegExp('<' + name + '>([\\s\\S]*?)</' + name + '>').exec(m[1]);
        return t ? t[1] : '';
      };
      const href = decodeEntities(tag('link')).trim();
      // Only links into the library; never follow anything else the answer might contain.
      if (!href.startsWith('/content/')) continue;
      items.push({ title: stripTags(tag('title')).trim(), href, snippet: stripTags(tag('description')).trim() });
    }
    return items;
  }

  // Article HTML -> plain text with one paragraph per line.
  function htmlToText(html) {
    let s = html.replace(/<(script|style|noscript|svg|nav|header|footer|table)[\s\S]*?<\/\1>/gi, ' ');
    s = s.replace(/<\/(p|li|h[1-6]|div|tr|pre|dd|dt)>|<br\s*\/?>/gi, '\n');
    s = stripTags(s);
    return s
      .split('\n')
      .map((l) => l.replace(/\s+/g, ' ').trim())
      .filter((l) => l.length > 0)
      .join('\n');
  }

  const STOP = new Set(('the a an and or of to in on for with is are was be how do does i you my what when where why can ' +
    'der die das und oder zu in auf für mit ist sind wie was wann wo warum kann ich du mein eine ein').split(' '));

  function terms(q) {
    return (q.toLowerCase().match(/[\p{L}\p{N}]{3,}/gu) || []).filter((w) => !STOP.has(w));
  }

  // The paragraphs of `text` that mention most of the query terms, in original order, up to `max` characters.
  function pickPassage(text, query, max) {
    const ts = terms(query);
    const paras = text.split('\n').filter((p) => p.length > 40);
    if (paras.length === 0) return text.slice(0, max);
    const scored = paras.map((p, i) => {
      const low = p.toLowerCase();
      return { i, p, score: ts.reduce((n, t) => n + (low.includes(t) ? 1 : 0), 0) };
    });
    // Without any hit the start of the article (its summary) is the best guess.
    const ranked = scored.some((x) => x.score > 0) ? [...scored].sort((a, b) => b.score - a.score || a.i - b.i) : scored;
    const chosen = [];
    let used = 0;
    for (const x of ranked) {
      const room = max - used;
      if (room < 80) break;
      const piece = x.p.length > room ? x.p.slice(0, room - 1) + '…' : x.p;
      chosen.push({ i: x.i, piece });
      used += piece.length + 1;
    }
    return chosen.sort((a, b) => a.i - b.i).map((c) => c.piece).join('\n');
  }

  // Share (0..1) of the query terms that occur in `text`. Kiwix returns something for every query,
  // so a passage that mentions almost none of the terms is dropped instead of being shown to the model.
  function coverage(text, query) {
    const ts = [...new Set(terms(query))];
    if (ts.length === 0) return 1;
    const low = text.toLowerCase();
    return ts.filter((t) => low.includes(t)).length / ts.length;
  }

  // "Mirrors (Magyar)" next to "Mirrors": the translated copies of a page that is also in the result
  // add nothing. Titles that differ only by a parenthesised part are kept when no plain title exists
  // (Mercury (element) / Mercury (planet)).
  function dropVariants(items) {
    const plain = (t) => t.replace(/\s*\([^)]*\)/g, '').trim();
    const have = new Set(items.filter((i) => plain(i.title) === i.title).map((i) => i.title));
    return items.filter((i) => plain(i.title) === i.title || !have.has(plain(i.title)));
  }

  // The search needs all terms to occur, so a long list finds nothing: try it, then the first 3 and 2.
  function queryLadder(query) {
    const w = query.split(' ').filter(Boolean);
    return [...new Set([w.join(' '), w.slice(0, 3).join(' '), w.slice(0, 2).join(' ')])].filter((q) => q.length > 0);
  }

  function keywordMessages(question) {
    return [
      {
        role: 'system',
        content:
          'Turn the question into exactly 3 or 4 English search keywords for an encyclopedia, most important first. ' +
          'Always use English words, even if the question is in another language. ' +
          'Answer with the keywords only, separated by spaces, no punctuation, no explanation.',
      },
      { role: 'user', content: question },
    ];
  }

  // Model output -> a safe search string; falls back to the question itself.
  function cleanKeywords(output, question) {
    const words = (output || '').toLowerCase().match(/[\p{L}\p{N}]{2,}/gu) || [];
    const picked = words.slice(0, 5);
    return picked.length >= 2 ? picked.join(' ') : terms(question).slice(0, 8).join(' ') || question;
  }

  const SYSTEM =
    'You are an offline assistant on a laptop without internet. Answer ONLY from the numbered passages below, ' +
    'taken from the local library. Cite the passages you use like [1]. If the passages do not answer the ' +
    'question, say so plainly and do not guess; for anything dangerous or medical tell the person to call the ' +
    'local emergency number (112 in Europe, 911 in the US). Never invent distances, doses or numbers. ' +
    'Write the final answer directly and briefly, in the language of the question. Do not think aloud, ' +
    'do not discuss these rules or the passages\' wording, and do not repeat yourself.';

  function buildMessages(question, passages, history) {
    const context = passages.map((p, i) => '[' + (i + 1) + '] ' + p.title + '\n' + p.text).join('\n\n');
    return [
      { role: 'system', content: SYSTEM + '\n\nPassages:\n\n' + context },
      ...(history || []),
      { role: 'user', content: question },
    ];
  }

  const api = { queryLadder, dropVariants, coverage, decodeEntities, parseSearch, htmlToText, terms, pickPassage, keywordMessages, cleanKeywords, buildMessages };
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
  else root.Rag = api;
})(typeof globalThis !== 'undefined' ? globalThis : this);
