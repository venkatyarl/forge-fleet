use crate::{CYAN, RESET, YELLOW};
use anyhow::Result;

pub async fn handle_events(cmd: crate::EventsCommand) -> Result<()> {
    use futures::StreamExt;
    let crate::EventsCommand::Tail { subject, pretty } = cmd;

    let url = std::env::var("FORGEFLEET_NATS_URL")
        .unwrap_or_else(|_| "nats://127.0.0.1:54222".to_string());
    let client = match async_nats::connect(&url).await {
        Ok(c) => c,
        Err(e) => {
            println!("{YELLOW}Could not connect to NATS at {url}: {e}{RESET}");
            println!(
                "Hint: start NATS via `docker compose up -d nats` or set FORGEFLEET_NATS_URL."
            );
            return Ok(());
        }
    };

    println!("{CYAN}▶ Tailing NATS subject `{subject}` (Ctrl-C to exit){RESET}");
    let mut sub = match client.subscribe(subject.clone()).await {
        Ok(s) => s,
        Err(e) => {
            println!("{YELLOW}NATS subscribe({subject}) failed: {e}{RESET}");
            return Ok(());
        }
    };

    while let Some(msg) = sub.next().await {
        let subj = msg.subject.to_string();
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&msg.payload) {
            let rendered = if pretty {
                serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
            } else {
                v.to_string()
            };
            println!("[{subj}] {rendered}");
        } else {
            println!("[{subj}] {}", String::from_utf8_lossy(&msg.payload));
        }
    }
    Ok(())
}
