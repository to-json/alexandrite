// Running a compiled program, on one thread or on several.
//
// A program that spawns or blocks exports run_task: its tasks are run by
// a loop on every thread (runTasks) that asks the runtime for the next
// task (sched.rs). Without threads, a run whose tasks are all asleep
// waits on a timer here; with them, the runtime blocks the thread instead.

// The imports of a program module instantiated on this thread: the
// runtime's memory and functions, and this thread's RET address.
export function programImports(mod, wasm, api) {
  const rt = { ret: new WebAssembly.Global({ value: 'i32', mutable: false }, api.ret_address()) };
  for (const imp of WebAssembly.Module.imports(mod)) {
    if (imp.module === 'rt' && imp.kind === 'function') rt[imp.name] = wasm[imp.name];
  }
  return { env: { memory: wasm.memory }, rt };
}

// Run tasks on this thread until the run is over.
export async function runTasks(exports, api) {
  for (;;) {
    const t = api.sched_next();
    if (t === -1) return;
    if (t === -3) {
      await new Promise((r) => setTimeout(r, api.sched_sleep_ms()));
      continue;
    }
    if (t === -4) {
      // Help a pmap that's waiting on its elements.
      try {
        exports.run_pmap();
      } catch (e) {
        const m = String(e);
        if (m.includes('alx:pmap')) api.pmap_failed();
        else api.run_failed(3, m);
      }
      continue;
    }
    try {
      exports.run_task(t);
    } catch (e) {
      const m = String(e);
      // A panic in a spawned task ends only that task.
      if (m.includes('alx:task')) api.task_failed();
      else api.run_failed(m.includes('alx:abort') ? 1 : m.includes('alx:exit1') ? 2 : 3, m);
    }
  }
}

// Run a program to the end. `started` is called once the run's tasks can
// be taken (helper threads join then). Throws what the program threw
// (alx:abort, alx:exit1, a crash).
export async function runProgram(exports, api, started = () => {}) {
  if (!exports.run_task) {
    exports.main();
    return;
  }
  api.sched_start();
  started();
  await runTasks(exports, api);
  const o = api.run_outcome();
  if (o === 1) throw new Error('alx:abort');
  if (o === 2) throw new Error('alx:exit1');
  if (o === 3) throw new Error(api.run_crash_message());
}
