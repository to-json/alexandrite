// Helper threads for the thread that compiles and runs programs (see
// thread.js). `make()` starts one: { post(msg), on(fn), terminate() }.
export class Pool {
  constructor(n, make, init) {
    this.n = n;
    this.make = make;
    this.init = init; // { pkg, module, memory }
    this.threads = [];
  }

  async spawn() {
    const t = this.make();
    const ready = new Promise((r) => {
      t.on((m) => {
        if (m.type === 'ready') r();
        else if (m.type === 'idle') t.idle?.();
      });
    });
    t.post({ type: 'init', ...this.init });
    await ready;
    return t;
  }

  async start() {
    this.threads = await Promise.all(Array.from({ length: this.n }, () => this.spawn()));
  }

  // Every helper joins the run of `prog` (a WebAssembly.Module). Returns
  // what `finish` waits on.
  join(prog) {
    const idle = this.threads.map((t) => new Promise((r) => (t.idle = r)));
    for (const t of this.threads) t.post({ type: 'run', prog });
    return idle;
  }

  // After the run: wait for each helper to be idle again. One still busy
  // `grace` ms later (a task that never blocks, say) is replaced, so the
  // next run starts clean.
  async finish(idle, grace = 200) {
    await Promise.all(
      idle.map(async (p, i) => {
        const done = await Promise.race([p.then(() => true), new Promise((r) => setTimeout(() => r(false), grace))]);
        if (!done) {
          this.threads[i].terminate();
          this.threads[i] = await this.spawn();
        }
      }),
    );
  }
}
