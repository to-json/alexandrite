// Run every acceptance case through the browser pipeline (in Node):
// compile to wasm in the alx-web module, instantiate, run, compare.
//   node test.mjs [FILTER] [--threads N]
//   node test.mjs path/to/prog.alx [--threads N]   (run one program, print its output)
// With --threads, the threaded build (pkg/mt) runs every case on N threads
// (this one plus N-1 helpers, as a cross-origin-isolated page does).
import { readFileSync, readdirSync } from 'node:fs';
import { Worker } from 'node:worker_threads';
import { programImports, runProgram } from './www/run.js';
import { Pool } from './www/pool.js';

const args = process.argv.slice(2);
const ti = args.indexOf('--threads');
const threads = ti >= 0 ? Number(args.splice(ti, 2)[1]) : 1;
const here = new URL('.', import.meta.url).pathname;
const cases = here + '../acceptance/cases/';
const pkg = here + (threads > 1 ? 'www/pkg/mt/' : 'www/pkg/');
const api = await import(pkg + 'alx_web.js');
const module = new WebAssembly.Module(readFileSync(pkg + 'alx_web_bg.wasm'));
const wasm = api.initSync({ module });
api.set_file('lib/primes/primes.alx', readFileSync(cases + 'lib/primes/primes.alx'));
for (const d of readdirSync(cases + 'pkgs')) for (const f of readdirSync(cases + 'pkgs/' + d)) api.set_file('pkgs/' + d + '/' + f, readFileSync(cases + 'pkgs/' + d + '/' + f));
api.set_file('fixtures/names.txt', readFileSync(cases + 'fixtures/names.txt'));
let pool = null;
if (threads > 1) {
  const make = () => {
    const w = new Worker(new URL('./www/thread.js', import.meta.url));
    return { post: (m) => w.postMessage(m), on: (f) => w.on('message', f), terminate: () => w.terminate() };
  };
  pool = new Pool(threads - 1, make, { pkg: pkg + 'alx_web.js', module, memory: wasm.memory });
  await pool.start();
}

export async function run(name, src) {
  const t0 = performance.now();
  let bytes;
  try { bytes = api.compile(name, src); } catch (e) { return { compileError: String(e) }; }
  const t1 = performance.now();
  const mod = new WebAssembly.Module(bytes);
  const inst = new WebAssembly.Instance(mod, programImports(mod, wasm, api));
  const t2 = performance.now();
  api.begin_run();
  let status = 0;
  let idle = null;
  try {
    await runProgram(inst.exports, api, () => { if (pool) idle = pool.join(mod); });
  } catch (e) {
    const m = String(e);
    status = m.includes('alx:exit1') ? 1 : m.includes('alx:abort') ? 'abort' : 'crash: ' + m;
  }
  const t3 = performance.now();
  if (idle) await pool.finish(idle);
  return { stdout: api.take_stdout(), stderr: api.take_stderr(), status, compileMs: t1 - t0, instMs: t2 - t1, runMs: t3 - t2 };
}

const filter = args[0] || '';
if (filter.endsWith('.alx') && filter.includes('/')) {
  const r = await run(filter.split('/').pop(), readFileSync(filter, 'utf8'));
  process.stdout.write(r.stdout ?? '');
  process.stderr.write((r.stderr ?? '') + (r.compileError ? r.compileError + '\n' : ''));
  console.error(`status ${r.status ?? 'compile error'}  run ${r.runMs?.toFixed(1)} ms`);
  process.exit(r.status === 0 ? 0 : 1);
}
let fail = 0;
// What needs the C library must be refused at compile time.
const browserless = ['ffi.alx', 'echo.alx', 'httpdemo.alx', 'scripting.alx'];
// Cases that need a file system: they run, and fail as Go's js/wasm does.
const fileless = ['wc.alx'];
for (const f of browserless.filter(f => f.includes(filter))) {
  const r = await run(f, readFileSync(cases + f, 'utf8'));
  const ok = (r.compileError ?? '').includes("aren't available in the browser") && !(r.compileError ?? '').includes('internal');
  if (!ok) fail++;
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${f} refused in the browser ${ok ? '' : JSON.stringify(r)}`);
}
for (const f of readdirSync(cases).filter(f => /^[a-z]+\d*\.alx$/.test(f) && !browserless.includes(f) && !fileless.includes(f) && readdirSync(cases).includes(f.replace('.alx', '.expected')) && f.includes(filter)).sort()) {
  const want = readFileSync(cases + f.replace('.alx', '.expected'), 'utf8').trim();
  const r = await run(f, readFileSync(cases + f, 'utf8'));
  const got = (r.stdout ?? '').trim();
  const ok = got === want && r.status === 0;
  if (!ok) fail++;
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${f.padEnd(10)} compile ${r.compileMs?.toFixed(1)} ms  inst ${r.instMs?.toFixed(1)} ms  run ${r.runMs?.toFixed(1)} ms` + (ok ? '' : `  got ${JSON.stringify(got)} ${r.compileError ?? ''} ${r.stderr ?? ''} ${r.status}`));
}
// Runtime failures.
for (const [f, status, needle] of [['wc.alx', 1, 'open wc.alx: function not implemented'], ['pe020.bad.alx', 'abort', 'overflow at pe020.bad.alx:1:'], ['pe022.missing.alx', 1, 'File.read: no such file `fixtures/nope.txt` (pe022.missing.alx:1:10)'], ['concurrency.deadlock.alx', 'abort', 'alexandrite: all tasks are asleep: deadlock']]) {
  const r = await run(f, readFileSync(cases + f, 'utf8'));
  const ok = r.status === status && r.stderr.includes(needle);
  if (!ok) fail++;
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${f} (${r.status}) ${ok ? '' : JSON.stringify(r)}`);
}
// The browser's own runtime (os, randomness): web/tests.
for (const f of readdirSync(here + 'tests').filter(f => f.endsWith('.alx') && f.includes(filter))) {
  const want = readFileSync(here + 'tests/' + f.replace('.alx', '.expected'), 'utf8');
  const r = await run(f, readFileSync(here + 'tests/' + f, 'utf8'));
  const ok = r.stdout === want && r.status === 0;
  if (!ok) fail++;
  console.log(`${ok ? 'ok  ' : 'FAIL'} tests/${f}` + (ok ? '' : `  got ${JSON.stringify(r.stdout)} ${r.compileError ?? ''} ${r.stderr ?? ''} ${r.status}`));
}
// Compile errors.
for (const f of readdirSync(cases).filter(f => f.endsWith('.expected_error'))) {
  const src = f.replace('.expected_error', '.alx');
  const want = readFileSync(cases + f, 'utf8').trim().split('\n').slice(0, 2).map(s => s.trimEnd());
  const r = await run(src, readFileSync(cases + src, 'utf8'));
  const got = (r.compileError ?? '').split('\n').slice(0, 2).map(s => s.trimEnd());
  const ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) fail++;
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${src}${ok ? '' : ' got ' + JSON.stringify(got)}`);
}
console.log(`${threads} thread${threads > 1 ? 's' : ''}: ${fail ? fail + ' failed' : 'all passed'}`);
process.exit(fail ? 1 : 0);
