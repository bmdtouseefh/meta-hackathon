//! `rosed` composition root: config -> factories -> run.
//!
//! IMPLEMENTATION.md T0.3. CLI owns config loading (file + env for secrets)
//! and selects implementations via factory fns keyed on config. Real
//! backends land in later tasks; unknown selectors fail loudly here.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use clap::{Parser, Subcommand};
use rose_core::{Actor, DesktopBackend, Policy, RoseConfig};
use rose_desktop_fake::FakeDesktop;
use rose_desktop_x11::X11Desktop;
use rose_models::{FakeBrain, ModelReply, MuseClient};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Systemd units are the single source of truth in `deploy/systemd/` and are
/// embedded into the binary, so the VM needs only this one file.
const XVNC_SERVICE: &str = include_str!("../../../deploy/systemd/xvnc.service");
const ROSED_SERVICE: &str = include_str!("../../../deploy/systemd/rosed.service");
const EXAMPLE_CONFIG: &str = include_str!("../../../config/rose.example.toml");

#[derive(Debug, Parser)]
#[command(name = "rosed", about = "Rose agent daemon (one Rust binary)")]
struct Cli {
    /// Path to config TOML. Defaults to ~/.config/rose/config.toml.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Print the resolved config (secrets redacted) and exit.
    #[arg(long)]
    print_config: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Install on this VM from this one binary: write user units + example
    /// config (never overwrites), enable lingering, (re)start services.
    /// The desktop platform itself (apt, Chrome, VNC server) still comes
    /// from deploy/setup.sh; run that once first on a fresh box.
    Install {
        /// Print what would be done without changing anything.
        #[arg(long)]
        dry_run: bool,
    },
}

/// Secrets come from env or the web Setup (server memory only), never from
/// files. Kept out of `RoseConfig` (and out of `Debug` output) so they can't
/// leak into logs or traces.
#[derive(Debug, Default)]
struct ApiKeys {
    muse_api_key: Option<String>,
}

impl ApiKeys {
    fn from_env() -> Self {
        Self {
            muse_api_key: std::env::var("ROSE_MUSE_API_KEY")
                .ok()
                .filter(|s| !s.is_empty()),
        }
    }

    /// `<set>` / `<missing>` — the value itself is never printed.
    fn muse_status(&self) -> &'static str {
        if self.muse_api_key.is_some() {
            "<set>"
        } else {
            "<missing>"
        }
    }
}

/// Which planner answers. Muse needs a key at step time (env or web Setup);
/// fake is the scripted offline double (tests / no-key demos).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrainSvc {
    Muse,
    Fake,
}

impl From<&str> for BrainSvc {
    fn from(s: &str) -> Self {
        match s {
            "fake" => BrainSvc::Fake,
            _ => BrainSvc::Muse,
        }
    }
}

#[allow(dead_code)]
struct StoreSvc {
    dir: PathBuf,
}

/// Narrow adapter: composition root (the one place allowed to touch
/// backends) exposing snapshots to the web layer. Input injection stays
/// out until the T1.4 takeover lease gates it.
struct BackendSource {
    backend: Arc<dyn DesktopBackend>,
    display: String,
    width: u32,
    height: u32,
}

#[async_trait]
impl rose_web::FrameSource for BackendSource {
    async fn jpeg(&self) -> std::result::Result<Vec<u8>, String> {
        self.backend
            .capture()
            .await
            .map(|f| f.jpeg)
            .map_err(|e| e.to_string())
    }

    fn display(&self) -> String {
        self.display.clone()
    }

    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// Everything the daemon needs, built once at startup.
#[allow(dead_code)]
struct Services {
    config: RoseConfig,
    api_keys: ApiKeys,
    desktop: Arc<dyn DesktopBackend>,
    brain: BrainSvc,
    store: StoreSvc,
}

fn default_config_path() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config/rose/config.toml"))
}

/// Load TOML file (if present) + env secrets. Missing file = built-in default.
fn load_config(path_override: Option<PathBuf>) -> Result<(RoseConfig, ApiKeys)> {
    let path = path_override.or_else(default_config_path);
    let config = match path {
        Some(p) if p.exists() => {
            let text =
                std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
            toml::from_str(&text).with_context(|| format!("parsing {}", p.display()))?
        }
        _ => RoseConfig::default(),
    };
    Ok((config, ApiKeys::from_env()))
}

fn expand_tilde(dir: &str) -> PathBuf {
    if let Some(rest) = dir.strip_prefix("~/") {
        match std::env::var("HOME") {
            Ok(h) => PathBuf::from(h).join(rest),
            Err(_) => PathBuf::from(dir),
        }
    } else {
        PathBuf::from(dir)
    }
}

fn make_desktop(config: &RoseConfig) -> Result<Arc<dyn DesktopBackend>> {
    match config.desktop.backend.as_str() {
        "fake" => Ok(Arc::new(FakeDesktop::new())),
        "x11" => Ok(Arc::new(X11Desktop::new(config.desktop.display.clone()))),
        other => bail!("unknown desktop backend: {other} (want \"x11\" | \"fake\")"),
    }
}

fn make_brain(config: &RoseConfig) -> Result<BrainSvc> {
    match config.models.brain.as_str() {
        "fake" => Ok(BrainSvc::Fake),
        // Muse builds without a key: the key is resolved per step from the
        // web Setup (server memory) or ROSE_MUSE_API_KEY, so the daemon can
        // start first and receive the key later via the UI.
        "muse" => Ok(BrainSvc::Muse),
        other => bail!("unknown brain: {other} (want \"muse\" | \"fake\")"),
    }
}

fn make_store(config: &RoseConfig) -> Result<StoreSvc> {
    // T2.1 builds the real SqliteStore on this dir; for now just resolve it.
    Ok(StoreSvc {
        dir: expand_tilde(&config.data.dir),
    })
}

fn build_services(config: RoseConfig, api_keys: ApiKeys) -> Result<Services> {
    // Guard: never serve remotely without an explicit opt-in.
    if config.server.allow_remote {
        eprintln!("WARNING: allow_remote = true: UI/API is reachable beyond localhost");
    }
    Ok(Services {
        desktop: make_desktop(&config)?,
        brain: make_brain(&config)?,
        store: make_store(&config)?,
        config,
        api_keys,
    })
}

/// Prototype Governor-lite (T2.2 builds the real one): irreversible-sounding
/// intent needs approval before anything moves.
fn needs_approval(policy: &Policy, intent: &str) -> bool {
    let lower = intent.to_lowercase();
    policy
        .require_approval_for
        .iter()
        .any(|k| !k.is_empty() && lower.contains(&k.to_lowercase()))
}

/// Single prototype step (the full loop lands in M2): capture -> brain ->
/// gate -> act. Stateless: empty history, no store, no budgets.
struct Stepper {
    desktop: Arc<dyn DesktopBackend>,
    brain: BrainSvc,
    fake: FakeBrain,
    keys: rose_web::KeySlot,
    env_key: Option<String>,
    endpoint: String,
    model: String,
    policy: Policy,
}

#[async_trait]
impl rose_web::StepRunner for Stepper {
    async fn run_step(&self, goal: &str, force: bool) -> Result<serde_json::Value, String> {
        let frame = self.desktop.capture().await.map_err(|e| e.to_string())?;
        let reply = match self.brain {
            BrainSvc::Fake => self
                .fake
                .next_action(goal, &[], &frame.jpeg)
                .await
                .map_err(|e| e.to_string())?,
            BrainSvc::Muse => {
                let key = self
                    .keys
                    .get()
                    .or_else(|| self.env_key.clone())
                    .ok_or_else(|| {
                        "no Muse API key — set it in Setup or ROSE_MUSE_API_KEY".to_string()
                    })?;
                let client = MuseClient::new(&self.endpoint, &self.model, &key)
                    .map_err(|e| e.to_string())?;
                client
                    .next_action(goal, &[], &frame.jpeg)
                    .await
                    .map_err(|e| e.to_string())?
            }
        };
        match reply {
            ModelReply::Act { action, say } => {
                if !force && needs_approval(&self.policy, &say) {
                    tracing::warn!(%say, "step gated: needs approval");
                    return Ok(rose_web::step_view(&say, false, true, false));
                }
                self.desktop
                    .act(&action, Actor::Agent)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(rose_web::step_view(&say, true, false, false))
            }
            ModelReply::Done { say } => Ok(rose_web::step_view(&say, false, false, true)),
            ModelReply::Ask { say } => Ok(rose_web::step_view(&say, false, false, false)),
            ModelReply::Fail { say } => Err(say),
        }
    }
}

/// Resolved config with secrets redacted. Acceptance command for T0.3.
fn print_config(config: &RoseConfig, keys: &ApiKeys) -> Result<()> {
    let toml = toml::to_string_pretty(config)?;
    println!("{toml}");
    println!("# secrets (env only, values never printed)");
    println!("# ROSE_MUSE_API_KEY = {}", keys.muse_status());
    Ok(())
}

fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn run_as_user(program: &str, args: &[&str], dry_run: bool) -> Result<()> {
    if dry_run {
        println!("  [dry-run] $ {program} {}", args.join(" "));
        return Ok(());
    }
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("spawning {program}"))?;
    if !status.success() {
        bail!("{program} {} exited with {status}", args.join(" "));
    }
    Ok(())
}

fn write_file(path: &Path, contents: &str, dry_run: bool) -> Result<()> {
    if dry_run {
        println!(
            "  [dry-run] write {} ({} bytes)",
            path.display(),
            contents.len()
        );
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    println!("  wrote {}", path.display());
    Ok(())
}

/// App-style install from the single binary. Idempotent; never overwrites
/// an existing config.toml.
fn cmd_install(dry_run: bool) -> Result<()> {
    let home = home_dir()?;
    let user = std::env::var("USER").unwrap_or_else(|_| "rose".to_string());
    let units_dir = home.join(".config/systemd/user");
    let config_path = home.join(".config/rose/config.toml");

    println!("rosed install (dry_run={dry_run})");

    println!("units -> {}", units_dir.display());
    write_file(&units_dir.join("xvnc.service"), XVNC_SERVICE, dry_run)?;
    // Point the unit at THIS binary (deb -> /usr/bin/rosed, source builds ->
    // ~/.local/bin/rosed), so the service always runs what you installed.
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "/usr/bin/rosed".to_string());
    let rosed_unit = ROSED_SERVICE.replace("ExecStart=/usr/bin/rosed", &format!("ExecStart={exe}"));
    if rosed_unit == ROSED_SERVICE {
        bail!("embedded rosed.service template has no ExecStart=/usr/bin/rosed line to rewrite");
    }
    if dry_run {
        println!(
            "  [dry-run] write {} (ExecStart={exe})",
            units_dir.join("rosed.service").display()
        );
    } else {
        write_file(&units_dir.join("rosed.service"), &rosed_unit, false)?;
    }

    println!("config -> {}", config_path.display());
    if config_path.exists() {
        println!("  exists, leaving it alone");
    } else {
        write_file(&config_path, EXAMPLE_CONFIG, dry_run)?;
    }

    // Lingering lets user services run without an active login. Needs sudo
    // once; failure is non-fatal (setup.sh or a manual invocation covers it).
    if dry_run {
        println!("  [dry-run] $ sudo loginctl enable-linger {user}");
    } else if let Err(e) = run_as_user("sudo", &["loginctl", "enable-linger", &user], false) {
        eprintln!("WARNING: lingering not enabled ({e}); run: sudo loginctl enable-linger {user}");
    }

    run_as_user("systemctl", &["--user", "daemon-reload"], dry_run)?;
    // Stale locks from a killed run make vncserver exit 1; clear first.
    if dry_run {
        println!("  [dry-run] clear stale :1 locks");
    } else {
        let _ = std::process::Command::new("vncserver")
            .args(["-kill", ":1"])
            .output();
        let _ = std::fs::remove_file("/tmp/.X1-lock");
        let _ = std::fs::remove_file("/tmp/.X11-unix/X1");
    }
    run_as_user(
        "systemctl",
        &["--user", "enable", "--now", "xvnc.service"],
        dry_run,
    )?;
    // rosed itself may bail until its backends land (T1.2/T3.1): warn, don't fail.
    if run_as_user(
        "systemctl",
        &["--user", "enable", "--now", "rosed.service"],
        dry_run,
    )
    .is_err()
        && !dry_run
    {
        eprintln!("WARNING: rosed.service did not start (expected until M1/M2 backends land).");
        eprintln!("  See: journalctl --user -u rosed -n 30");
    }

    println!("done. Next: ssh -L 5901:127.0.0.1:5901 <vm>, VNC to localhost:5901");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    if let Some(Command::Install { dry_run }) = cli.command {
        return cmd_install(dry_run);
    }

    let (config, keys) = load_config(cli.config)?;

    if cli.print_config {
        return print_config(&config, &keys);
    }

    let services = build_services(config, keys)?;

    // Bind guard: loopback always OK; anything else needs explicit opt-in.
    let addr: std::net::SocketAddr = services
        .config
        .server
        .bind
        .parse()
        .context("invalid server.bind (want 127.0.0.1:8787)")?;
    if !addr.ip().is_loopback() && !services.config.server.allow_remote {
        bail!("refusing to bind {addr} without allow_remote = true");
    }

    let info = services.desktop.info().await?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        bind = %addr,
        backend = %services.config.desktop.backend,
        brain = %services.config.models.brain,
        display = %info.display,
        size = %format!("{}x{}", info.width, info.height),
        "rosed starting"
    );
    // Web-set keys live in server memory (KeySlot); env key is the fallback.
    let keys = rose_web::KeySlot::default();
    let stepper = Stepper {
        desktop: Arc::clone(&services.desktop),
        brain: services.config.models.brain.as_str().into(),
        fake: FakeBrain::demo(),
        keys: keys.clone(),
        env_key: services.api_keys.muse_api_key.clone(),
        endpoint: services.config.models.endpoint.clone(),
        model: services.config.models.model.clone(),
        policy: services.config.policy.clone(),
    };
    let state = rose_web::AppState {
        source: Some(Arc::new(BackendSource {
            backend: Arc::clone(&services.desktop),
            display: info.display.clone(),
            width: info.width,
            height: info.height,
        })),
        stepper: Some(Arc::new(stepper)),
        keys,
        env_key_present: services.api_keys.muse_api_key.is_some(),
        backend: services.config.desktop.backend.clone(),
        brain: services.config.models.brain.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        started: std::time::SystemTime::now(),
    };
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    println!("rosed: UI on http://{addr} (localhost + tunnel only)");
    rose_web::serve(listener, state)
        .await
        .context("web server")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_toml_parses() {
        let text = include_str!("../../../config/rose.example.toml");
        let cfg: RoseConfig = toml::from_str(text).expect("example must parse");
        assert_eq!(cfg.server.bind, "127.0.0.1:8787");
        assert!(!cfg.server.allow_remote);
        assert_eq!(cfg.desktop.backend, "x11");
        assert_eq!(cfg.policy.max_steps, 40);
    }

    #[test]
    fn missing_file_falls_back_to_default() {
        let (cfg, _) =
            load_config(Some(PathBuf::from("/nonexistent/rose-test.toml"))).expect("load");
        assert_eq!(cfg, RoseConfig::default());
    }

    #[tokio::test]
    async fn fake_selectors_build_and_capture() {
        let mut cfg = RoseConfig::default();
        cfg.desktop.backend = "fake".to_string();
        cfg.models.brain = "fake".to_string();
        let svc = build_services(cfg, ApiKeys::default()).expect("fake services build");
        let info = svc.desktop.info().await.expect("info");
        assert_eq!((info.width, info.height), (1600, 900));
        let frame = svc.desktop.capture().await.expect("capture");
        assert!(frame.jpeg.starts_with(&[0xff, 0xd8]), "is jpeg");
    }

    #[test]
    fn muse_builds_without_key_and_gates_risky_intents() {
        // The daemon starts keyless; the key arrives later via Setup/env and
        // is resolved per step.
        let mut cfg = RoseConfig::default(); // backend x11 (lazy), brain muse
        cfg.desktop.backend = "fake".to_string(); // keep this test display-free
        build_services(cfg, ApiKeys::default()).expect("muse selector builds");
        assert!(needs_approval(&Policy::default(), "Click Pay now"));
        assert!(!needs_approval(&Policy::default(), "Clicking Export"));
    }

    fn test_stepper(brain: BrainSvc) -> Stepper {
        Stepper {
            desktop: Arc::new(FakeDesktop::new()),
            brain,
            fake: FakeBrain::demo(),
            keys: rose_web::KeySlot::default(),
            env_key: None,
            endpoint: "https://api.meta.ai/v1".to_string(),
            model: "muse-spark-1.3-contributor".to_string(),
            policy: Policy::default(),
        }
    }

    #[tokio::test]
    async fn fake_brain_step_acts_on_fake_desktop() {
        use rose_web::StepRunner;
        let view = test_stepper(BrainSvc::Fake)
            .run_step("click the first cell", false)
            .await
            .expect("step");
        assert_eq!(view["acted"], true);
        assert_eq!(view["approval_needed"], false);
    }

    #[tokio::test]
    async fn muse_step_without_key_errors_clearly() {
        use rose_web::StepRunner;
        let err = test_stepper(BrainSvc::Muse)
            .run_step("do a thing", false)
            .await
            .unwrap_err();
        assert!(err.contains("API key"), "{err}");
    }

    #[tokio::test]
    async fn risky_step_needs_approval_until_forced() {
        use rose_web::StepRunner;
        let pay_click = || rose_models::ModelReply::Act {
            action: rose_core::Action::Click {
                target: rose_core::Target::Point { x: 10, y: 10 },
                button: rose_core::MouseButton::Left,
            },
            say: "Clicking Pay now".to_string(),
        };
        let stepper = Stepper {
            // One reply per step call: first gated (no act), second forced.
            fake: FakeBrain::scripted(vec![pay_click(), pay_click()]),
            ..test_stepper(BrainSvc::Fake)
        };
        let gated = stepper.run_step("pay", false).await.expect("gated");
        assert_eq!(gated["acted"], false);
        assert_eq!(gated["approval_needed"], true);
        let forced = stepper.run_step("pay", true).await.expect("forced");
        assert_eq!(forced["acted"], true);
    }

    #[test]
    fn x11_selector_constructs_without_a_display() {
        // Construction is lazy: no X connection until first capture/act.
        let mut cfg = RoseConfig::default();
        cfg.desktop.backend = "x11".to_string();
        cfg.models.brain = "fake".to_string();
        build_services(cfg, ApiKeys::default()).expect("x11 selector builds");
    }

    #[test]
    fn unknown_selectors_rejected() {
        let mut cfg = RoseConfig::default();
        cfg.desktop.backend = "wayland".to_string();
        cfg.models.brain = "fake".to_string();
        let err = build_services(cfg, ApiKeys::default())
            .map(|_| ())
            .unwrap_err();
        assert!(err.to_string().contains("unknown desktop backend"), "{err}");
    }
}
