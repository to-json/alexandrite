// Compiles and runs programs off the main thread, so a long or endless
// program never freezes the page (Stop terminates this worker).
//
// alx_web_bg.wasm holds the whole compiler (front end, checker, wasm
// backend) and the runtime. compile() returns a program module whose
// imports are this module's memory and its alxr_* runtime exports; calls
// between the two are direct wasm-to-wasm calls.
import init, { compile, set_file, begin_run, take_stdout, take_stderr } from './pkg/alx_web.js';
import { FILES } from './examples.js';

let wasm;
const enc = new TextEncoder();

const ready = (async () => {
  wasm = await init({ module_or_path: new URL('./pkg/alx_web_bg.wasm', import.meta.url) });
  for (const [path, text] of Object.entries(FILES)) set_file(path, enc.encode(text));
  const names = await fetch(new URL('./fixtures/names.txt', import.meta.url));
  set_file('fixtures/names.txt', new Uint8Array(await names.arrayBuffer()));
  postMessage({ type: 'ready' });
})().catch((e) => postMessage({ type: 'fatal', message: String(e) }));

onmessage = async ({ data }) => {
  await ready;
  const { name, source } = data;
  const t0 = performance.now();
  let bytes;
  try {
    bytes = compile(name, source);
  } catch (e) {
    const internal = !(typeof e === 'string');
    postMessage({ type: 'compile-error', message: internal ? `internal compiler error: ${e}` : e, compileMs: performance.now() - t0, fatal: internal });
    return;
  }
  const t1 = performance.now();
  let inst;
  try {
    const mod = await WebAssembly.compile(bytes);
    const rt = {};
    for (const imp of WebAssembly.Module.imports(mod)) {
      if (imp.module === 'rt') rt[imp.name] = wasm[imp.name];
    }
    inst = await WebAssembly.instantiate(mod, { env: { memory: wasm.memory }, rt });
  } catch (e) {
    postMessage({ type: 'compile-error', message: `internal compiler error: ${e}`, compileMs: t1 - t0, fatal: true });
    return;
  }
  const t2 = performance.now();
  begin_run();
  let status = 0;
  try {
    inst.exports.main();
  } catch (e) {
    const m = String(e);
    status = m.includes('alx:exit1') ? 1 : m.includes('alx:abort') ? 'abort' : `crash: ${m}`;
  }
  const t3 = performance.now();
  postMessage({
    type: 'result',
    stdout: take_stdout(),
    stderr: take_stderr(),
    status,
    compileMs: t1 - t0,
    instMs: t2 - t1,
    runMs: t3 - t2,
    wasmBytes: bytes.length,
    fatal: typeof status === 'string',
  });
};
