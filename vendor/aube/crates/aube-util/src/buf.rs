//! Reusable per-thread scratch buffers.
//!
//! Avoids per-call `format!` / `Vec::new` allocations for transient
//! lookup keys. Caller borrows the cleared buffer for the lifetime of
//! the closure and then it returns to the pool. Same idea as the
//! ad-hoc `KEY_BUF` thread_local in `aube-scripts::policy`.

use std::cell::RefCell;

thread_local! {
    static SCRATCH_STRING: RefCell<String> = const { RefCell::new(String::new()) };
    static SCRATCH_BYTES: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Borrow a thread-local `String`, cleared on entry. Useful for
/// temporary `format!`-style key construction where the result is
/// only consumed inside the closure (e.g. a HashMap probe).
///
/// The buffer lives for the life of the thread, so repeated calls
/// do not re-allocate. Nesting calls is supported because each call
/// clears on entry.
pub fn with_scratch_string<R>(f: impl FnOnce(&mut String) -> R) -> R {
    SCRATCH_STRING.with(|cell| {
        let mut owned = match cell.try_borrow_mut() {
            Ok(mut b) => {
                b.clear();
                return f(&mut b);
            }
            Err(_) => String::new(),
        };
        f(&mut owned)
    })
}

/// Borrow a thread-local `Vec<u8>`, cleared on entry. Useful for
/// streaming hashers that need a per-chunk buffer.
pub fn with_scratch_bytes<R>(f: impl FnOnce(&mut Vec<u8>) -> R) -> R {
    SCRATCH_BYTES.with(|cell| {
        let mut owned = match cell.try_borrow_mut() {
            Ok(mut b) => {
                b.clear();
                return f(&mut b);
            }
            Err(_) => Vec::new(),
        };
        f(&mut owned)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    #[test]
    fn string_scratch_clears_each_call() {
        with_scratch_string(|s| write!(s, "first").unwrap());
        let observed = with_scratch_string(|s| {
            assert!(s.is_empty(), "scratch must be cleared on entry");
            write!(s, "second").unwrap();
            s.clone()
        });
        assert_eq!(observed, "second");
    }

    #[test]
    fn bytes_scratch_clears_each_call() {
        with_scratch_bytes(|b| b.extend_from_slice(b"first"));
        with_scratch_bytes(|b| {
            assert!(b.is_empty());
            b.extend_from_slice(b"second");
            assert_eq!(b.as_slice(), b"second");
        });
    }

    #[test]
    fn nested_calls_get_fresh_buffer() {
        with_scratch_string(|outer| {
            outer.push_str("outer");
            // Inner call sees a borrow conflict and falls back to a
            // fresh local buffer; the outer borrow stays live.
            let inner = with_scratch_string(|inner| {
                assert!(inner.is_empty());
                inner.push_str("inner");
                inner.clone()
            });
            assert_eq!(inner, "inner");
            assert_eq!(outer.as_str(), "outer");
        });
    }
}
