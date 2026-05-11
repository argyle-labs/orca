use super::{Session, util};
use colored::Colorize;

impl Session {
    pub(super) fn print_banner(&self) {
        let emoji = util::agent_emoji(&self.active_agent);
        self.out("");
        if let Some(p) = &self.project {
            self.out_fmt(format!("  {emoji} orca  ·  {p}").bold());
        } else {
            self.out_fmt(format!("  {emoji} orca").bold());
        }
        self.out(&format!(
            "  {}  {}",
            format!("{emoji} @{}", self.active_agent).cyan(),
            format!("{}:{}", self.backend.name(), self.backend.model_id()).dimmed()
        ));
        self.out(&"  /help · exit to quit".dimmed().to_string());
        self.out("");
    }

    pub(super) fn print_help(&self) {
        self.out(&"Navigation:".green().to_string());
        self.out("  /model            list models + interactive picker");
        self.out("  /model <spec>     switch directly  (lmstudio:qwen3, claude-sonnet-4-6)");
        self.out("  clear             clear conversation history");
        self.out("");
        self.out(&"Context:".green().to_string());
        self.out("  /context          show messages, token usage, context window %");
        self.out("  /tokens           token ledger");
        self.out("  /agent            show active agent");
        self.out("  /system           show current system prompt");
        self.out("");
        self.out(&"Logging (Otter):".green().to_string());
        self.out("  /flag [note]      mark last message as important");
        self.out("  /log              show current session log path");
        self.out("  /search <query>   search all session logs for a keyword");
        self.out("  /sessions         list recent sessions");
        self.out("  /recall <id>      replay a session's messages");
        self.out("");
        self.out(&"Background jobs:".green().to_string());
        self.out("  /bg <prompt>      run a prompt in the background");
        self.out("  /jobs             list background jobs");
        self.out("  /output <id>      view a job's output  (e.g. /output 1)");
        self.out("  /cancel <id>      cancel a running job");
        self.out("");
        self.out(&"Escalation:".green().to_string());
        self.out("  /escalate <q>     send question to Claude, inject answer into context");
        self.out("");
        self.out(&"Preferences:".green().to_string());
        self.out("  /narration        toggle Orca/Otter narration on/off");
        self.out("");
        self.out(&"Maintenance:".green().to_string());
        self.out("  /cleanup          find and kill orphaned orca processes");
        self.out("");
        self.out(&"Session:".green().to_string());
        self.out("  exit              quit  (also: quit, q, bye, ^D)");
    }

    pub(super) fn warn_phantom_processes(&self) {
        let others = util::find_other_orca_pids();
        if !others.is_empty() {
            let pids: Vec<String> = others.iter().map(|p| p.to_string()).collect();
            self.out_fmt(
                format!(
                    "  {} other orca process(es) running: {}",
                    others.len(),
                    pids.join(", ")
                )
                .yellow(),
            );
            self.out(&"  run /cleanup to kill them".dimmed().to_string());
            self.out("");
        }
    }

    pub(super) fn cleanup_phantom_processes(&self) {
        let others = util::find_other_orca_pids();
        if others.is_empty() {
            self.out(&"no phantom orca processes found.".green().to_string());
            return;
        }

        self.out_fmt(format!("found {} other orca process(es):", others.len()).yellow());
        for pid in &others {
            let info = std::process::Command::new("ps")
                .args(["-p", &pid.to_string(), "-o", "pid,etime,args"])
                .output();
            if let Ok(out) = info {
                let text = String::from_utf8_lossy(&out.stdout);
                for line in text.lines().skip(1) {
                    self.out(&format!("  {}", line.trim()));
                }
            }
        }
        self.out(
            &"  use `kill <pid>` to stop them manually"
                .dimmed()
                .to_string(),
        );
    }
}
