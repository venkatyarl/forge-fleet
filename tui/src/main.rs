//! fftui — ForgeFleet terminal UI.
//!
//! A kimi-style agent TUI that attaches to ANY LLM:
//!   fftui --backend router                        # fleet LLM router (default)
//!   fftui --backend local                         # localhost:55000
//!   fftui --backend "endpoint http://host:55008 glm-4.5-air"
//!   fftui --backend claude|codex|kimi             # cloud CLI

mod app;
mod backend;
mod markdown;
mod slash;
mod ui;

use std::path::PathBuf;

use anyhow::Result;
use backend::Backend;

#[derive(Default)]
struct Args {
    backend: Option<String>,
    model: Option<String>,
    cwd: Option<PathBuf>,
    print_help: bool,
}

fn parse_args() -> Result<Args> {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--backend" | "-b" => {
                args.backend = Some(
                    it.next()
                        .ok_or_else(|| anyhow::anyhow!("--backend needs a value"))?,
                )
            }
            "--model" | "-m" => {
                args.model = Some(
                    it.next()
                        .ok_or_else(|| anyhow::anyhow!("--model needs a value"))?,
                )
            }
            "--cwd" => {
                args.cwd = Some(PathBuf::from(
                    it.next()
                        .ok_or_else(|| anyhow::anyhow!("--cwd needs a value"))?,
                ))
            }
            "--help" | "-h" => args.print_help = true,
            other => anyhow::bail!("unknown argument: {other} (try --help)"),
        }
    }
    Ok(args)
}

const HELP: &str = "\
fftui — ForgeFleet terminal UI (attach to any LLM)

USAGE:
  fftui [--backend <SPEC>] [--model <NAME>] [--cwd <DIR>]

BACKENDS:
  router                        fleet LLM router: local-first + automatic
                                fleet failover (default)
  local                         http://localhost:55000
  endpoint <url> [model]        any OpenAI-compatible server, e.g.
                                --backend \"endpoint http://192.168.5.112:55008 glm-4.5-air\"
  claude | codex | kimi         cloud vendor CLI (single-shot, tools owned by
                                the CLI)

In the TUI: /help lists slash commands; /backend switches LLMs live.
";

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;
    if args.print_help {
        print!("{HELP}");
        return Ok(());
    }
    let cwd = match args.cwd {
        Some(d) => d,
        None => std::env::current_dir()?,
    };
    let model = args.model.unwrap_or_else(|| "auto".into());
    let backend: Backend =
        backend::parse(args.backend.as_deref().unwrap_or("router"), &model).await?;
    app::run(backend, cwd).await
}
