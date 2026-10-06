// Generate www/examples.js from the acceptance cases (the page's examples
// are exactly the programs the compiler is tested on), and copy fixtures.
import { readFileSync, writeFileSync, mkdirSync, copyFileSync } from 'node:fs';

const here = new URL('.', import.meta.url).pathname;
const cases = here + '../acceptance/cases/';
const read = (f) => readFileSync(cases + f, 'utf8');

const PE = [
  ['pe001', 'multiples of 3 or 5', 'Enumerable is the whole program: a range, a block, `it`.'],
  ['pe002', 'even Fibonacci numbers', 'An infinite generator, made finite by `.lazy.take_while`.'],
  ['pe004', 'largest palindrome product', 'A #[pure] helper; 405,450 products checked.'],
  ['pe006', 'sum square difference', ''],
  ['pe007', '10001st prime', 'lib/primes/primes.alx (bundled) is an infinite generator of primes by trial division.'],
  ['pe008', 'largest product in a series', 'A heredoc, `chars`, `each_cons`.'],
  ['pe010', 'summation of primes', 'A sieve of 2,000,000 booleans. Index checks are proven away.'],
  ['pe014', 'longest Collatz sequence', '`pmap` over a million starts (sequential in the browser). `~(3 * n + 1)` because 3n + 1 can overflow.'],
  ['pe016', 'power digit sum', '#![overflow(promote)]: Ints become bignums instead of overflowing.'],
  ['pe020', 'factorial digit sum', '100! with promotion.'],
  ['pe022', 'names scores', 'File.read of a bundled 46 KB fixture; `sort`, `each_with_index`, `bytes`.'],
  ['pe025', '1000-digit Fibonacci number', 'Bignums in a loop; `to_s.size` is a digit count, no string built.'],
];
const BENCHMARKS = [
  ['nbody', 'n-body', 'The Benchmarks Game classic: structs, Floats, 1,000,000 steps of a 5-body orbit.'],
  ['bintrees', 'binary trees', 'Another Benchmarks Game classic: @Node handles into pools, short-lived trees freed a pool at a time.'],
  ['wordfreq', 'word frequencies', 'A Map[Str, Int] over 1,000,000 words.'],
  ['strchurn', 'string churn', '200,000 interpolated lines built, joined and split again.'],
];
const TOUR = [
  ['ints', 'numbers', 'Go\'s integer types, exact constants, bit operations.'],
  ['syntax', 'everyday syntax', 'for-in, case, if expressions, interpolation, defer.'],
  ['collections', 'collections', 'Slices and fixed arrays, byte strings, ordered maps, T?.'],
  ['types', 'types', 'Methods, operators, enums, interfaces, generics, lambdas.'],
  ['concurrency', 'tasks and channels', 'spawn, wait, Chan, select; a panic ends only its task.'],
  ['sync', 'Mutex and Atomic', 'A Mutex guards a value shared between tasks; atomic cells.'],
  ['tuples', 'tuples', 'Multiple results as values.'],
  ['closures', 'closures', 'Lambdas capture variables, not copies.'],
  ['funcvalues', 'function values', 'Named defs and blocks where a function is wanted.'],
  ['pools', 'pools', 'Recursive data through @T handles: a binary search tree.'],
  ['fmt', 'fmt', 'Go\'s format verbs, flags, widths, errorf.'],
  ['math', 'math', 'The math package.'],
  ['jsondemo', 'json', 'encoding/json on compile-time derives: no reflection.'],
  ['pure', 'purity', 'Pure code is proven safe or explicitly fallible.'],
  ['errors', 'errors', '`error` types, `fail`, `~` propagation, declared error sets, Results.'],
  ['packages', 'packages', 'Directory packages (bundled under pkgs/), `pub`, refinements across packages.'],
  ['refinements', 'refinements', 'Methods on existing types, active only under `using`.'],
];
const LEARN = [
  ['learnruby', 'Ruby', 'Learn Ruby in Y minutes, the parts that carry over.'],
];
const MISTAKES = [
  ['pe001.typo', 'a typo', 'Did-you-mean suggestions name the line and column.'],
  ['pe004.bad', 'a type error', '`palindrome?` takes an Int, not a Str.'],
  ['pe014.bad1', 'I/O in a #[pure] function', 'Purity is checked: pure functions can run in parallel.'],
  ['jsondemo.bad1', 'an underivable field', '#[derive(Json)] needs every field type to derive Json too.'],
  ['pe014.bad2', 'unproven arithmetic', 'A #[pure] function must prove 3 * n + 1 fits in 64 bits, or check it with `~(...)`.'],
  ['pe020.bad', 'overflow at run time', 'Without #![overflow(promote)], 100! overflows 64 bits: a panic naming the line.'],
];

const ex = [];
for (const [id, title, note] of PE) {
  const n = Number(id.slice(2));
  const head = `# Project Euler ${n}: ${title}` + (note ? `\n# ${note}` : '');
  ex.push({ id, group: 'project euler', title: `${n}. ${title}`, source: `${head}\n${read(id + '.alx')}` });
}
for (const [id, title, note] of BENCHMARKS) {
  ex.push({ id, group: 'benchmarks game', title, source: `# ${note}\n${read(id + '.alx')}` });
}
for (const [id, title, note] of TOUR) {
  ex.push({ id, group: 'language tour', title, source: `# ${note}\n${read(id + '.alx')}` });
}
for (const [id, title, note] of LEARN) {
  ex.push({ id, group: 'learn x in y minutes', title, source: read(id + '.alx') });
}
for (const [id, title, note] of MISTAKES) {
  ex.push({ id, group: 'mistakes', title, source: `# Mistake: ${title}. ${note}\n${read(id + '.alx')}` });
}
const files = { 'lib/primes/primes.alx': read('lib/primes/primes.alx') };
for (const f of ['pkgs/geom/point.alx', 'pkgs/geom/stack.alx', 'pkgs/shapes/shapes.alx']) files[f] = read(f);
writeFileSync(here + 'www/examples.js',
  '// Generated by web/gen-examples.mjs from acceptance/cases. Do not edit.\n' +
  `export const DEFAULT = 'pe010';\nexport const EXAMPLES = ${JSON.stringify(ex, null, 1)};\nexport const FILES = ${JSON.stringify(files, null, 1)};\n`);
mkdirSync(here + 'www/fixtures', { recursive: true });
copyFileSync(cases + 'fixtures/names.txt', here + 'www/fixtures/names.txt');
console.log(`examples.js: ${ex.length} examples`);
