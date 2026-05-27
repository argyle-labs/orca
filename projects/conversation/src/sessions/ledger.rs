/// Tracks cumulative token usage for one interactive session.
#[derive(Debug, Default)]
pub struct TokenLedger {
    pub session_input: u32,
    pub session_output: u32,
    pub total_calls: u32,
}

impl TokenLedger {
    pub fn record(&mut self, input: u32, output: u32) {
        self.session_input += input;
        self.session_output += output;
        self.total_calls += 1;
    }

    pub fn format(&self) -> String {
        let total = self.session_input + self.session_output;
        format!(
            " ↑{} ↓{} | session: {} | calls: {} ",
            fmt_tokens(self.session_input),
            fmt_tokens(self.session_output),
            fmt_tokens(total),
            self.total_calls,
        )
    }
}

pub fn fmt_tokens(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_tokens_zero() {
        assert_eq!(fmt_tokens(0), "0");
    }

    #[test]
    fn fmt_tokens_small() {
        assert_eq!(fmt_tokens(999), "999");
    }

    #[test]
    fn fmt_tokens_thousands() {
        assert_eq!(fmt_tokens(1_000), "1.0k");
        assert_eq!(fmt_tokens(1_500), "1.5k");
        assert_eq!(fmt_tokens(500_000), "500.0k");
    }

    #[test]
    fn fmt_tokens_millions() {
        assert_eq!(fmt_tokens(1_000_000), "1.0M");
        assert_eq!(fmt_tokens(2_500_000), "2.5M");
    }

    #[test]
    fn ledger_record_accumulates() {
        let mut l = TokenLedger::default();
        l.record(100, 50);
        l.record(200, 75);
        assert_eq!(l.session_input, 300);
        assert_eq!(l.session_output, 125);
        assert_eq!(l.total_calls, 2);
    }

    #[test]
    fn ledger_format_contains_key_fields() {
        let mut l = TokenLedger::default();
        l.record(1_000, 500);
        let s = l.format();
        assert!(s.contains("1.0k"), "input fmt missing: {s}");
        assert!(s.contains("500"), "output missing: {s}");
        assert!(s.contains("calls: 1"), "calls missing: {s}");
    }
}
