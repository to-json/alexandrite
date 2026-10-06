// A helper thread of a cross-origin-isolated page (or a Node test): it
// shares the runtime's memory with the thread that compiles, instantiates
// each program on its own, and takes tasks until the run is over.
import { programImports, runTasks } from './run.js';

let post, listen;
if (typeof self !== 'undefined' && typeof self.postMessage === 'function') {
  post = (m) => self.postMessage(m);
  listen = (f) => (self.onmessage = (e) => f(e.data));
} else {
  const { parentPort } = await import('node:worker_threads');
  post = (m) => parentPort.postMessage(m);
  listen = (f) => parentPort.on('message', f);
}

let api, wasm;
listen(async (msg) => {
  if (msg.type === 'init') {
    api = await import(msg.pkg);
    wasm = api.initSync({ module: msg.module, memory: msg.memory });
    post({ type: 'ready' });
  } else if (msg.type === 'run') {
    api.thread_reset();
    const inst = new WebAssembly.Instance(msg.prog, programImports(msg.prog, wasm, api));
    await runTasks(inst.exports, api);
    post({ type: 'idle' });
  }
});
