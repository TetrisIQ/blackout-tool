// Evaluation of the offline assistant against the real library and a real model (opt-in, needs a
// running kiwix-serve and Ollama; changes nothing). It runs the page's own pipeline (keywords ->
// retrieve -> answer, shared code and options from assets/ai/rag.js) on test/ai_eval/questions.json.
//
//   node test/ai_eval.mjs [--model qwen3.5:4b] [--set dev|holdout] [--only id,id] [--retrieval-only] [--show] [--json out.json]
//   node test/ai_eval.mjs --verify      (checks that the library holds each case's facts; no model needed)
//
// Per question: retrieval (do the passages contain the expected facts?), answer (does the answer
// state them, and does it avoid forbidden advice?) and, for questions the library cannot answer,
// "nothing found" or an answer that says so. Exit code 1 when anything fails, except cases with a
// "known" reason (a gap of the installed library, listed but not counted).
// Environment: KIT_KIWIX (default http://127.0.0.1:6589), KIT_OLLAMA (default http://127.0.0.1:11434),
// KIT_DATA (default ~/.local/share/kit; its portal/ai/config.json names the model and the books).
import { createRequire } from 'node:module';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const R = createRequire(import.meta.url)('../assets/ai/rag.js');
const here = path.dirname(new URL(import.meta.url).pathname);

const argv = process.argv.slice(2);
const flag = (n) => argv.includes('--' + n);
const opt = (n) => (argv.includes('--' + n) ? argv[argv.indexOf('--' + n) + 1] : undefined);

const kiwix = process.env.KIT_KIWIX || 'http://127.0.0.1:6589';
const ollama = process.env.KIT_OLLAMA || 'http://127.0.0.1:11434';
const data = process.env.KIT_DATA || path.join(os.homedir(), '.local/share/kit');
const cfg = JSON.parse(fs.readFileSync(path.join(data, 'portal/ai/config.json'), 'utf8'));
const model = opt('model') || cfg.model;
const books = cfg.books || [];
const cases = JSON.parse(fs.readFileSync(path.join(here, 'ai_eval/questions.json'), 'utf8'));
const only = opt('only') ? new Set(opt('only').split(',')) : null;

const get = async (u) => {
  const r = await fetch(kiwix + u);
  if (!r.ok) throw new Error(u + ' -> ' + r.status);
  return r.text();
};
const chat = async (messages, options) => {
  const r = await fetch(ollama + '/api/chat', {
    method: 'POST',
    body: JSON.stringify({ model, messages, stream: false, think: false, keep_alive: '30m', options }),
  });
  if (!r.ok) throw new Error('ollama ' + r.status);
  return (await r.json()).message.content;
};

// A "fact" is a regular expression (case-insensitive); all of a case's facts must match.
const hasAll = (text, facts) => facts.filter((f) => !new RegExp(f, 'i').test(text));
// Forbidden advice counts only where it is not negated ("do not apply butter" is fine): sentences with a
// negation are skipped.
const NEG = /\b(not|never|avoid|don'?t|do not|nicht|nie|niemals|kein|keine|without|ohne)\b/i;
const forbidden = (text, pats) =>
  (pats || []).filter((p) => text.split(/(?<=[.!?\n])\s+/).some((s) => !NEG.test(s) && new RegExp(p, 'i').test(s)));
const REFUSAL = /(do(es)? not|don'?t|no)\b.*\b(answer|mention|specif|contain|provide|include|inform|say)|nicht|keine|nothing/i;

// --verify: does the library really hold the facts? Each answerable case names the `page` (a title, or the
// start of one) that states them; the facts are checked against that page's full text. A retrieval failure
// is only a failure of the assistant if this passes.
if (flag('verify')) {
  let bad = 0;
  for (const c of cases) {
    if (c.none || (only && !only.has(c.id))) continue;
    let state = 'ok';
    try {
      const wanted = c.page.toLowerCase();
      let text = null;
      // The search needs all words, and stems oddly ("Emergencies" does not find "Emergency"): try fewer words.
      const words = (c.find || c.page.replace(/\s*\|.*$/, '')).split(/\s+/);
      for (let n = words.length; n >= 1 && text === null; n--) {
        for (const b of books) {
          const items = R.parseSearch(await get('/search?format=xml&pageLength=40&pattern=' + encodeURIComponent(words.slice(0, n).join(' ')) + '&books.name=' + encodeURIComponent(b)));
          // translated copies share the title; the original has the shortest link
          const hit = items.filter((i) => i.title.toLowerCase().startsWith(wanted)).sort((a, b) => a.href.length - b.href.length)[0];
          if (hit) { text = R.htmlToText(await get(hit.href)); break; }
        }
      }
      if (text === null) state = 'page not found: ' + c.page;
      else { const miss = hasAll(text, c.facts); if (miss.length) state = 'facts not on the page: ' + miss.join(' & '); }
    } catch (e) { state = String(e.message || e); }
    if (state !== 'ok') bad++;
    console.log((state === 'ok' ? 'ok   ' : 'BAD  ') + c.id.padEnd(22) + (state === 'ok' ? c.page : state));
  }
  process.exit(bad === 0 ? 0 : 1);
}

const results = [];
for (const c of cases) {
  if (only && !only.has(c.id)) continue;
  if (opt('set') && (c.set || 'dev') !== opt('set')) continue;
  const t0 = Date.now();
  const res = { id: c.id, question: c.question, checks: {} };
  try {
    const kw = R.cleanKeywords(await chat(R.keywordMessages(c.question), R.OPTIONS.keywords), c.question);
    const { found } = await R.retrieve(kw, get, books, { question: c.question });
    res.keywords = kw;
    res.sources = found.map((p) => p.title);
    const text = (ps) => ps.map((p) => p.title + '\n' + p.text).join('\n');
    if (c.none) {
      // The library has nothing: no passages is the right outcome; otherwise the answer must admit it.
      res.checks.retrieval = found.length === 0 ? 'ok (nothing found)' : 'ok (passages found, the answer must admit it)';
    } else {
      const miss = hasAll(text(found), c.facts);
      res.checks.retrieval = miss.length === 0 ? 'ok' : 'missing ' + miss.join(' & ');
    }
    // The answer names its sources in a first line (SOURCES: 1 3 / none); only those passages count as used.
    // "none" or a missing line is a refusal: nothing is shown to the person.
    let given = [];
    if (!flag('retrieval-only') && found.length > 0) {
      const raw = await chat(R.buildMessages(c.question, found, []), R.OPTIONS.answer);
      const sp = R.splitSources(raw, found.length, true);
      res.format = sp.state;
      given = sp.state === 'ok' ? sp.sources.map((i) => found[i]) : [];
      res.kept = given.map((p) => p.title);
      res.holds = !c.none && hasAll(text(given), c.facts).length === 0;
      const answer = sp.state === 'ok' && given.length > 0 ? sp.body : '';
      res.raw = raw;
      if (given.length === 0) {
        // Refused (or no usable format): right for the library's gaps, a miss for the rest.
        res.checks.answer = c.none ? 'ok (not found)' : 'not found / refused';
      } else {
        res.answer = answer;
        if (c.none) {
          res.checks.answer = REFUSAL.test(answer) ? 'ok (says it cannot answer)' : 'invented an answer';
        } else {
          const miss = hasAll(answer, c.answer || c.facts);
          const bad = forbidden(answer, c.forbid);
          res.checks.answer = miss.length === 0 && bad.length === 0 ? 'ok' : [miss.length ? 'missing ' + miss.join(' & ') : '', bad.length ? 'FORBIDDEN ' + bad.join(' & ') : ''].filter(Boolean).join('; ');
          if (!res.holds) res.checks.sources = 'named sources without the facts: ' + res.kept.join(' | ');
        }
      }
    } else if (found.length === 0) {
      res.checks.answer = c.none ? 'ok (nothing found)' : 'no passages, no answer';
    }
  } catch (e) {
    res.checks.error = String(e.message || e);
  }
  // The failure that matters most: an answer was given although the passages did not hold the facts
  // (or the library has nothing): the person would act on it.
  res.unsafe = !!res.answer && !REFUSAL.test(res.answer) && !res.holds;
  res.ms = Date.now() - t0;
  const failed = Object.values(res.checks).some((v) => v === null || !String(v).startsWith('ok'));
  res.pass = !failed;
  res.known = c.known;
  results.push(res);
  console.log((res.pass ? 'PASS ' : c.known ? 'KNOWN' : 'FAIL ') + c.id.padEnd(22) + ' ' + ((res.ms / 1000).toFixed(1) + 's').padStart(6) + '  kw: ' + (res.keywords || '-'));
  if (res.unsafe) console.log('       UNSAFE: answered without the facts being in the passages');
  if (!res.pass) for (const [k, v] of Object.entries(res.checks)) if (v === null || !String(v).startsWith('ok')) console.log('       ' + k + ': ' + v + '  [' + (res.sources || []).join(' | ') + ']');
  if (flag('show') && res.answer) console.log('       > ' + res.answer.replace(/\n+/g, ' ').slice(0, 600));
}

const passed = results.filter((r) => r.pass).length;
const unexpected = results.filter((r) => !r.pass && !r.known).length;
console.log(`\n${model}: ${passed} of ${results.length} questions pass`);
console.log('retrieval ok: ' + results.filter((r) => String(r.checks.retrieval || '').startsWith('ok')).length +
  ', answers ok: ' + results.filter((r) => String(r.checks.answer || '').startsWith('ok')).length);
if (opt('json')) fs.writeFileSync(opt('json'), JSON.stringify({ model, books, results }, null, 2));
const bySet = (n) => results.filter((r) => (cases.find((c) => c.id === r.id).set || 'dev') === n);
for (const n of ['dev', 'holdout']) if (bySet(n).length) console.log(`${n}: ${bySet(n).filter((r) => r.pass).length} of ${bySet(n).length} pass, ${bySet(n).filter((r) => r.unsafe).length} unsafe answers`);
console.log('unsafe answers (answered without the facts in the passages): ' + results.filter((r) => r.unsafe).length);
console.log('known gaps (not counted): ' + results.filter((r) => !r.pass && r.known).length);
process.exit(unexpected === 0 ? 0 : 1);
