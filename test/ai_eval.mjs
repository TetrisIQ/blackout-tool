// Evaluation of the offline assistant against the real library and a real model (opt-in, needs a
// running kiwix-serve and Ollama; changes nothing). It runs the page's own pipeline (keywords ->
// retrieve -> answer, shared code and options from assets/ai/rag.js) on test/ai_eval/questions.json.
//
//   node test/ai_eval.mjs [--model qwen3.5:4b] [--only id,id] [--retrieval-only] [--show] [--json out.json]
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

const results = [];
for (const c of cases) {
  if (only && !only.has(c.id)) continue;
  const t0 = Date.now();
  const res = { id: c.id, question: c.question, checks: {} };
  try {
    const kw = R.cleanKeywords(await chat(R.keywordMessages(c.question), R.OPTIONS.keywords), c.question);
    const { found } = await R.retrieve(kw, get, books, { question: c.question });
    res.keywords = kw;
    res.sources = found.map((p) => p.title);
    const passages = found.map((p) => p.title + '\n' + p.text).join('\n');
    if (c.none) {
      // The library has nothing: no passages is the right outcome; otherwise the answer must admit it.
      res.checks.retrieval = found.length === 0 ? 'ok (nothing found)' : 'ok (passages found, the answer must admit it)';
    } else {
      const miss = hasAll(passages, c.facts);
      res.checks.retrieval = miss.length === 0 ? 'ok' : 'missing ' + miss.join(' & ');
    }
    if (!flag('retrieval-only') && found.length > 0) {
      const answer = await chat(R.buildMessages(c.question, found, []), R.OPTIONS.answer);
      res.answer = answer;
      if (c.none) {
        res.checks.answer = REFUSAL.test(answer) ? 'ok (says it cannot answer)' : 'invented an answer';
      } else {
        const miss = hasAll(answer, c.answer || c.facts);
        const bad = forbidden(answer, c.forbid);
        res.checks.answer = miss.length === 0 && bad.length === 0 ? 'ok' : [miss.length ? 'missing ' + miss.join(' & ') : '', bad.length ? 'FORBIDDEN ' + bad.join(' & ') : ''].filter(Boolean).join('; ');
      }
    } else if (found.length === 0 && !c.none) {
      res.checks.answer = 'no passages, no answer';
    }
  } catch (e) {
    res.checks.error = String(e.message || e);
  }
  res.ms = Date.now() - t0;
  const failed = Object.values(res.checks).some((v) => v === null || !String(v).startsWith('ok'));
  res.pass = !failed;
  res.known = c.known;
  results.push(res);
  console.log((res.pass ? 'PASS ' : c.known ? 'KNOWN' : 'FAIL ') + c.id.padEnd(22) + ' ' + ((res.ms / 1000).toFixed(1) + 's').padStart(6) + '  kw: ' + (res.keywords || '-'));
  if (!res.pass) for (const [k, v] of Object.entries(res.checks)) if (v === null || !String(v).startsWith('ok')) console.log('       ' + k + ': ' + v + '  [' + (res.sources || []).join(' | ') + ']');
  if (flag('show') && res.answer) console.log('       > ' + res.answer.replace(/\n+/g, ' ').slice(0, 600));
}

const passed = results.filter((r) => r.pass).length;
const unexpected = results.filter((r) => !r.pass && !r.known).length;
console.log(`\n${model}: ${passed} of ${results.length} questions pass`);
console.log('retrieval ok: ' + results.filter((r) => String(r.checks.retrieval || '').startsWith('ok')).length +
  ', answers ok: ' + results.filter((r) => String(r.checks.answer || '').startsWith('ok')).length);
if (opt('json')) fs.writeFileSync(opt('json'), JSON.stringify({ model, books, results }, null, 2));
console.log('known gaps (not counted): ' + results.filter((r) => !r.pass && r.known).length);
process.exit(unexpected === 0 ? 0 : 1);
