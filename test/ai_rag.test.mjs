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

assert.equal(R.cleanKeywords('Burn first aid, water!\nSure', 'x y'), 'burn first aid water sure');
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

const msgs = R.buildMessages('Q?', [{ title: 'T', text: 'body' }], [{ role: 'user', content: 'earlier' }]);
assert.equal(msgs[0].role, 'system');
assert.ok(msgs[0].content.includes('[1] T\nbody') && msgs[0].content.includes('Never invent'));
assert.deepEqual(msgs.slice(1).map((m) => m.content), ['earlier', 'Q?']);
console.log('ai_rag: all checks passed');
