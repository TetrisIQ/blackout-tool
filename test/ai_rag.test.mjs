// Run: node test/ai_rag.test.mjs   (pure logic of assets/ai/rag.js; no network, no browser)
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
const R = createRequire(import.meta.url)('../assets/ai/rag.js');

const xml = `<rss><channel><item><title>Mirrors &amp; lists</title>
<link>/content/arch_2026-10/Mirrors</link>
<description>...<b>pacman</b>-<b>mirrorlist</b> &lt;x&gt;...</description></item>
<item><title>Evil</title><link>https://evil.example/x</link><description>d</description></item>
<item><title>Second</title><link>/content/arch_2026-10/Pacman_(Deutsch)</link><description>s</description></item></channel></rss>`;
const items = R.parseSearch(xml);
assert.equal(items.length, 2, 'links outside /content/ are dropped');
assert.deepEqual(items[0], { title: 'Mirrors & lists', href: '/content/arch_2026-10/Mirrors', snippet: '...pacman-mirrorlist <x>...' });

const text = R.htmlToText('<nav>menu</nav><h1>Burns</h1><p>Cool the <b>burn</b> &amp; wait.</p><script>x()</script><p>&#8211; ice is bad</p>');
assert.equal(text, 'Burns\nCool the burn & wait.\n– ice is bad');

const article = [
  'Introduction paragraph about many unrelated things in the world of software.',
  'Cooling a burn under running water for twenty minutes reduces tissue damage and pain a lot.',
  'Completely different text about mirrors and package managers, long enough to count here.',
].join('\n');
const picked = R.pickPassage(article, 'how to cool a burn with water', 100);
assert.ok(picked.includes('Cooling a burn') && !picked.includes('mirrors'), picked);
assert.ok(R.pickPassage(article, 'zzzz', 90).startsWith('Introduction'), 'no hit: article start');
assert.ok(R.pickPassage(article, 'burn', 200).length <= 200);

assert.equal(R.cleanKeywords('Burn first aid, water!\nSure', 'x y'), 'burn water sure');
assert.equal(R.cleanKeywords('first aid', 'x y'), 'first aid');
assert.equal(R.cleanKeywords('', 'Wie behandle ich eine Verbrennung?'), 'behandle verbrennung');
assert.equal(R.cleanKeywords('ok', 'Wie behandle ich eine Verbrennung?'), 'behandle verbrennung');

assert.equal(R.coverage('Cooling a burn under water', 'how to treat a burn'), 0.5);
assert.equal(R.coverage('HP laptop battery', 'treat burn'), 0);
assert.equal(R.coverage('anything', 'a to of'), 1, 'no usable terms: nothing to filter on');

const t = (title) => ({ title });
assert.deepEqual(
  R.dropVariants([t('Mirrors (Magyar)'), t('Pacman'), t('Mirrors'), t('Pacman (हिन्दी)'), t('Mercury (element)'), t('Mercury (planet)')]).map((x) => x.title),
  ['Pacman', 'Mirrors', 'Mercury (element)', 'Mercury (planet)'],
);

assert.deepEqual(R.queryLadder('a b c d e'), ['a b c d e', 'a b c', 'a b', 'a', 'b', 'c', 'd', 'e']);
assert.deepEqual(R.queryLadder('a b c d'), ['a b c d', 'a b c', 'a b d', 'a c d', 'b c d', 'a b', 'a', 'b', 'c', 'd']);
assert.deepEqual(R.queryLadder('a b c'), ['a b c', 'a b', 'a c', 'b c', 'a', 'b', 'c']);

// One hit per title (the original, not the translated copy), then title/snippet matches first.
const h = (title, href, snippet = '') => ({ title, href, snippet });
const dd = R.dedupeTitles([h('Burns', '/content/b/www/ar/node/1'), h('Water', '/content/b/www/node/2'), h('Burns', '/content/b/www/node/1')]);
assert.deepEqual(dd.map((x) => x.href), ['/content/b/www/node/2', '/content/b/www/node/1']);
assert.deepEqual(
  R.rankHits([h('Urgent care', '/a'), h('Friction burn', '/b'), h('Burns treatment', '/c'), h('Other', '/d', 'minor burn treatment')], 'minor burn treatment').map((x) => x.href),
  ['/c', '/b', '/d', '/a'],
);

// retrieve(): ladder, ranking, dedupe and the coverage filter against a fake library.
const page = (t) => '<html><body><p>' + t + '</p></body></html>';
const rss = (...hs) => '<rss><channel>' + hs.map((x) => '<item><title>' + x.title + '</title><link>' + x.href + '</link><description>' + x.snippet + '</description></item>').join('') + '</channel></rss>';
const calls = [];
const lib = async (url) => {
  calls.push(url);
  if (url.startsWith('/search')) {
    const p = new URL('http://x' + url).searchParams.get('pattern');
    if (p === 'safe drinking water') {
      return rss(h('Water', '/content/b/ar/node/9', 'boil'), h('Water', '/content/b/node/9', 'boil'), h('Noise', '/content/b/n', ''));
    }
    return rss();
  }
  return page(url.endsWith('/node/9') ? 'Boil water for one minute to make drinking water safe in an emergency situation.' : 'Unrelated text about something else entirely, long enough to count.');
};
const r = await R.retrieve('safe drinking water emergency', lib, ['bk']);
assert.equal(r.used, 'safe drinking water emergency');
assert.deepEqual(r.found.map((x) => x.href), ['/content/b/node/9']);
assert.ok(calls[0].includes('books.name=bk') && calls[0].includes('pageLength=6'));
assert.deepEqual((await R.retrieve('zzz yyy', lib, [])).found, []);
assert.deepEqual(R.queryLadder('a b'), ['a b', 'a', 'b']);
assert.deepEqual(R.queryLadder('a'), ['a']);

const msgs = R.buildMessages('Q?', [{ title: 'T', text: 'body' }], [{ role: 'user', content: 'earlier' }]);
assert.equal(msgs[0].role, 'system');
assert.ok(msgs[0].content.includes('[1] T\nbody') && msgs[0].content.includes('Never invent'));
assert.deepEqual(msgs.slice(1).map((m) => m.content), ['earlier', 'Q?']);

// The reply's first line names the passages that are about the question.
assert.deepEqual(R.splitSources('SOURCES: 1 3\n\nAnswer [1].', 4), { state: 'ok', sources: [0, 2], body: 'Answer [1].' });
assert.deepEqual(R.splitSources('Sources: none', 4, true), { state: 'ok', sources: [], body: '' });
assert.deepEqual(R.splitSources('**SOURCES:** 2, 9\ntext', 4), { state: 'ok', sources: [1], body: 'text' });
assert.deepEqual(R.splitSources('SOURCES: 1', 4), { state: 'pending' });
assert.deepEqual(R.splitSources('[1] burns\n[2] a device\nSOURCES: 1\nUse water [1].', 4), { state: 'ok', sources: [0], body: 'Use water [1].' });
assert.deepEqual(R.splitSources('[1] burns\n[2] a de', 4), { state: 'pending' });
assert.deepEqual(R.splitSources('SOUR', 4), { state: 'pending' });
assert.equal(R.splitSources('Here is the answer.\nmore', 4, true).state, 'bad');
assert.equal(R.splitSources('SOURCES: see below\nx', 4).sources.length, 0);
assert.ok(R.buildMessages('Q?', [{ title: 'T', text: 'b' }], [])[0].content.includes('SOURCES: none'));
console.log('ai_rag: all checks passed');
