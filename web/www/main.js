import { EXAMPLES, DEFAULT } from './examples.js';

const $ = (id) => document.getElementById(id);
const editor = $('editor');
const highlightLayer = $('highlightLayer');
const lineNumbers = $('lineNumbers');
const output = $('output');
const runBtn = $('runBtn');
const stopBtn = $('stopBtn');
const status = $('status');
const loading = $('loading');
const examples = $('examples');
const fileName = $('fileName');
const divider = $('divider');
const editorPanel = $('editorPanel');
const outputPanel = $('outputPanel');

let current = EXAMPLES.find((e) => e.id === DEFAULT) ?? EXAMPLES[0];

// ── Hue ──

$('hue').oninput = (e) => document.documentElement.style.setProperty('--hue', e.target.value);

// ── Examples ──

for (const group of [...new Set(EXAMPLES.map((e) => e.group))]) {
  const og = document.createElement('optgroup');
  og.label = group;
  for (const ex of EXAMPLES.filter((e) => e.group === group)) {
    const o = document.createElement('option');
    o.value = ex.id;
    o.textContent = ex.title;
    og.appendChild(o);
  }
  examples.appendChild(og);
}

function load(ex) {
  current = ex;
  examples.value = ex.id;
  editor.value = ex.source;
  fileName.textContent = `${ex.id}.alx`;
  syncEditor();
}

examples.addEventListener('change', () => load(EXAMPLES.find((e) => e.id === examples.value)));

// ── Syntax highlighting (DOM nodes only, never innerHTML) ──

const KEYWORDS = new Set([
  'def', 'if', 'unless', 'else', 'elsif', 'while', 'until', 'loop', 'next', 'break', 'return',
  'try', 'require', 'puts', 'it', 'and', 'or', 'not', 'nil', 'self', 'do', 'end',
]);
const BOOLEANS = new Set(['true', 'false']);

function tokens(src) {
  const out = [];
  const push = (cls, text) => out.push([cls, text]);
  let i = 0;
  const n = src.length;
  let heredoc = null;
  while (i < n) {
    const ch = src[i];
    const atLineStart = i === 0 || src[i - 1] === '\n';
    // Heredoc body: everything up to the terminator line.
    if (heredoc && atLineStart) {
      const re = new RegExp(`^[ \\t]*${heredoc}[ \\t]*$`, 'm');
      const m = re.exec(src.slice(i));
      const end = m ? i + m.index + m[0].length : n;
      push('hl-string', src.slice(i, end));
      i = end;
      heredoc = null;
      continue;
    }
    if (ch === '#' && (src[i + 1] === '[' || (src[i + 1] === '!' && src[i + 2] === '['))) {
      const end = src.indexOf(']', i);
      const j = end === -1 ? n : end + 1;
      push('hl-attr', src.slice(i, j));
      i = j;
      continue;
    }
    if (ch === '#') {
      let end = src.indexOf('\n', i);
      if (end === -1) end = n;
      push('hl-comment', src.slice(i, end));
      i = end;
      continue;
    }
    if (ch === '"' || ch === "'") {
      let j = i + 1;
      while (j < n && src[j] !== ch && src[j] !== '\n') {
        if (src[j] === '\\') j++;
        j++;
      }
      if (j < n && src[j] === ch) j++;
      push('hl-string', src.slice(i, j));
      i = j;
      continue;
    }
    const hd = /^<<[~-]?([A-Z_]+)/.exec(src.slice(i, i + 40));
    if (hd) {
      push('hl-string', hd[0]);
      heredoc = hd[1];
      i += hd[0].length;
      continue;
    }
    if (ch === ':' && /[A-Za-z_]/.test(src[i + 1] ?? '') && src[i - 1] !== ':') {
      const m = /^:[A-Za-z_][A-Za-z0-9_?!]*/.exec(src.slice(i));
      push('hl-symbol', m[0]);
      i += m[0].length;
      continue;
    }
    if (/[0-9]/.test(ch)) {
      const m = /^[0-9][0-9_]*/.exec(src.slice(i));
      push('hl-number', m[0]);
      i += m[0].length;
      continue;
    }
    if (/[A-Za-z_]/.test(ch)) {
      const m = /^[A-Za-z_][A-Za-z0-9_]*[?!]?/.exec(src.slice(i));
      const w = m[0];
      const cls = KEYWORDS.has(w) ? 'hl-keyword' : BOOLEANS.has(w) ? 'hl-boolean' : /^[A-Z]/.test(w) ? 'hl-type' : 'hl-plain';
      push(cls, w);
      i += w.length;
      continue;
    }
    if ('(){}[]|'.includes(ch)) {
      push('hl-paren', ch);
      i++;
      continue;
    }
    // Operators and punctuation (whitespace stays a bare text node).
    push(/\s/.test(ch) ? null : 'hl-plain', ch);
    i++;
  }
  return out;
}

function syncEditor() {
  const frag = document.createDocumentFragment();
  for (const [cls, text] of tokens(editor.value)) {
    if (cls) {
      const s = document.createElement('span');
      s.className = cls;
      s.textContent = text;
      frag.appendChild(s);
    } else {
      frag.appendChild(document.createTextNode(text));
    }
  }
  frag.appendChild(document.createTextNode('\n'));
  highlightLayer.replaceChildren(frag);

  const count = editor.value.split('\n').length;
  if (lineNumbers.childElementCount !== count) {
    const nums = document.createDocumentFragment();
    for (let k = 1; k <= count; k++) {
      const s = document.createElement('span');
      s.textContent = String(k);
      nums.appendChild(s);
    }
    lineNumbers.replaceChildren(nums);
  }
}

editor.addEventListener('input', syncEditor);
editor.addEventListener('keydown', (e) => {
  if (e.key === 'Tab') {
    e.preventDefault();
    const { selectionStart: a, selectionEnd: b } = editor;
    editor.value = editor.value.slice(0, a) + '  ' + editor.value.slice(b);
    editor.selectionStart = editor.selectionEnd = a + 2;
    syncEditor();
  }
});

// ── Worker ──

let worker = null;
let running = false;
let stopTimer = 0;
let workerReady = null;

function spawn() {
  worker?.terminate();
  worker = new Worker(new URL('./worker.js', import.meta.url), { type: 'module' });
  workerReady = new Promise((resolve, reject) => {
    worker.onmessage = ({ data }) => {
      if (data.type === 'ready') resolve();
      else if (data.type === 'fatal') reject(new Error(data.message));
      else finish(data);
    };
    worker.onerror = (e) => reject(e);
  });
  return workerReady;
}

function setStatus(text, cls) {
  status.textContent = text;
  status.className = `status ${cls}`;
}

const ms = (x) => (x < 1 ? x.toFixed(2) : x < 100 ? x.toFixed(1) : x.toFixed(0)) + ' ms';
const kb = (n) => (n < 1024 ? `${n} B` : `${(n / 1024).toFixed(1)} KB`);

function showOutput(nodes, cls, meta) {
  output.className = `output-content${cls ? ' ' + cls : ''}`;
  output.replaceChildren(...nodes);
  for (const m of output.parentElement.querySelectorAll('.output-meta')) m.remove();
  if (meta) output.parentElement.appendChild(meta);
}

function metaLine(parts) {
  const d = document.createElement('div');
  d.className = 'output-meta';
  parts.forEach(([text, fast], k) => {
    if (k) d.appendChild(document.createTextNode(' · '));
    const s = document.createElement('span');
    if (fast) s.className = 'fast';
    s.textContent = text;
    d.appendChild(s);
  });
  return d;
}

function finish(r) {
  clearTimeout(stopTimer);
  running = false;
  stopBtn.hidden = true;
  runBtn.disabled = false;
  if (r.type === 'compile-error') {
    showOutput([document.createTextNode(r.message)], 'error', metaLine([[`rejected in ${ms(r.compileMs)}`, true]]));
    setStatus('compile error', 'error');
  } else {
    const nodes = [];
    if (r.stdout) nodes.push(document.createTextNode(r.stdout));
    if (r.stderr) {
      const s = document.createElement('span');
      s.className = 'output-stderr';
      s.textContent = r.stderr;
      nodes.push(s);
    }
    const ok = r.status === 0;
    if (!nodes.length) nodes.push(document.createTextNode('(no output)'));
    showOutput(nodes, nodes.length === 1 && !r.stdout && !r.stderr ? 'empty' : '', metaLine([
      [`compiled in ${ms(r.compileMs)} → ${kb(r.wasmBytes)} of wasm`, true],
      [`instantiated in ${ms(r.instMs)}`],
      [`ran in ${ms(r.runMs)}`, true],
      ...(ok ? [] : [[r.status === 'abort' ? 'aborted' : r.status === 1 ? 'exit 1' : String(r.status)]]),
    ]));
    setStatus(ok ? 'done' : 'error', ok ? 'ready' : 'error');
  }
  if (r.fatal) spawn();
}

async function run() {
  if (running) return;
  running = true;
  runBtn.disabled = true;
  setStatus('compiling', 'running');
  stopTimer = setTimeout(() => {
    if (running) {
      stopBtn.hidden = false;
      setStatus('running', 'running');
    }
  }, 150);
  await workerReady;
  worker.postMessage({ name: `${current.id}.alx`, source: editor.value });
}

async function stop() {
  if (!running) return;
  clearTimeout(stopTimer);
  running = false;
  stopBtn.hidden = true;
  setStatus('stopped', 'error');
  showOutput([document.createTextNode('stopped')], 'empty', null);
  await spawn();
  runBtn.disabled = false;
  setStatus('ready', 'ready');
}

runBtn.addEventListener('click', run);
stopBtn.addEventListener('click', stop);
document.addEventListener('keydown', (e) => {
  if ((e.metaKey || e.ctrlKey) && e.key === 'Enter') {
    e.preventDefault();
    run();
  } else if (e.key === 'Escape' && running) {
    stop();
  }
});

// ── Resizable divider ──

let dragging = false;
divider.addEventListener('mousedown', (e) => {
  e.preventDefault();
  dragging = true;
  divider.classList.add('active');
  document.body.style.cursor = 'col-resize';
  document.body.style.userSelect = 'none';
});
document.addEventListener('mousemove', (e) => {
  if (!dragging) return;
  const rect = editorPanel.parentElement.getBoundingClientRect();
  const pct = Math.max(20, Math.min(80, ((e.clientX - rect.left) / rect.width) * 100));
  editorPanel.style.flex = `0 0 ${pct}%`;
  outputPanel.style.flex = '1';
});
document.addEventListener('mouseup', () => {
  if (!dragging) return;
  dragging = false;
  divider.classList.remove('active');
  document.body.style.cursor = '';
  document.body.style.userSelect = '';
});

// ── Init ──

load(current);
try {
  await spawn();
  loading.classList.add('hidden');
  setStatus('ready', 'ready');
  runBtn.disabled = false;
} catch (e) {
  loading.querySelector('.loading-text').textContent = `failed to load: ${e.message ?? e}`;
  setStatus('error', 'error');
}
