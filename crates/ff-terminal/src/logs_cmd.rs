use crate::{CYAN, RESET, YELLOW};
use anyhow::Result;

pub async fn handle_logs(
    computer: Option<String>,
    service: Option<String>,
    _tail: usize,
) -> Result<()> {
    use futures::StreamExt;
    let url = std::env::var("FORGEFLEET_NATS_URL")
        .unwrap_or_else(|_| "nats://127.0.0.1:4222".to_string());
    let client = match async_nats::connect(&url).await {
        Ok(c) => c,
        Err(e) => {
            println!("{YELLOW}Could not connect to NATS at {url}: {e}{RESET}");
            println!(
                "Set FORGEFLEET_NATS_URL or ensure nats:// is reachable (docker: `forgefleet-nats`)."
            );
            return Ok(());
        }
    };

    let computer = computer.as_deref().unwrap_or("*");
    let service = service.as_deref().unwrap_or("*");
    let subject = format!("logs.{computer}.{service}.>");
    println!("{CYAN}▶ Tailing NATS subject `{subject}` (Ctrl-C to exit){RESET}");

    let mut sub = match client.subscribe(subject.clone()).await {
        Ok(s) => s,
        Err(e) => {
            println!("{YELLOW}NATS subscribe({subject}) failed: {e}{RESET}");
            return Ok(());
        }
    };

    while let Some(msg) = sub.next().await {
        let subject = msg.subject.to_string();
        let body = String::from_utf8_lossy(&msg.payload);
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&msg.payload) {
            let ts = v.get("ts").and_then(|t| t.as_str()).unwrap_or("?");
            let lvl = v.get("level").and_then(|l| l.as_str()).unwrap_or("info");
            let msg_txt = v.get("message").and_then(|m| m.as_str()).unwrap_or("");
            let target = v.get("target").and_then(|t| t.as_str()).unwrap_or("");
            println!("[{ts}] {lvl:<5} {target} {msg_txt}  ({subject})");
        } else {
            println!("[{subject}] {body}");
        }
    }
    Ok(())
}
