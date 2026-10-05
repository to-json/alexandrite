//! Tasks, channels, locks and sleep for the browser.
//!
//! Generated code suspends a task by unwinding it: every function that can
//! block saves its variables to the task's frame stack (`alxr_frame_push`)
//! and returns; `run_task` re-enters the task later and each function
//! restores its frame (`alxr_frame_pop`) on the way back down to the
//! operation that blocked, which then runs again. So every blocking
//! operation here is retried: it returns -1 ("suspend") after arranging a
//! wakeup, and the retry finds what happened meanwhile in `Task::fired`.
//!
//! JS drives the loop (`sched_next`, then the program's `run_task`; see
//! www/run.js), so a panic, thrown through the task's wasm frames to JS,
//! ends only that task (`task_failed`). Values (spawn environments, channel
//! elements, task results) travel as pointers to memory the sender
//! allocated; the run's arenas never free, so they stay valid.
//!
//! Threads: in the `atomics` build every worker of a cross-origin-isolated
//! page runs that loop over one shared memory, so a task can resume on any
//! thread (its frames are in memory). Everything here is behind one lock;
//! an idle thread waits on the condvar. A task that blocked is `Parking`
//! until its thread has finished unwinding it (`alxr_task_suspended`): a
//! wakeup meanwhile only marks it, so no other thread can resume it while
//! its frames are still being written. Without threads (the plain build)
//! there's one thread, and a run with every task asleep returns to JS to
//! wait on a timer instead of blocking.
//!
//! Panics: a lock is never held across a throw (throwing skips Rust's
//! destructors), so operations that can panic return what to panic with
//! and the caller panics after unlocking.

use crate::rt::{bytes, new_bytes, ret, ret_str, sh, stop, Stop};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = performance, js_name = now)]
    fn perf_now() -> f64;
    #[wasm_bindgen(js_namespace = Date, js_name = now)]
    fn date_now() -> f64;
}

#[wasm_bindgen(inline_js = "
export function tz_offset(sec) { return -new Date(sec * 1000).getTimezoneOffset() * 60; }
export function tz_name(sec) {
  try {
    const p = new Intl.DateTimeFormat('en-US', { timeZoneName: 'short' }).formatToParts(new Date(sec * 1000));
    return (p.find((x) => x.type === 'timeZoneName') || {}).value || 'UTC';
  } catch (e) { return 'UTC'; }
}")]
extern "C" {
    fn tz_offset(sec: f64) -> f64;
    fn tz_name(sec: f64) -> String;
}

/// What happened to a parked task while it slept (read by the retry).
#[derive(Clone, Copy, Debug)]
enum Fired {
    /// Woken: look again (a task finished, a lock was handed over, a sleep ended).
    Woke,
    /// Received through a select case or a recv: the value (or closed).
    Recv { case: usize, ptr: i64, ok: bool },
    /// A parked send was taken.
    Sent { case: usize },
    /// A parked send's channel was closed.
    SendClosed,
    /// A select with `else` gave the others a turn; now it takes the default.
    Default,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    /// In the run queue.
    Runnable,
    /// On a thread.
    Running,
    /// Blocked, still unwinding on its thread.
    Parking,
    /// Woken while `Parking`: runnable once unwound.
    Woken,
    /// Blocked.
    Parked,
    Done,
}

struct Task {
    /// The spawned worker's id (-1: main).
    worker: i64,
    env: i64,
    /// Saved frames of a suspended task, innermost first.
    frames: Vec<u8>,
    state: State,
    /// Bumped on every wakeup: waiter entries from an older parking are stale.
    epoch: u64,
    fired: Option<Fired>,
    /// Ok(pointer to the value) or Err(panic message).
    result: Option<Result<i64, String>>,
    waiters: Vec<Waiter>,
    /// The locks it holds, oldest first (lock blocks nest).
    held: Vec<usize>,
}

impl Task {
    fn new(worker: i64, env: i64) -> Task {
        Task { worker, env, frames: vec![], state: State::Runnable, epoch: 0, fired: None, result: None, waiters: vec![], held: vec![] }
    }
}

#[derive(Clone, Copy)]
struct Waiter {
    task: usize,
    epoch: u64,
    case: usize,
}

struct Chan {
    cap: usize,
    buf: VecDeque<i64>,
    closed: bool,
    recvq: VecDeque<Waiter>,
    sendq: VecDeque<(Waiter, i64)>,
}

struct Lock {
    held: bool,
    /// A holder panicked (until `clear_poison!`).
    poisoned: bool,
    q: VecDeque<Waiter>,
}

/// How a run ended early (`run_outcome`).
#[derive(Clone, PartialEq, Debug, Default)]
enum Outcome {
    #[default]
    Running,
    Abort,
    Exit1,
    /// `exit 0`: every thread stops, and the run ends normally.
    Exit0,
    Crash(String),
}

/// A parallel `pmap`: elements split into chunks that any thread claims.
struct Job {
    worker: i64,
    inp: i64,
    out: i64,
    n: i64,
    chunk: i64,
    chunks: i64,
    next: i64,
    done: i64,
    /// The lowest failing element and its Result (a fallible worker).
    fail: Option<(i64, i64)>,
    /// A panic on a helper thread, re-raised by the caller.
    panic: Option<String>,
}

#[derive(Default)]
struct Sched {
    tasks: Vec<Task>,
    chans: Vec<Chan>,
    locks: Vec<Lock>,
    runq: VecDeque<usize>,
    /// (wake time in ms, waiter)
    sleepers: Vec<(f64, Waiter)>,
    /// Threads running a task right now.
    running: usize,
    outcome: Outcome,
    jobs: Vec<Job>,
    /// Jobs that may still have chunks to claim.
    open: VecDeque<usize>,
    sleep_ms: f64,
    rng: u64,
}

static S: LazyLock<Mutex<Sched>> = LazyLock::new(|| Mutex::new(Sched::default()));
#[cfg_attr(not(target_feature = "atomics"), allow(dead_code))]
static CV: Condvar = Condvar::new();
/// A scheduled run is going (panics in tasks end only the task).
static ACTIVE: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// The task this thread is running.
    static CUR: Cell<usize> = const { Cell::new(0) };
    /// This thread took a task from `sched_next` (and counts as running).
    static HOLDING: Cell<bool> = const { Cell::new(false) };
    /// The message of the panic that is ending this thread's task.
    static PANIC: RefCell<Option<String>> = const { RefCell::new(None) };
    /// The `pmap` job this helper thread is working on (-1: none).
    static HELPING: Cell<i64> = const { Cell::new(-1) };
    /// The locks this helper thread holds in a pmap element (oldest first).
    static HELPER_HELD: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

fn s() -> MutexGuard<'static, Sched> {
    S.lock().unwrap_or_else(|e| e.into_inner())
}

fn cur() -> usize {
    CUR.with(|c| c.get())
}

fn notify_one() {
    #[cfg(target_feature = "atomics")]
    CV.notify_one();
}

fn notify_all() {
    #[cfg(target_feature = "atomics")]
    CV.notify_all();
}

pub fn reset() {
    *s() = Sched::default();
    ACTIVE.store(false, Ordering::SeqCst);
}

/// Called on every panic: inside a spawned task, end only the task.
pub fn task_panic(msg: &str) {
    if HELPING.with(|h| h.get()) >= 0 {
        PANIC.with(|p| *p.borrow_mut() = Some(msg.to_string()));
        stop(Stop::Pmap);
    }
    if ACTIVE.load(Ordering::SeqCst) && cur() != 0 {
        PANIC.with(|p| *p.borrow_mut() = Some(msg.to_string()));
        stop(Stop::Task);
    }
}

/// A panic to raise once the lock is released: (what, where).
type Panic = (&'static str, String);

fn raise<T>(r: Result<T, Panic>) -> T {
    match r {
        Ok(v) => v,
        Err((what, at)) => crate::rt::panic_msg(what, &at),
    }
}

impl Sched {
    fn rand(&mut self, n: usize) -> usize {
        // xorshift64*
        if self.rng == 0 {
            self.rng = 0x9e37_79b9_7f4a_7c15;
        }
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        (self.rng.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 33) as usize % n
    }

    fn me(&self) -> Waiter {
        let t = cur();
        Waiter { task: t, epoch: self.tasks[t].epoch, case: 0 }
    }

    fn valid(&self, w: &Waiter) -> bool {
        let t = &self.tasks[w.task];
        matches!(t.state, State::Parking | State::Parked) && t.epoch == w.epoch
    }

    fn enqueue(&mut self, t: usize) {
        self.tasks[t].state = State::Runnable;
        self.runq.push_back(t);
        notify_one();
    }

    /// Wake a parked task with what happened, unless the waiter is stale.
    fn fire(&mut self, w: Waiter, f: Fired) -> bool {
        if !self.valid(&w) {
            return false;
        }
        let t = &mut self.tasks[w.task];
        t.fired = Some(f);
        t.epoch += 1;
        if t.state == State::Parking {
            t.state = State::Woken;
        } else {
            self.enqueue(w.task);
        }
        true
    }

    /// The current task blocks: suspend it.
    fn park(&mut self) -> i32 {
        self.tasks[cur()].state = State::Parking;
        -1
    }

    fn take_fired(&mut self) -> Option<Fired> {
        self.tasks[cur()].fired.take()
    }

    fn chan(&mut self, h: i64) -> &mut Chan {
        &mut self.chans[h as usize]
    }

    /// The first live waiting receiver, dropping stale ones.
    fn pop_recv(&mut self, h: i64) -> Option<Waiter> {
        while let Some(w) = self.chan(h).recvq.pop_front() {
            if self.valid(&w) {
                return Some(w);
            }
        }
        None
    }

    fn pop_send(&mut self, h: i64) -> Option<(Waiter, i64)> {
        while let Some((w, p)) = self.chan(h).sendq.pop_front() {
            if self.valid(&w) {
                return Some((w, p));
            }
        }
        None
    }

    fn has_recv(&mut self, h: i64) -> bool {
        loop {
            let Some(w) = self.chan(h).recvq.front().copied() else { return false };
            if self.valid(&w) {
                return true;
            }
            self.chan(h).recvq.pop_front();
        }
    }

    fn has_send(&mut self, h: i64) -> bool {
        loop {
            let Some((w, _)) = self.chan(h).sendq.front().copied() else { return false };
            if self.valid(&w) {
                return true;
            }
            self.chan(h).sendq.pop_front();
        }
    }

    fn send_ready(&mut self, h: i64) -> bool {
        let c = self.chan(h);
        c.closed || c.buf.len() < c.cap || self.has_recv(h)
    }

    fn recv_ready(&mut self, h: i64) -> bool {
        let c = self.chan(h);
        !c.buf.is_empty() || c.closed || self.has_send(h)
    }

    /// Send now (the channel is ready for it): to a waiting receiver, else
    /// into the buffer. Closed: the panic to raise.
    fn send_now(&mut self, h: i64, p: i64, loc: &str) -> Result<(), Panic> {
        if self.chan(h).closed {
            return Err(("send on closed channel", loc.to_string()));
        }
        if let Some(w) = self.pop_recv(h) {
            self.fire(w, Fired::Recv { case: w.case, ptr: p, ok: true });
        } else {
            self.chan(h).buf.push_back(p);
        }
        Ok(())
    }

    /// Receive now (the channel is ready): Some(pointer), or None if closed and drained.
    fn recv_now(&mut self, h: i64) -> Option<i64> {
        if let Some(p) = self.chan(h).buf.pop_front() {
            // A parked sender's value moves into the freed slot.
            if let Some((w, q)) = self.pop_send(h) {
                self.chan(h).buf.push_back(q);
                self.fire(w, Fired::Sent { case: w.case });
            }
            return Some(p);
        }
        if let Some((w, q)) = self.pop_send(h) {
            self.fire(w, Fired::Sent { case: w.case });
            return Some(q);
        }
        None
    }

    fn finish(&mut self, t: usize, r: Result<i64, String>) {
        let task = &mut self.tasks[t];
        task.state = State::Done;
        task.result = Some(r);
        task.frames = vec![];
        for w in std::mem::take(&mut task.waiters) {
            self.fire(w, Fired::Woke);
        }
        if t == 0 {
            notify_all();
        }
    }

    /// A job with chunks left to claim, if any.
    fn open_job(&mut self) -> Option<usize> {
        while let Some(&j) = self.open.front() {
            if self.jobs[j].next < self.jobs[j].chunks {
                return Some(j);
            }
            self.open.pop_front();
        }
        None
    }

    /// The run is over: main returned, or it ended early.
    fn over(&self) -> bool {
        self.tasks.first().is_none_or(|m| m.state == State::Done) || self.outcome != Outcome::Running
    }

    /// Wake the sleepers whose time has come; the earliest left (ms), if any.
    fn wake_sleepers(&mut self) -> Option<f64> {
        if self.sleepers.is_empty() {
            return None;
        }
        let now = perf_now();
        let mut i = 0;
        while i < self.sleepers.len() {
            if self.sleepers[i].0 <= now {
                let (_, w) = self.sleepers.swap_remove(i);
                self.fire(w, Fired::Woke);
            } else {
                i += 1;
            }
        }
        self.sleepers.iter().map(|x| x.0).reduce(f64::min)
    }
}

fn loc_str(lp: i64, ln: i64) -> String {
    String::from_utf8_lossy(bytes(lp, ln)).into_owned()
}

// ---------- driven from JS ----------

/// Start a scheduled run: main is task 0. (Once, on one thread, before any
/// thread calls `sched_next`.)
#[wasm_bindgen]
pub fn sched_start() {
    let mut s = s();
    *s = Sched::default();
    s.tasks.push(Task::new(-1, 0));
    s.runq.push_back(0);
    ACTIVE.store(true, Ordering::SeqCst);
}

/// The next task for this thread to run (pass it to the program's
/// `run_task`), or -1: the run is over (see `run_outcome`); -3: every task
/// is asleep, wait `sched_sleep_ms` and ask again (only without threads:
/// with them, this waits); -4: help with a `pmap` (the program's `run_pmap`).
#[wasm_bindgen]
pub fn sched_next() -> i32 {
    let mut s = s();
    if HOLDING.with(|h| h.replace(false)) {
        s.running -= 1;
    }
    loop {
        if s.over() {
            ACTIVE.store(false, Ordering::SeqCst);
            notify_all();
            return -1;
        }
        // A pmap's caller is waiting: help it first.
        if s.open_job().is_some() {
            return -4;
        }
        let next_wake = s.wake_sleepers();
        if let Some(t) = s.runq.pop_front() {
            if s.tasks[t].state != State::Runnable {
                continue;
            }
            s.tasks[t].state = State::Running;
            s.running += 1;
            HOLDING.with(|h| h.set(true));
            CUR.with(|c| c.set(t));
            return t as i32;
        }
        if next_wake.is_none() && s.running == 0 {
            s.outcome = Outcome::Abort;
            sh().err.push_str("alexandrite: all tasks are asleep: deadlock at runtime\n");
            continue;
        }
        #[cfg(target_feature = "atomics")]
        {
            s = match next_wake {
                None => CV.wait(s).unwrap_or_else(|e| e.into_inner()),
                Some(at) => {
                    let ms = (at - perf_now()).clamp(0.0, 1e9);
                    CV.wait_timeout(s, std::time::Duration::from_micros((ms * 1000.0) as u64)).unwrap_or_else(|e| e.into_inner()).0
                }
            };
        }
        #[cfg(not(target_feature = "atomics"))]
        {
            // One thread, so nothing else is running: only sleepers are left.
            s.sleep_ms = (next_wake.unwrap() - perf_now()).max(0.0);
            return -3;
        }
    }
}

#[wasm_bindgen]
pub fn sched_sleep_ms() -> f64 {
    s().sleep_ms
}

/// How the run ended: 0 normally (main returned), 1 a panic or deadlock,
/// 2 exit 1, 3 a crash (`run_crash_message`).
#[wasm_bindgen]
pub fn run_outcome() -> i32 {
    match s().outcome {
        Outcome::Running => 0,
        Outcome::Abort => 1,
        Outcome::Exit1 => 2,
        Outcome::Crash(_) => 3,
        Outcome::Exit0 => 4,
    }
}

#[wasm_bindgen]
pub fn run_crash_message() -> String {
    match &s().outcome {
        Outcome::Crash(m) => m.clone(),
        _ => String::new(),
    }
}

/// This thread's task ended the run (`alx:abort` = 1, `alx:exit1` = 2,
/// a crash = 3 with its message, `alx:exit0` = 4): every thread stops.
#[wasm_bindgen]
pub fn run_failed(how: i32, msg: String) {
    let mut s = s();
    if s.outcome == Outcome::Running {
        s.outcome = match how {
            1 => Outcome::Abort,
            2 => Outcome::Exit1,
            4 => Outcome::Exit0,
            _ => Outcome::Crash(msg),
        };
    }
    notify_all();
}

/// The current task panicked (JS caught `alx:task`): it's done, with the message.
#[wasm_bindgen]
pub fn task_failed() {
    let msg = PANIC.with(|p| p.borrow_mut().take()).unwrap_or_else(|| "alexandrite: task failed".into());
    let mut s = s();
    let t = cur();
    let held = std::mem::take(&mut s.tasks[t].held);
    s.poison(held);
    s.finish(t, Err(msg));
}

/// A `pmap` element panicked while this helper ran it (JS caught `alx:pmap`).
#[wasm_bindgen]
pub fn pmap_failed() {
    let j = HELPING.with(|h| h.replace(-1));
    let msg = PANIC.with(|p| p.borrow_mut().take()).unwrap_or_else(|| "alexandrite: pmap failed".into());
    if j >= 0 {
        let mut s = s();
        let held = HELPER_HELD.with(|h| std::mem::take(&mut *h.borrow_mut()));
        s.poison(held);
        let job = &mut s.jobs[j as usize];
        job.panic.get_or_insert(msg);
        job.done += 1;
        notify_all();
    }
}

// ---------- pmap (threads) ----------

/// Start a parallel `pmap` of worker `w` over `n` elements at `inp` into `out`.
#[unsafe(no_mangle)]
pub extern "C" fn alxr_pmap_begin(w: i64, inp: i64, n: i64, out: i64) -> i64 {
    let mut s = s();
    let chunk = ((n + 127) / 128).max(1);
    let chunks = (n + chunk - 1) / chunk;
    s.jobs.push(Job { worker: w, inp, out, n, chunk, chunks, next: 0, done: 0, fail: None, panic: None });
    let j = s.jobs.len() - 1;
    s.open.push_back(j);
    notify_all();
    j as i64
}

/// Claim a chunk of job `j`: its first element (RET[0] = one past its
/// last), or -1 when none are left.
#[unsafe(no_mangle)]
pub extern "C" fn alxr_pmap_claim(j: i64) -> i64 {
    let mut s = s();
    let job = &mut s.jobs[j as usize];
    // Past a failure nothing else matters.
    if job.next >= job.chunks || job.fail.is_some_and(|(i, _)| i < job.next * job.chunk) {
        return -1;
    }
    let lo = job.next * job.chunk;
    job.next += 1;
    ret(&[(lo + job.chunk).min(job.n)]);
    lo
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_pmap_chunk_done(j: i64) {
    let mut s = s();
    let job = &mut s.jobs[j as usize];
    job.done += 1;
    if job.done >= job.next && job.next >= job.chunks {
        notify_all();
    }
}

/// Element `i` failed with the Result at `p`; the lowest failure wins.
#[unsafe(no_mangle)]
pub extern "C" fn alxr_pmap_fail(j: i64, i: i64, p: i64) {
    let mut s = s();
    let job = &mut s.jobs[j as usize];
    if job.fail.is_none_or(|(k, _)| i < k) {
        job.fail = Some((i, p));
    }
    // Chunks after it won't be claimed: count them done.
    let skipped = job.chunks - job.next;
    job.next = job.chunks;
    job.done += skipped;
    notify_all();
}

/// Wait for every claimed chunk of job `j`: the lowest failing element
/// (RET[0] = its Result), or -1. A helper's panic is raised here.
#[unsafe(no_mangle)]
pub extern "C" fn alxr_pmap_wait(j: i64) -> i64 {
    let panic = {
        let mut s = s();
        loop {
            let job = &s.jobs[j as usize];
            if job.done >= job.next && (job.next >= job.chunks || job.fail.is_some()) {
                break;
            }
            #[cfg(target_feature = "atomics")]
            {
                s = CV.wait(s).unwrap_or_else(|e| e.into_inner());
            }
            #[cfg(not(target_feature = "atomics"))]
            unreachable!("pmap jobs need threads");
        }
        let job = &mut s.jobs[j as usize];
        // No one claims what's left.
        job.next = job.chunks;
        match (&job.panic, job.fail) {
            (Some(m), _) => Some(m.clone()),
            (None, Some((i, p))) => {
                ret(&[p]);
                return i;
            }
            (None, None) => return -1,
        }
    };
    crate::rt::abort_with(panic.unwrap())
}

/// For a helper: a job to work on (RET[0..3] = worker, input, output), or -1.
#[unsafe(no_mangle)]
pub extern "C" fn alxr_pmap_job() -> i64 {
    let mut s = s();
    match s.open_job() {
        Some(j) => {
            let job = &s.jobs[j];
            ret(&[job.worker, job.inp, job.out]);
            HELPING.with(|h| h.set(j as i64));
            j as i64
        }
        None => {
            HELPING.with(|h| h.set(-1));
            -1
        }
    }
}

// ---------- called by run_task ----------

/// The current task's worker id (-1 for main).
#[unsafe(no_mangle)]
pub extern "C" fn alxr_task_begin(_t: i32) -> i64 {
    s().tasks[cur()].worker
}

/// Whether the current task has frames to restore (it suspended before).
#[unsafe(no_mangle)]
pub extern "C" fn alxr_task_resuming() -> i32 {
    !s().tasks[cur()].frames.is_empty() as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_task_env() -> i64 {
    s().tasks[cur()].env
}

/// The current task has finished unwinding after it blocked.
#[unsafe(no_mangle)]
pub extern "C" fn alxr_task_suspended() {
    let mut s = s();
    let t = cur();
    match s.tasks[t].state {
        State::Woken => s.enqueue(t),
        State::Parking => s.tasks[t].state = State::Parked,
        _ => {}
    }
}

/// The current task returned (its value at `p`).
#[unsafe(no_mangle)]
pub extern "C" fn alxr_task_done(p: i64) {
    s().finish(cur(), Ok(p));
}

/// Room for `n` bytes of a frame on the current task's frame stack.
#[unsafe(no_mangle)]
pub extern "C" fn alxr_frame_push(n: i64) -> i64 {
    let mut s = s();
    let f = &mut s.tasks[cur()].frames;
    let at = f.len();
    f.resize(at + n as usize, 0);
    f.as_ptr() as usize as i64 + at as i64
}

/// The current task's newest frame (`n` bytes), popped (valid until the next push).
#[unsafe(no_mangle)]
pub extern "C" fn alxr_frame_pop(n: i64) -> i64 {
    let mut s = s();
    let f = &mut s.tasks[cur()].frames;
    let at = f.len() - n as usize;
    f.truncate(at);
    f.as_ptr() as usize as i64 + at as i64
}

// ---------- tasks ----------

/// Start a task running worker `w` on the environment at `env`.
#[unsafe(no_mangle)]
pub extern "C" fn alxr_spawn(w: i64, env: i64) -> i64 {
    let mut s = s();
    let t = s.tasks.len();
    s.tasks.push(Task::new(w, env));
    s.enqueue(t);
    t as i64
}

/// -1: suspend; 1: returned (RET[0] = pointer to the value); 0: panicked
/// (RET[0..2] = the message).
#[unsafe(no_mangle)]
pub extern "C" fn alxr_wait(t: i64) -> i32 {
    let mut s = s();
    s.take_fired();
    match &s.tasks[t as usize].result {
        Some(Ok(p)) => {
            ret(&[*p]);
            1
        }
        Some(Err(m)) => {
            ret_str(m.clone().as_bytes());
            0
        }
        None => {
            let me = s.me();
            s.tasks[t as usize].waiters.push(me);
            s.park()
        }
    }
}

// ---------- channels ----------

#[unsafe(no_mangle)]
pub extern "C" fn alxr_chan_new(cap: i64) -> i64 {
    let mut s = s();
    s.chans.push(Chan { cap: cap.max(0) as usize, buf: VecDeque::new(), closed: false, recvq: VecDeque::new(), sendq: VecDeque::new() });
    (s.chans.len() - 1) as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_chan_len(h: i64) -> i64 {
    s().chan(h).buf.len() as i64
}

/// Send the value at `p`: 0 sent, -1 suspend.
#[unsafe(no_mangle)]
pub extern "C" fn alxr_chan_send(h: i64, p: i64, lp: i64, ln: i64) -> i32 {
    raise((|| {
        let mut s = s();
        match s.take_fired() {
            Some(Fired::Sent { .. }) => return Ok(0),
            Some(Fired::SendClosed) => return Err(("send on closed channel", loc_str(lp, ln))),
            _ => {}
        }
        if s.send_ready(h) {
            s.send_now(h, p, &loc_str(lp, ln))?;
            return Ok(0);
        }
        let me = s.me();
        s.chan(h).sendq.push_back((me, p));
        Ok(s.park())
    })())
}

/// 1: received (RET[0] = pointer to the value); 0: closed and drained; -1: suspend.
#[unsafe(no_mangle)]
pub extern "C" fn alxr_chan_recv(h: i64) -> i32 {
    let mut s = s();
    if let Some(Fired::Recv { ptr, ok, .. }) = s.take_fired() {
        ret(&[ptr]);
        return ok as i32;
    }
    if s.recv_ready(h) {
        return match s.recv_now(h) {
            Some(p) => {
                ret(&[p]);
                1
            }
            None => 0,
        };
    }
    let me = s.me();
    s.chan(h).recvq.push_back(me);
    s.park()
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_chan_close(h: i64, lp: i64, ln: i64) {
    raise((|| {
        let mut s = s();
        if s.chan(h).closed {
            return Err(("close of closed channel", loc_str(lp, ln)));
        }
        s.chan(h).closed = true;
        while let Some(w) = s.pop_recv(h) {
            s.fire(w, Fired::Recv { case: w.case, ptr: 0, ok: false });
        }
        while let Some((w, _)) = s.pop_send(h) {
            s.fire(w, Fired::SendClosed);
        }
        Ok(())
    })())
}

/// Select over `n` cases at `cs` (each: kind 0 send / 1 recv, channel,
/// value pointer for a send). The chosen case's index; `n` for the
/// default; -1: suspend. A receive also sets RET[0] = value pointer and
/// RET[1] = ok.
#[unsafe(no_mangle)]
pub extern "C" fn alxr_select(cs: i64, n: i64, default: i32) -> i64 {
    raise((|| {
        let mut s = s();
        let n = n as usize;
        let case = |i: usize| -> (bool, i64, i64) {
            let w = |k: usize| unsafe { *((cs as usize + 24 * i + 8 * k) as *const i64) };
            (w(0) == 0, w(1), w(2))
        };
        match s.take_fired() {
            Some(Fired::Recv { case, ptr, ok }) => {
                ret(&[ptr, ok as i64]);
                return Ok(case as i64);
            }
            Some(Fired::Sent { case }) => return Ok(case as i64),
            Some(Fired::SendClosed) => return Err(("send on closed channel", "select".to_string())),
            Some(Fired::Default) => return Ok(n as i64),
            _ => {}
        }
        let ready: Vec<usize> = (0..n)
            .filter(|&i| {
                let (send, h, _) = case(i);
                if send { s.send_ready(h) } else { s.recv_ready(h) }
            })
            .collect();
        if !ready.is_empty() {
            let i = ready[s.rand(ready.len())];
            let (send, h, p) = case(i);
            if send {
                s.send_now(h, p, "select")?;
            } else {
                match s.recv_now(h) {
                    Some(p) => ret(&[p, 1]),
                    None => ret(&[0, 0]),
                }
            }
            return Ok(i as i64);
        }
        if default != 0 {
            // Nothing ready: let the other tasks run before taking the default.
            let t = cur();
            s.tasks[t].fired = Some(Fired::Default);
            s.tasks[t].state = State::Woken;
            return Ok(-1);
        }
        let me = s.me();
        for i in 0..n {
            let (send, h, p) = case(i);
            let w = Waiter { case: i, ..me };
            if send {
                s.chan(h).sendq.push_back((w, p));
            } else {
                s.chan(h).recvq.push_back(w);
            }
        }
        Ok(s.park() as i64)
    })())
}

// ---------- locks ----------

// Each holder keeps the locks it holds (`Task::held`; a pmap helper thread,
// `HELPER_HELD`). A panic releases them and marks them poisoned; taking a
// poisoned lock panics until `clear_poison!`.

#[unsafe(no_mangle)]
pub extern "C" fn alxr_lock_new() -> i64 {
    let mut s = s();
    s.locks.push(Lock { held: false, poisoned: false, q: VecDeque::new() });
    (s.locks.len() - 1) as i64
}

const POISONED: &str = "Mutex poisoned: a task panicked while holding it";

/// 0: taken; -1: suspend (the unlocker hands it over).
#[unsafe(no_mangle)]
pub extern "C" fn alxr_lock(l: i64, lp: i64, ln: i64) -> i32 {
    let l = l as usize;
    raise((|| {
        let mut s = s();
        if let Some(Fired::Woke) = s.take_fired() {
            // Handed over.
            if s.locks[l].poisoned {
                s.release(l);
                return Err((POISONED, loc_str(lp, ln)));
            }
            s.hold(l);
            return Ok(0);
        }
        let lk = &mut s.locks[l];
        if !lk.held {
            if lk.poisoned {
                return Err((POISONED, loc_str(lp, ln)));
            }
            lk.held = true;
            s.hold(l);
            return Ok(0);
        }
        let me = s.me();
        s.locks[l].q.push_back(me);
        Ok(s.park())
    })())
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_unlock(l: i64) {
    let l = l as usize;
    let mut s = s();
    let drop_held = |h: &mut Vec<usize>| {
        if let Some(i) = h.iter().rposition(|&x| x == l) {
            h.remove(i);
        }
    };
    if HELPING.with(|h| h.get()) >= 0 {
        HELPER_HELD.with(|h| drop_held(&mut h.borrow_mut()));
    } else {
        let t = cur();
        drop_held(&mut s.tasks[t].held);
    }
    s.release(l);
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_lock_poisoned(l: i64) -> i32 {
    s().locks[l as usize].poisoned as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_lock_clear_poison(l: i64) {
    s().locks[l as usize].poisoned = false;
}

impl Sched {
    /// The current holder took lock `l`.
    fn hold(&mut self, l: usize) {
        if HELPING.with(|h| h.get()) >= 0 {
            HELPER_HELD.with(|h| h.borrow_mut().push(l));
        } else {
            let t = cur();
            self.tasks[t].held.push(l);
        }
    }

    /// Hand lock `l` to a waiter, or free it.
    fn release(&mut self, l: usize) {
        while let Some(w) = self.locks[l].q.pop_front() {
            // Handed over: it stays held.
            if self.fire(w, Fired::Woke) {
                return;
            }
        }
        self.locks[l].held = false;
    }

    /// After a panic: release the locks in `held` (newest last) and poison them.
    fn poison(&mut self, held: Vec<usize>) {
        for l in held.into_iter().rev() {
            self.locks[l].poisoned = true;
            self.release(l);
        }
    }
}

// ---------- time ----------

/// Sleep the current task: 0 done, -1 suspend.
#[unsafe(no_mangle)]
pub extern "C" fn alxr_sleep(ns: i64) -> i32 {
    let mut s = s();
    if let Some(Fired::Woke) = s.take_fired() {
        return 0;
    }
    if ns <= 0 || !ACTIVE.load(Ordering::SeqCst) {
        return 0;
    }
    let me = s.me();
    s.sleepers.push((perf_now() + ns as f64 / 1e6, me));
    // A thread waiting on the condvar may need to wake sooner now.
    notify_all();
    s.park()
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_now_ns() -> i64 {
    (perf_now() * 1e6) as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_wall_ns() -> i64 {
    (date_now() * 1e6) as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_local_offset(sec: i64) -> i64 {
    tz_offset(sec as f64) as i64
}

/// The zone's abbreviation as a NUL-terminated string (a `Ptr`).
#[unsafe(no_mangle)]
pub extern "C" fn alxr_local_zone(sec: i64) -> i64 {
    let mut b = tz_name(sec as f64).into_bytes();
    b.push(0);
    new_bytes(&b)
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_str_from_cstr(p: i64) {
    let mut n = 0usize;
    while unsafe { *((p as usize + n) as *const u8) } != 0 {
        n += 1;
    }
    ret_str(bytes(p, n as i64));
}

// ---------- panics and capture ----------

/// Panic with a computed message (without the `alexandrite: ` prefix).
#[unsafe(no_mangle)]
pub extern "C" fn alxr_panic_str(p: i64, n: i64) {
    crate::rt::abort_with(format!("alexandrite: {}", String::from_utf8_lossy(bytes(p, n))))
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_cap_begin() -> i64 {
    let mut s = sh();
    s.cap = Some(s.out.len());
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn alxr_cap_end() {
    let got = {
        let mut s = sh();
        let at = s.cap.take().unwrap_or(s.out.len()).min(s.out.len());
        s.out.split_off(at)
    };
    ret_str(got.as_bytes());
}
