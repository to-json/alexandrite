// Compiles and runs programs off the main thread, so a long or endless
// program never freezes the page (Stop terminates this worker).
//
// alx_web_bg.wasm holds the whole compiler (front end, checker, wasm
// backend) and the runtime. compile() returns a program module whose
// imports are this module's memory and its alxr_* runtime exports; calls
// between the two are direct wasm-to-wasm calls.
//
// On a cross-origin-isolated page the threaded build (pkg/mt) is used: its
// memory is shared with helper threads (thread.js), and spawned tasks run
// in parallel on all of them. Elsewhere tasks take turns on this thread.
import { programImports, runProgram } from './run.js';
import { Pool } from './pool.js';
import { FILES } from './examples.js';

const threaded = self.crossOriginIsolated === true && typeof SharedArrayBuffer !== 'undefined';
const pkg = threaded ? './pkg/mt/alx_web.js' : './pkg/alx_web.js';
const wasmUrl = threaded ? './pkg/mt/alx_web_bg.wasm' : './pkg/alx_web_bg.wasm';

let api, wasm, pool;
const enc = new TextEncoder();

const ready = (async () => {
  api = await import(pkg);
  const module = await WebAssembly.compileStreaming(fetch(new URL(wasmUrl, import.meta.url)));
  wasm = await api.default({ module_or_path: module });
  for (const [path, text] of Object.entries(FILES)) api.set_file(path, enc.encode(text));
  const names = await fetch(new URL('./fixtures/names.txt', import.meta.url));
  api.set_file('fixtures/names.txt', new Uint8Array(await names.arrayBuffer()));
  if (threaded) {
    const n = Math.max(1, Math.min((navigator.hardwareConcurrency || 4) - 1, 7));
    const make = () => {
      const w = new Worker(new URL('./thread.js', import.meta.url), { type: 'module' });
      return { post: (m) => w.postMessage(m), on: (f) => (w.onmessage = (e) => f(e.data)), terminate: () => w.terminate() };
    };
    pool = new Pool(n, make, { pkg: new URL(pkg, import.meta.url).href, module, memory: wasm.memory });
    await pool.start();
  }
  postMessage({ type: 'ready', threads: pool ? pool.n + 1 : 1 });
})().catch((e) => postMessage({ type: 'fatal', message: String(e) }));

onmessage = async ({ data }) => {
  await ready;
  const { name, source } = data;
  const t0 = performance.now();
  let bytes;
  try {
    bytes = api.compile(name, source);
  } catch (e) {
    const internal = !(typeof e === 'string');
    postMessage({ type: 'compile-error', message: internal ? `internal compiler error: ${e}` : e, compileMs: performance.now() - t0, fatal: internal });
    return;
  }
  const t1 = performance.now();
  let inst, mod;
  try {
    mod = await WebAssembly.compile(bytes);
    inst = await WebAssembly.instantiate(mod, programImports(mod, wasm, api));
  } catch (e) {
    postMessage({ type: 'compile-error', message: `internal compiler error: ${e}`, compileMs: t1 - t0, fatal: true });
    return;
  }
  const t2 = performance.now();
  api.begin_run();
  let status = 0;
  let idle = null;
  try {
    await runProgram(inst.exports, api, () => {
      if (pool) idle = pool.join(mod);
    });
  } catch (e) {
    const m = String(e);
    status = m.includes('alx:exit1') ? 1 : m.includes('alx:abort') ? 'abort' : `crash: ${m}`;
  }
  const t3 = performance.now();
  if (idle) await pool.finish(idle);
  postMessage({
    type: 'result',
    stdout: api.take_stdout(),
    stderr: api.take_stderr(),
    status,
    compileMs: t1 - t0,
    instMs: t2 - t1,
    runMs: t3 - t2,
    wasmBytes: bytes.length,
    fatal: typeof status === 'string',
  });
};
