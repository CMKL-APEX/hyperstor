//! Diagnostics, built in only with `HYPERSTOR_DIAG=1` (`build.rs`, the
//! `hs_diag` cfg): `diag!` lines on standard error, each key at most
//! once per 10 s with the count held back since. Without it, `diag!`
//! expands to nothing and its arguments aren't evaluated.

#[cfg(hs_diag)]
mod imp {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    const EVERY: Duration = Duration::from_secs(10);

    thread_local! {
        static LAST: RefCell<HashMap<String, (Instant, u64)>> = RefCell::new(HashMap::new());
    }

    pub fn note(key: String, line: impl FnOnce() -> String) {
        LAST.with(|l| {
            let mut l = l.borrow_mut();
            let now = Instant::now();
            match l.get_mut(&key) {
                Some((at, held)) if now.duration_since(*at) < EVERY => *held += 1,
                Some((at, held)) => {
                    eprintln!("hyperstord diag: {} (+{held} like it)", line());
                    (*at, *held) = (now, 0);
                }
                None => {
                    eprintln!("hyperstord diag: {}", line());
                    l.insert(key, (now, 0));
                }
            }
        });
    }
}

#[cfg(hs_diag)]
pub use imp::note;

/// `diag!(key_fmt, key_args…; line_fmt, line_args…)`: a throttled line
/// on standard error in a diagnostic build; nothing otherwise.
#[cfg(hs_diag)]
#[macro_export]
macro_rules! diag {
    ($kf:literal $(, $ka:expr)* ; $($line:tt)*) => {
        $crate::diag::note(format!($kf $(, $ka)*), || format!($($line)*))
    };
}

#[cfg(not(hs_diag))]
#[macro_export]
macro_rules! diag {
    ($($t:tt)*) => {};
}
