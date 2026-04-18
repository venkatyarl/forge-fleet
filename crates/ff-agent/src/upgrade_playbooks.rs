//! Per-tool upgrade playbook snippets.
pub fn playbook_for(tool: &str, os_family: &str) -> Option<String> {
    match (tool, os_family) {
        ("gh", "linux") => Some("sudo apt-get update && sudo apt-get install --only-upgrade -y gh".into()),
        ("gh", "macos") => Some("brew upgrade gh".into()),
        ("op", "linux") => Some("sudo apt-get update && sudo apt-get install --only-upgrade -y 1password-cli".into()),
        ("op", "macos") => Some("brew upgrade --cask 1password-cli".into()),
        ("openclaw", _) => Some("curl -fsSL https://openclaw.ai/install.sh | bash".into()),
        ("mlx_lm", _) => Some("pip install -U mlx-lm".into()),
        ("vllm", _) => Some("pip install -U vllm".into()),
        ("llama.cpp", _) => Some("cd ~/llama.cpp && git pull && cmake --build build --config Release -j".into()),
        ("ff_git" | "ff", "macos") => Some(
            "cd ~/projects/forge-fleet && git pull --ff-only && \
             cargo build -p ff-terminal --release && \
             install -m 755 target/release/ff ~/.local/bin/ff && \
             codesign --force --sign - ~/.local/bin/ff".into()),
        ("ff_git" | "ff", "linux") => Some(
            "cd ~/projects/forge-fleet && git pull --ff-only && \
             cargo build -p ff-terminal --release && \
             install -m 755 target/release/ff ~/.local/bin/ff".into()),
        ("os", "linux") => Some("sudo apt-get update && sudo apt-get -y upgrade".into()),
        ("os", "macos") => Some("softwareupdate -i -a".into()),
        _ => None,
    }
}
