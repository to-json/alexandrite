// Run every acceptance case through the browser pipeline (in Node):
// compile to wasm in the alx-web module, instantiate, run, compare.
import { readFileSync, readdirSync } from 'node:fs';
import { initSync, compile, set_file, begin_run, take_stdout, take_stderr } from './www/pkg/alx_web.js';

const here = new URL('.', import.meta.url).pathname;
const cases = here + '../acceptance/cases/';
const wasm = initSync({ module: readFileSync(here + 'www/pkg/alx_web_bg.wasm') });
set_file('lib/primes/primes.alx', readFileSync(cases + 'lib/primes/primes.alx'));
set_file('fixtures/names.txt', readFileSync(cases + 'fixtures/names.txt'));

export function run(name, src) {
  const t0 = performance.now();
  let bytes;
  try { bytes = compile(name, src); } catch (e) { return { compileError: String(e) }; }
  const t1 = performance.now();
  const mod = new WebAssembly.Module(bytes);
  const rt = {};
  for (const imp of WebAssembly.Module.imports(mod)) if (imp.module === 'rt') rt[imp.name] = wasm[imp.name];
  const inst = new WebAssembly.Instance(mod, { env: { memory: wasm.memory }, rt });
  const t2 = performance.now();
  begin_run();
  let status = 0;
  try { inst.exports.main(); } catch (e) {
    const m = String(e);
    status = m.includes('alx:exit1') ? 1 : m.includes('alx:abort') ? 'abort' : 'crash: ' + m;
  }
  const t3 = performance.now();
  return { stdout: take_stdout(), stderr: take_stderr(), status, compileMs: t1 - t0, instMs: t2 - t1, runMs: t3 - t2 };
}

const filter = process.argv[2] || '';
let fail = 0;
for (const f of readdirSync(cases).filter(f => /^[a-z]+\d*\.alx$/.test(f) && readdirSync(cases).includes(f.replace('.alx', '.expected')) && f.includes(filter)).sort()) {
  const want = readFileSync(cases + f.replace('.alx', '.expected'), 'utf8').trim();
  const r = run(f, readFileSync(cases + f, 'utf8'));
  const got = (r.stdout ?? '').trim();
  const ok = got === want && r.status === 0;
  if (!ok) fail++;
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${f.padEnd(10)} compile ${r.compileMs?.toFixed(1)} ms  inst ${r.instMs?.toFixed(1)} ms  run ${r.runMs?.toFixed(1)} ms` + (ok ? '' : `  got ${JSON.stringify(got)} ${r.compileError ?? ''} ${r.stderr ?? ''} ${r.status}`));
}
// Runtime failures.
for (const [f, status, needle] of [['pe020.bad.alx', 'abort', 'overflow at pe020.bad.alx:1:'], ['pe022.missing.alx', 1, 'File.read: no such file `fixtures/nope.txt` (pe022.missing.alx:1:10)']]) {
  const r = run(f, readFileSync(cases + f, 'utf8'));
  const ok = r.status === status && r.stderr.includes(needle);
  if (!ok) fail++;
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${f} (${r.status}) ${ok ? '' : JSON.stringify(r)}`);
}
// Compile errors.
for (const f of readdirSync(cases).filter(f => f.endsWith('.expected_error'))) {
  const src = f.replace('.expected_error', '.alx');
  const want = readFileSync(cases + f, 'utf8').trim().split('\n').slice(0, 2).map(s => s.trimEnd());
  const r = run(src, readFileSync(cases + src, 'utf8'));
  const got = (r.compileError ?? '').split('\n').slice(0, 2).map(s => s.trimEnd());
  const ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) fail++;
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${src}${ok ? '' : ' got ' + JSON.stringify(got)}`);
}
process.exit(fail ? 1 : 0);
