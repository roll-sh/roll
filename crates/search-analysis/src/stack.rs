//! Stack budgets for the recursive pattern parsers and the regex backtracker.
//!
//! The Java-regex and Lucene-RegExp parsers recurse once per nesting level, as Java's and
//! Lucene's do. Both bound the nesting (a pattern deeper than the bound is a 400 error, where
//! Elasticsearch would overflow its stack and, for Lucene patterns, die), but the bound has to
//! hold on whatever thread compiles the pattern: a tokio worker has 2 MB, and an unoptimized
//! build spends several KB of stack per level. So a pattern that could be deeply nested (many
//! opening brackets) is compiled on a short-lived thread with a large stack ([`deep`]); the
//! stack is reserved virtual memory, only the pages the recursion touches are committed.
//! Ordinary patterns compile in place.
//!
//! The backtracker runs on the caller's thread for every match, so it cannot move; it measures
//! the stack it has used instead ([`Depth`]) and gives up (no match) past a fixed budget, which
//! is what Java's matcher would turn into a `StackOverflowError`.

/// Opening brackets above which [`deep`] moves the work to a large stack.
const SHALLOW_OPENS: usize = 64;

/// Stack of the thread [`deep`] starts.
const DEEP_STACK: usize = 64 << 20;

/// Runs `f`, on a thread with a large stack when `opens` (an upper bound on the nesting of what
/// `f` parses) is high. Returns `None` if that thread could not be started.
pub(crate) fn deep<R: Send>(opens: usize, f: impl FnOnce() -> R + Send) -> Option<R> {
    if opens <= SHALLOW_OPENS {
        return Some(f());
    }
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .name("roll-pattern-compile".into())
            .stack_size(DEEP_STACK)
            .spawn_scoped(s, f)
            .ok()?
            .join()
            .ok()
    })
}

/// Counts the characters of `pattern` that can open a nesting level.
pub(crate) fn opens(pattern: &str, open: &[u8]) -> usize {
    pattern.bytes().filter(|b| open.contains(b)).count()
}

/// The address of a local: the stack pointer, near enough. Stacks grow down on every target
/// we build for.
#[inline(always)]
fn stack_pointer() -> usize {
    let marker = 0u8;
    std::hint::black_box(&marker) as *const u8 as usize
}

/// Stack used since a starting point, checked against a budget.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Depth {
    base: usize,
}

/// Stack the backtracker may use below its entry point. Callers run on threads with at least
/// 2 MB; this leaves room for everything above the analysis call.
pub(crate) const MATCH_STACK_BUDGET: usize = 768 << 10;

impl Depth {
    #[inline(always)]
    pub fn start() -> Depth {
        Depth {
            base: stack_pointer(),
        }
    }

    /// Whether more than `budget` bytes of stack are in use since [`Depth::start`].
    #[inline(always)]
    pub fn exceeded(&self, budget: usize) -> bool {
        self.base.saturating_sub(stack_pointer()) > budget
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[inline(never)]
    fn recurse(d: Depth, n: u32) -> u32 {
        let pad = std::hint::black_box([0u8; 256]);
        if d.exceeded(64 << 10) {
            return n;
        }
        recurse(d, n + 1) + pad[0] as u32
    }

    #[test]
    fn depth_measures_the_stack() {
        let n = recurse(Depth::start(), 0);
        // At least 256 bytes per level, so no more than 256 levels fit 64 KB.
        assert!(n > 0 && n <= 256, "{n}");
    }

    #[test]
    fn deep_runs_on_a_large_stack() {
        assert_eq!(deep(0, || 1), Some(1));
        let r = std::thread::Builder::new()
            .stack_size(256 << 10)
            .spawn(|| deep(1000, || recurse(Depth::start(), 0) + recurse_far(0)))
            .unwrap()
            .join()
            .unwrap();
        assert!(r.is_some());
        assert_eq!(opens("(a[b]\\(", b"(["), 3);
    }

    /// Uses about 4 MB of stack, far more than the calling thread has.
    #[inline(never)]
    fn recurse_far(n: u32) -> u32 {
        let pad = std::hint::black_box([1u8; 4096]);
        if n == 1000 {
            return 0;
        }
        recurse_far(n + 1) + pad[0] as u32 - 1
    }
}
