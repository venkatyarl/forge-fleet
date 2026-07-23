use std::sync::mpsc;
use std::path::Path;
use std::fs::File;
use std::io::{self, Write};

pub fn setup_work_log_dir(work_item: &str) -> Result<mpsc::Receiver<String>> {
    let log_dir = format!("/home/veronica/.forgefleet/logs/{}/", work_item);
    let log_file = format!("{}/{}", log_dir, "log");

    // Idempotent path creation
    if !Path::is_dir(&log_dir) {
        std::fs::create_dir(&log_dir)?;
    }

    // Setup internal file-writer
    let (sender, receiver) = mpsc::channel();
    let log_file = format!("{}{}", log_dir, "log");
    let file = File::create(log_file)?;
    let writer = io::BufWriter::new(file);
    let _ = std::thread::spawn(move || {
        loop {
            match receiver.recv() {
                Ok(msg) => {
                    writeln!(writer, "{}", msg)?;
                },
                Err(e) => {
                    // Handle error if needed
                },
            }
        }
    });

    Ok(receiver)
}
