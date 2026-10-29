//! Native shell access for the agent: run a command with `bash -lc`
//! instead of opening a terminal emulator and typing into it.
//!
//! Safety rails (this is a real shell on the VM):
//! - Secrets are scrubbed from the child environment (anything whose
//!   name contains API_KEY / TOKEN / SECRET / PASSWORD), so the agent
//!   cannot `env`-dump the Muse key that `rosed` itself holds.
//! - 30s timeout, killed on drop (a stopped run kills its command).
//! - Output is truncated before it reaches the trace, the chat, or
//!   the brain's history — a chatty command cannot flood any of them.
//! - Dangerous commands are gated for approval by the caller
//!   (see `looks_dangerous`); this module only executes.

use std::collections::HashMap;
use std::time::Duration;

pub const SHELL_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OUTPUT_CHARS: usize = 4_000;

#[derive(Debug, Clone, PartialEq)]
pub struct ShellOutput {
    /// Exit code; `None` if killed or timed out.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub ms: u64,
}

pub fn truncate_output(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}… [truncated]")
}

/// Parent environment minus anything secret-looking.
pub fn scrubbed_env(display: &str) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = std::env::vars()
        .filter(|(key, _)| {
            let upper = key.to_uppercase();
            !["API_KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD"]
                .iter()
                .any(|needle| upper.contains(needle))
        })
        .collect();
    env.insert("DISPLAY".to_string(), display.to_string());
    env
}

/// Commands that must not run without human approval, matched
/// conservatively on the raw command text. Approval flows through
/// the same gate as irreversible UI actions.
pub fn looks_dangerous(command: &str) -> bool {
    let lower = format!(" {} ", command.to_lowercase());
    const NEEDLES: &[&str] = &[
        " rm ",
        " sudo ",
        " shutdown",
        " reboot",
        " kill ",
        " pkill",
        " apt ",
        " apt-get",
        " dpkg",
        " systemctl",
        " mkfs",
        " dd ",
        " chmod",
        " chown",
        " mv ",
        " shred",
        " passwd",
        " useradd",
        " userdel",
        " mount",
        " umount",
        " fdisk",
        " wipefs",
        "> /dev/",
    ];
    NEEDLES.iter().any(|n| lower.contains(n))
}

pub struct ShellRunner {
    display: String,
}

impl ShellRunner {
    pub fn new(display: impl Into<String>) -> Self {
        Self {
            display: display.into(),
        }
    }

    /// Run one command to completion (or timeout). Never panics on
    /// model-supplied text; spawn failures come back as `Err(String)`.
    pub async fn run(&self, command: &str) -> Result<ShellOutput, String> {
        let started = std::time::Instant::now();
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-lc")
            .arg(command)
            .env_clear()
            .envs(scrubbed_env(&self.display))
            .kill_on_drop(true);
        match tokio::time::timeout(SHELL_TIMEOUT, cmd.output()).await {
            Ok(Ok(out)) => Ok(ShellOutput {
                code: out.status.code(),
                stdout: truncate_output(&String::from_utf8_lossy(&out.stdout), MAX_OUTPUT_CHARS),
                stderr: truncate_output(&String::from_utf8_lossy(&out.stderr), MAX_OUTPUT_CHARS),
                timed_out: false,
                ms: started.elapsed().as_millis() as u64,
            }),
            Ok(Err(e)) => Err(format!("could not run shell command: {e}")),
            Err(_) => Ok(ShellOutput {
                code: None,
                stdout: String::new(),
                stderr: format!(
                    "timed out after {}s and was killed",
                    SHELL_TIMEOUT.as_secs()
                ),
                timed_out: true,
                ms: started.elapsed().as_millis() as u64,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_scrub_drops_secrets_keeps_path_and_display() {
        // SAFETY: single-threaded test binary section; vars restored after.
        std::env::set_var("ROSE_MUSE_API_KEY", "sk-should-not-leak");
        std::env::set_var("MY_GITHUB_TOKEN", "ghp_nope");
        let env = scrubbed_env(":1");
        std::env::remove_var("ROSE_MUSE_API_KEY");
        std::env::remove_var("MY_GITHUB_TOKEN");
        assert!(!env.contains_key("ROSE_MUSE_API_KEY"));
        assert!(!env.contains_key("MY_GITHUB_TOKEN"));
        assert_eq!(env.get("DISPLAY").map(String::as_str), Some(":1"));
    }

    #[test]
    fn danger_detection_is_conservative() {
        assert!(looks_dangerous("rm -rf /tmp/x"));
        assert!(looks_dangerous("sudo apt install foo"));
        assert!(looks_dangerous("echo hi && kill 1234"));
        assert!(!looks_dangerous("google-chrome https://example.com"));
        assert!(!looks_dangerous("ls -la ~/Downloads"));
        assert!(!looks_dangerous("echo warm regards")); // ' rm ' needs spaces
    }

    #[test]
    fn output_truncates() {
        assert_eq!(truncate_output("  hi  ", 10), "hi");
        let long = "x".repeat(50);
        assert!(truncate_output(&long, 10).starts_with("xxxxxxxxxx…"));
    }

    #[tokio::test]
    async fn runs_a_real_command() {
        let out = ShellRunner::new(":test")
            .run("echo hello-rose && exit 3")
            .await
            .expect("run");
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stdout, "hello-rose");
        assert!(!out.timed_out);
    }
}
