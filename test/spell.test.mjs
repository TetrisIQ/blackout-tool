// Run: node test/spell.test.mjs   (pure logic of assets/portal/spell.js; no network, no browser)
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
const S = createRequire(import.meta.url)('../assets/portal/spell.js');

// The same folding as mapsearch::norm in Rust (see its unit test).
assert.equal(S.norm('Hauptstraße'), 'hauptstrasse');
assert.equal(S.norm('Straße des 17. Juni'), 'strasse des 17 juni');
assert.equal(S.norm('München'), 'munchen');
assert.equal(S.norm('Şişli  Cd.'), 'sisli cd');
assert.equal(S.norm('Łódź'), 'lodz');
assert.equal(S.norm('Øster Allé'), 'oster alle');
assert.equal(S.norm('  --  '), '');
assert.equal(S.norm('école'), 'ecole', 'combining accent');

assert.equal(S.distance('earthquake', 'earthquake', 2), 0);
assert.equal(S.distance('eartquake', 'earthquake', 2), 1);
assert.equal(S.distance('teh', 'the', 1), 1, 'transposition is one slip');
assert.equal(S.distance('kitten', 'sitting', 2), 3, 'more than max: max + 1 or more');
assert.ok(S.distance('abcdef', 'uvwxyz', 2) > 2);

// Two books: words with accents carry their display form, the same word in both books adds up.
const files = {
  '/spell/books.json': JSON.stringify([{ book: 'ready_en', level: 'sample' }, { book: 'med_de', level: 'titles' }]),
  '/spell/ready_en/w10.tsv': 'earthquake\t120\nhurricanes\t30\n',
  '/spell/ready_en/w9.tsv': 'hurricane\t90\nearthquak\t1\ntornadoes\t5\n',
  '/spell/ready_en/w7.tsv': 'tornado\t80\n',
  '/spell/med_de/w6.tsv': 'ubelkeit\t7\tübelkeit\n',
  '/spell/med_de/w10.tsv': 'ubelkeiten\t2\tübelkeiten\nearthquake\t3\n',
  '/spell/med_de/w8.tsv': 'ubelkeit\t7\tübelkeit\n',
};
const asked = [];
const spell = S.create(async (url) => { asked.push(url); return Object.prototype.hasOwnProperty.call(files, url) ? files[url] : null; });

let r = await spell.suggest('eartquake');
assert.deepEqual(r, { text: 'earthquake', changes: [{ from: 'eartquake', to: 'earthquake' }] });
r = await spell.suggest('Hurricain safety');
assert.equal(r.text, 'Hurricane safety', 'case kept, known words untouched');
r = await spell.suggest('TORNADOO');
assert.equal(r.text, 'TORNADO');
assert.equal(await spell.suggest('earthquake'), null, 'a known word needs nothing');
assert.equal(await spell.suggest('zzzzzzzz'), null, 'nothing close: no suggestion');
assert.equal(await spell.suggest('abc 1999 x2'), null, 'short words and numbers are left alone');
r = await spell.suggest('Ubelkeitt');
assert.equal(r.text, 'Übelkeit', 'accented display form comes back, with the capital');
r = await spell.suggest('how to survive an earhquake and hurricain');
assert.equal(r.text, 'how to survive an earthquake and hurricane');
assert.equal(r.changes.length, 2);
assert.ok(asked.every((u) => u.startsWith('/spell/')), asked.join());
assert.equal(asked.filter((u) => u === '/spell/ready_en/w9.tsv').length, 1, 'a shard is fetched once');

// No index installed: no suggestions and no errors.
const none = S.create(async () => null);
assert.equal(await none.suggest('eartquake'), null);
const broken = S.create(async () => { throw new Error('offline'); });
assert.equal(await broken.suggest('eartquake'), null);
console.log('spell.js ok');
