import { EXAMPLES } from './www/examples.js';

const w = new Worker(new URL('./www/worker.js', import.meta.url), { type: 'module' });
const next = () => new Promise((r) => { w.onmessage = ({ data }) => r(data); });
const out = [];
const pre = document.getElementById('bench');
w.onerror = (e) => { pre.textContent = 'worker error: ' + (e.message || e); };
const first = await next(); // ready
pre.textContent = 'worker: ' + first.type;
for (const round of [1, 2, 3]) {
  for (const ex of EXAMPLES.filter((e) => e.group === 'project euler')) {
    w.postMessage({ name: `${ex.id}.alx`, source: ex.source });
    const r = await next();
    pre.textContent = `round ${round} ${ex.id} ${r.type}`;
    if (round === 3) out.push(`${ex.id}\t${(r.stdout || r.message || '').trim()}\tcompile ${r.compileMs.toFixed(2)}\tinst ${r.instMs?.toFixed(2)}\trun ${r.runMs?.toFixed(1)}`);
  }
}
document.getElementById('bench').textContent = out.join('\n');
