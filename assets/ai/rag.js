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
    'der die das und oder zu in auf für mit ist sind wie was wann wo warum kann ich du mein eine ein ' +
    'may should would could does did during after before about there their them this that from into stop').split(' '));

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

  // The search needs all terms to occur, so a long list finds nothing. Order of tries: the full list, with up to
  // 4 terms every list that leaves one term out (last ones first), then the first 3, the first 2 and the first term.
  function queryLadder(query) {
    const w = query.split(' ').filter(Boolean);
    const out = [w.join(' ')];
    if (w.length >= 3 && w.length <= 4) {
      for (let drop = w.length - 1; drop >= 0; drop--) out.push(w.filter((_, i) => i !== drop).join(' '));
    }
    out.push(w.slice(0, 3).join(' '), w.slice(0, 2).join(' '));
    if (w.length >= 3) out.push(w[0]);
    return [...new Set(out)].filter((q) => q.length > 0);
  }

  // Ready.gov and similar books hold every page once per language under the same title (".../ar/node/1",
  // ".../fr/node/1"); they would fill the whole result list. Keep one per title, the shortest link (the
  // original has no language segment), at the position of the first occurrence.
  function dedupeTitles(items) {
    const best = new Map();
    for (const i of items) {
      const b = best.get(i.title);
      if (!b || i.href.length < b.href.length) best.set(i.title, i);
    }
    return items.filter((i) => best.get(i.title) === i);
  }

  // Hits whose title and snippet mention more of the query terms first; ties keep the search engine's order.
  function rankHits(items, query) {
    const score = (i) => 2 * coverage(i.title, query) + coverage(i.title + ' ' + i.snippet, query);
    return items
      .map((item, n) => ({ item, n, s: score(item) }))
      .sort((a, b) => b.s - a.s || a.n - b.n)
      .map((x) => x.item);
  }

  // Candidate pages for a keyword string. The search needs all terms, so the ladder of shorter strings is
  // searched too and everything is ranked together against the full keyword string by how much of it the
  // title and snippet mention (the engine's rank within a list breaks ties). Every book is searched on its
  // own, because one big book such as the Wikipedia medicine extract would otherwise fill the list and hide
  // Ready.gov or CDC; no book contributes more than `perBook` pages.
  async function candidates(query, extra, get, books, o) {
    const names = books && books.length > 0 ? books : [null];
    const lists = await Promise.all([...new Set([...queryLadder(query), ...extra.flatMap(queryLadder)])].flatMap((q) => names.map(async (b) => {
      const scope = b ? '&books.name=' + encodeURIComponent(b) : '';
      let xml = '';
      try {
        xml = await get('/search?format=xml&pageLength=' + o.pageLength + '&pattern=' + encodeURIComponent(q) + scope);
      } catch (e) {
        // One failed search (kiwix answers 500 for some odd strings) must not lose the others.
      }
      return dedupeTitles(dropVariants(parseSearch(xml))).map((item, n) => ({ item, n, book: b || item.href.split('/')[2] }));
    })));
    const best = new Map();
    for (const x of lists.flat()) {
      const k = x.item.href;
      if (!best.has(k) || x.n < best.get(k).n) best.set(k, x);
    }
    const all = [query, ...extra].join(' ');
    const score = (x) => 2 * coverage(x.item.title, all) + coverage(x.item.title + ' ' + x.item.snippet, all) - 0.05 * x.n -
      0.03 * x.item.title.replace(/\|.*$/, '').split(/\s+/).length;
    const ranked = [...best.values()].sort((a, b) => score(b) - score(a));
    const used = {};
    return ranked.filter((x) => (used[x.book] = (used[x.book] || 0) + 1) <= o.perBook).slice(0, o.hits).map((x) => x.item);
  }

  // Words that say "this is a question about help" and never occur as the subject of a page; they only dilute the
  // search (every term has to occur) and the ranking.
  const FILLER = new Set(('emergency medical treatment treat treating guidelines guideline procedures procedure tips safety first aid ' +
    'symptoms prevention methods steps advice instructions information best practices what how').split(' '));

  function keywordMessages(question) {
    return [
      {
        role: 'system',
        content:
          'Turn the question into 3 or 4 English search keywords for an encyclopedia: the subject itself, named as an ' +
          'article title would name it, most important first. Leave out generic words like emergency, medical, ' +
          'treatment, safety, first aid, guide. Always use English words, even if the question is in another language. ' +
          'Examples: "How do I get rid of head lice?" -> head lice. "Was tun bei einem Wespenstich?" -> wasp sting. ' +
          'Answer with the keywords only, separated by spaces, no punctuation, no explanation.',
      },
      { role: 'user', content: question },
    ];
  }

  // Model output -> a safe search string; falls back to the question itself.
  function cleanKeywords(output, question) {
    const words = (output || '').toLowerCase().match(/[\p{L}\p{N}]{2,}/gu) || [];
    const subject = words.filter((w) => !FILLER.has(w));
    const picked = (subject.length >= 2 ? subject : words).slice(0, 5);
    return picked.length >= 2 ? picked.join(' ') : terms(question).slice(0, 8).join(' ') || question;
  }

  // The question's own content words (generic words dropped), at most 4: a second way to search that does not
  // depend on how the model chose its keywords.
  function questionTerms(question) {
    const w = [...new Set(terms(question).filter((t) => !FILLER.has(t)))].slice(0, 4);
    return w.length >= 2 ? w.join(' ') : '';
  }

  // Passages for a keyword string. `get(url)` resolves to the response body text (the page passes fetch).
  // With `opts.question` the question's own terms are searched as well. Returns `{found: [{title, href, text}],
  // used}`; `used` is the keyword string, for the "nothing found" notice.
  async function retrieve(query, get, books, opts) {
    const o = Object.assign({ hits: 4, perBook: 2, chars: 1400, pageLength: 6, question: '' }, opts);
    const extra = [questionTerms(o.question)].filter((q) => q && q !== query);
    const all = [query, ...extra].join(' ');
    const out = [];
    for (const h of await candidates(query, extra, get, books, o)) {
      try {
        out.push({ title: h.title, href: h.href, text: pickPassage(htmlToText(await get(h.href)), all, o.chars) });
      } catch (e) {
        out.push({ title: h.title, href: h.href, text: h.snippet });
      }
    }
    // Keep passages that mention at least half of the keywords (title included).
    return { found: out.filter((p) => p.text.length > 0 && coverage(p.title + ' ' + p.text, query) >= 0.5), used: query };
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

  // Ollama options of the two calls; the page and test/ai_eval.mjs share them so the evaluation measures what runs.
  const OPTIONS = {
    keywords: { temperature: 0, num_predict: 24, num_ctx: 8192 },
    answer: { temperature: 0.2, num_ctx: 8192, num_predict: 700 },
    reasoning: { temperature: 0.2, num_ctx: 8192, num_predict: 3000 },
  };

  const api = { OPTIONS, retrieve, rankHits, dedupeTitles, queryLadder, dropVariants, coverage, decodeEntities, parseSearch, htmlToText, terms, pickPassage, keywordMessages, cleanKeywords, buildMessages };
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
  else root.Rag = api;
})(typeof globalThis !== 'undefined' ? globalThis : this);
