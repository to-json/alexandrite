// Test body for the oracle's task/channel prelude. run_conc.sh appends this
// to prelude.rs (as the emitter does with generated code) and runs it.

fn w_send(ch: Chan<i64>) -> String {
    for i in 1..=10 {
        ch.send(i, "t.alx:1:1");
    }
    ch.close("t.alx:1:1");
    "done".to_string()
}
fn w_oob(_: ()) -> i64 {
    let a: Sl<i64> = Sl::from(vec![1, 2, 3]);
    a.get(idx(7, a.len(), "f.alx:3:5"))
}
fn w_unbuf(a: (Chan<i64>, Chan<i64>)) {
    // Receives 1, then reports it on the second channel after taking it.
    let x = a.0.recv().unwrap();
    a.1.send(x * 10, "t");
}
fn w_ret(n: i64) -> i64 {
    n + 1
}
fn w_late_send(ch: Chan<i64>) {
    std::thread::sleep(std::time::Duration::from_millis(50));
    ch.send(42, "t");
}
fn w_closed(c: Chan<i64>) {
    c.send(1, "g.alx:2:3");
}
fn w_close2(c: Chan<i64>) {
    c.close("g.alx:9:1");
}

fn main() {
    // 1. buffered producer, main sums
    let ch: Chan<i64> = Chan::new(2);
    let t = task_spawn(w_send, ch.clone());
    let mut sum = 0;
    while let Some(v) = ch.recv() {
        sum += v;
    }
    assert_eq!(sum, 55);
    assert_eq!(t.wait().unwrap().len(), 4);
    assert!(t.wait().is_ok()); // repeatable

    // 2. unbuffered rendezvous: send returns only after the receiver took it
    let (a, b): (Chan<i64>, Chan<i64>) = (Chan::new(0), Chan::new(0));
    let t = task_spawn(w_unbuf, (a.clone(), b.clone()));
    assert_eq!(a.len(), 0);
    a.send(1, "t");
    assert_eq!(b.recv(), Some(10));
    t.wait().unwrap();
    // a sender must stay blocked while nobody receives
    let c: Chan<i64> = Chan::new(0);
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (c2, d2) = (c.clone(), done.clone());
    std::thread::spawn(move || {
        c2.send(5, "t");
        d2.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(!done.load(std::sync::atomic::Ordering::SeqCst), "unbuffered send returned without receiver");
    assert_eq!(c.recv(), Some(5));
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(done.load(std::sync::atomic::Ordering::SeqCst));

    // 3. select: default, blocking, send case, closed recv, fairness
    let e: Chan<i64> = Chan::new(1);
    let mut r = SelRecv::new(e.clone());
    assert_eq!(select(&mut [&mut r as &mut dyn SelCase], true), 1);
    e.send(7, "t");
    let mut r = SelRecv::new(e.clone());
    assert_eq!(select(&mut [&mut r as &mut dyn SelCase], true), 0);
    assert_eq!(r.result(), Some(7));
    let mut s = SelSend::new(e.clone(), 8);
    let mut r2 = SelRecv::new(Chan::<i64>::new(0));
    assert_eq!(select(&mut [&mut r2 as &mut dyn SelCase, &mut s], false), 1);
    assert_eq!(e.len(), 1);
    let late: Chan<i64> = Chan::new(0);
    let t = task_spawn(w_late_send, late.clone());
    let mut r = SelRecv::new(late.clone());
    let mut r0 = SelRecv::new(Chan::<i64>::new(0));
    assert_eq!(select(&mut [&mut r0 as &mut dyn SelCase, &mut r], false), 1);
    assert_eq!(r.result(), Some(42));
    t.wait().unwrap();
    e.close("t");
    let mut r = SelRecv::new(e.clone());
    assert_eq!(select(&mut [&mut r as &mut dyn SelCase], false), 0);
    assert_eq!(r.result(), Some(8));
    let mut r = SelRecv::new(e.clone());
    select(&mut [&mut r as &mut dyn SelCase], false);
    assert_eq!(r.result(), None);
    let (x, y): (Chan<i64>, Chan<i64>) = (Chan::new(100), Chan::new(100));
    for i in 0..100 {
        x.send(i, "t");
        y.send(i, "t");
    }
    let mut hits = [0; 2];
    for _ in 0..100 {
        let mut rx = SelRecv::new(x.clone());
        let mut ry = SelRecv::new(y.clone());
        hits[select(&mut [&mut rx as &mut dyn SelCase, &mut ry], false) as usize] += 1;
    }
    assert!(hits[0] > 10 && hits[1] > 10, "{hits:?}");

    // 4. panicking task: message is what main would print; nothing on stderr
    let t = task_spawn(w_oob, ());
    match t.wait() {
        Ok(_) => panic!("expected failure"),
        Err(m) => assert_eq!(&*m.0, b"alexandrite: index out of bounds at f.alx:3:5"),
    }
    assert!(t.wait().is_err());
    assert_eq!(task_spawn(w_ret, 1).wait().unwrap(), 2);

    // 5. send on closed / double close fail inside a task, with messages
    let c: Chan<i64> = Chan::new(1);
    c.close("t");
    assert_eq!(&*task_spawn(w_closed, c.clone()).wait().unwrap_err().0, b"alexandrite: send on a closed channel at g.alx:2:3");
    assert_eq!(&*task_spawn(w_close2, c.clone()).wait().unwrap_err().0, b"alexandrite: close of a closed channel at g.alx:9:1");
    println!("conc ok");
}
