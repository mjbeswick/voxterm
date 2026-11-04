//! voxterm: a voice assistant for the terminal. Gemini Live talks. Tasks make changes.
//! One process, one language: the core and the terminal UI run together.

mod acp;
mod aec;
mod audio;
mod core;
mod live;
mod memory;
mod theme;
mod ui;

use anyhow::{Result, anyhow};

/// Opt-in event log: set VOXTERM_LOG to a file path. Each line gets a timestamp.
pub fn dbg(msg: impl AsRef<str>) {
    if let Ok(path) = std::env::var("VOXTERM_LOG") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let _ = writeln!(f, "{} {}", ms % 100_000_000, msg.as_ref());
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let key = std::env::var("GEMINI_API_KEY").map_err(|_| anyhow!("GEMINI_API_KEY is not set"))?;
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--selftest") {
        return selftest(&key).await;
    }
    let cfg = core::Config::load();
    let cwd = std::env::current_dir()?;
    ui::run(cfg, key, cwd).await?;
    // Blocking terminal and audio threads can keep the runtime alive. Exit explicitly.
    std::process::exit(0);
}

/// Headless check: a task writes a file through the worker, and Live returns audio.
async fn selftest(key: &str) -> Result<()> {
    let dir = std::env::temp_dir().join(format!("voxterm-selftest-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let cmd = vec!["opencode".to_string(), "acp".to_string()];
    core::selftest_task(
        &dir,
        &cmd,
        "Create a file named hello.txt containing exactly: hi",
    )
    .await?;
    let file = std::fs::read_to_string(dir.join("hello.txt"))?;
    println!("Task: PASS, hello.txt = {:?}", file.trim());

    let s = live::Settings {
        model: core::Config::default().live_model,
        voice: "Kore".into(),
        style: "plain".into(),
        memory: String::new(),
    };
    let samples = live::once(key, &s, "Say hello in one short sentence.").await?;
    println!(
        "Live audio: {} samples at 24 kHz, {}",
        samples.len(),
        if samples.is_empty() { "FAIL" } else { "PASS" }
    );
    Ok(())
}
