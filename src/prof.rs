//! A tiny span profiler for finding where statements spend their time. Build with `--features prof`; without it
//! every call compiles to nothing. `span!("name")` times the rest of the enclosing block; `report()` prints, per
//! span, the number of calls and the inclusive and exclusive (without child spans) time.

#[cfg(feature = "prof")]
mod imp {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::time::Instant;

    #[derive(Default, Clone, Copy)]
    struct Total {
        calls: u64,
        incl: u128,
        excl: u128,
    }

    thread_local! {
        static STACK: RefCell<Vec<(&'static str, Instant, u128)>> = const { RefCell::new(Vec::new()) };
        static TOTALS: RefCell<HashMap<&'static str, Total>> = RefCell::new(HashMap::new());
    }

    pub struct Span;

    pub fn enter(name: &'static str) -> Span {
        STACK.with(|s| s.borrow_mut().push((name, Instant::now(), 0)));
        Span
    }

    impl Drop for Span {
        fn drop(&mut self) {
            STACK.with(|s| {
                let mut s = s.borrow_mut();
                if let Some((name, start, child)) = s.pop() {
                    let el = start.elapsed().as_nanos();
                    if let Some(parent) = s.last_mut() {
                        parent.2 += el;
                    }
                    TOTALS.with(|t| {
                        let mut t = t.borrow_mut();
                        let e = t.entry(name).or_default();
                        e.calls += 1;
                        e.incl += el;
                        e.excl += el.saturating_sub(child);
                    });
                }
            });
        }
    }

    pub fn report() {
        TOTALS.with(|t| {
            let t = t.borrow();
            let mut v: Vec<_> = t.iter().collect();
            v.sort_by(|a, b| b.1.excl.cmp(&a.1.excl));
            println!("{:<34} {:>10} {:>12} {:>12} {:>10}", "span", "calls", "incl ms", "excl ms", "excl us/call");
            for (n, x) in v {
                println!("{:<34} {:>10} {:>12.1} {:>12.1} {:>10.2}", n, x.calls, x.incl as f64 / 1e6, x.excl as f64 / 1e6, x.excl as f64 / 1e3 / x.calls.max(1) as f64);
            }
        });
    }

    pub fn reset() {
        TOTALS.with(|t| t.borrow_mut().clear());
    }
}

#[cfg(not(feature = "prof"))]
mod imp {
    pub struct Span;
    #[inline(always)]
    pub fn enter(_name: &'static str) -> Span {
        Span
    }
    pub fn report() {}
    pub fn reset() {}
}

pub use imp::{enter, report, reset, Span};

/// Time the rest of the enclosing block under `name` (a no-op unless built with `--features prof`).
#[macro_export]
macro_rules! span {
    ($name:expr) => {
        let _span_guard = $crate::prof::enter($name);
    };
}
