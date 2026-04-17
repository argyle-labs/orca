use colored::Colorize;

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

    pub fn display(&self) {
        let total = self.session_input + self.session_output;
        println!(
            "{}",
            format!(
                " ↑{} ↓{} | session: {} | calls: {} ",
                fmt_tokens(self.session_input),
                fmt_tokens(self.session_output),
                fmt_tokens(total),
                self.total_calls,
            )
            .dimmed()
        );
    }

}

fn fmt_tokens(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}
