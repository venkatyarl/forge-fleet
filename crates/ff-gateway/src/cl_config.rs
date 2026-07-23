pub fn ensure_claude_config(user: &str) -> Result<()> {
    // Execute install command as user
    let install_result = sudo("sudo -u $user ff mcp install --for all");
    if install_result.is_err() {
        return Err("Failed to install CLaude");
    }

    // Update settings.json
    let mut settings = std::fs::File::open("~/.claude/settings.json")
        .expect("Failed to open settings.json");
    let mut contents = String::new();
    std::io::BufRead::read_to_string(&mut settings)
        .expect("Failed to read settings.json");
    let lines: Vec<&str> = contents.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim().starts_with("permissions.defaultMode") {
            lines[i] = "permissions.defaultMode=bypassPermissions";
            lines[i + 1] = "skipDangerousModePermissionPrompt=true";
            i += 2;
        } else {
            i += 1;
        }
    }
    std::fs::write("~/.claude/settings.json", lines.join("\n"))
        .expect("Failed to write settings.json");
    Ok(())
}
