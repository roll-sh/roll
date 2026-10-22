//! A conservative size estimate of the automaton a Lucene regular expression expands to, so
//! patterns like `(a|aa){1000}{1000}` (a few bytes that unroll into millions of states) are
//! rejected before any automaton is built.

/// Upper bound of NFA states the engine accepts before determinization.
pub const MAX_EXPANDED_STATES: u64 = 200_000;

struct P<'a> {
    s: &'a [u8],
    i: usize,
}

impl P<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn alt(&mut self, depth: u32) -> u64 {
        if depth > 1000 {
            return u64::MAX;
        }
        let mut total = self.concat(depth);
        while matches!(self.peek(), Some(b'|') | Some(b'&')) {
            self.i += 1;
            total = total.saturating_add(self.concat(depth));
        }
        total
    }

    fn concat(&mut self, depth: u32) -> u64 {
        let mut total = 0u64;
        while let Some(c) = self.peek() {
            if c == b'|' || c == b'&' || c == b')' {
                break;
            }
            total = total.saturating_add(self.repeat(depth));
        }
        total.max(1)
    }

    fn number(&mut self) -> Option<u64> {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        std::str::from_utf8(&self.s[start..self.i]).ok()?.parse().ok()
    }

    fn repeat(&mut self, depth: u32) -> u64 {
        let mut s = self.atom(depth);
        loop {
            match self.peek() {
                Some(b'*') | Some(b'+') | Some(b'?') => {
                    self.i += 1;
                    s = s.saturating_add(1);
                }
                Some(b'{') => {
                    let save = self.i;
                    self.i += 1;
                    let Some(n) = self.number() else {
                        self.i = save + 1;
                        return s;
                    };
                    let m = if self.peek() == Some(b',') {
                        self.i += 1;
                        self.number().unwrap_or(n.saturating_add(1))
                    } else {
                        n
                    };
                    if self.peek() == Some(b'}') {
                        self.i += 1;
                    }
                    s = s.saturating_mul(m.max(n).max(1)).saturating_add(1);
                }
                _ => return s,
            }
        }
    }

    fn atom(&mut self, depth: u32) -> u64 {
        let Some(c) = self.peek() else { return 1 };
        self.i += 1;
        match c {
            b'(' => {
                let s = self.alt(depth + 1);
                if self.peek() == Some(b')') {
                    self.i += 1;
                }
                s.saturating_add(1)
            }
            b'[' => {
                while let Some(c) = self.peek() {
                    self.i += 1;
                    if c == b'\\' {
                        self.i += 1;
                    } else if c == b']' {
                        break;
                    }
                }
                2
            }
            b'"' => {
                let start = self.i;
                while self.peek().is_some_and(|c| c != b'"') {
                    self.i += 1;
                }
                let n = (self.i - start) as u64;
                self.i += 1;
                n.max(1)
            }
            b'<' => {
                let start = self.i;
                while self.peek().is_some_and(|c| c != b'>') {
                    self.i += 1;
                }
                let n = (self.i - start) as u64;
                self.i += 1;
                n.saturating_mul(10).max(1)
            }
            b'~' => self.repeat(depth).saturating_mul(2).saturating_add(1),
            b'\\' => {
                self.i += 1;
                1
            }
            _ => 1,
        }
    }
}

/// Estimated states of `pattern`'s automaton (saturating).
pub fn estimated_states(pattern: &str) -> u64 {
    let mut p = P { s: pattern.as_bytes(), i: 0 };
    let mut total = 0u64;
    while p.i < p.s.len() {
        total = total.saturating_add(p.alt(0));
        if p.peek() == Some(b')') {
            p.i += 1;
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates() {
        assert!(estimated_states("abc") < 10);
        assert!(estimated_states("[a-z]{1,100}") < 1000);
        assert!(estimated_states("(abc){1000}") < 10_000);
        assert!(estimated_states("(a|aa){1000}{1000}") > MAX_EXPANDED_STATES);
        assert!(estimated_states("((a{100}){100}){100}") > MAX_EXPANDED_STATES);
        assert!(estimated_states("((((((((((((") < 100);
        assert!(estimated_states("\\") < 10);
    }
}
