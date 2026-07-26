use std::collections::HashMap;

/// Define the technology stack detection logic for the native scan module.
/// This function should return a mapping of file patterns to detected technologies.
pub fn detect_tech_stack() -> HashMap<String, Vec<String>> {
    let mut tech_stack = HashMap::new();

    // Rust projects
    tech_stack.insert("Cargo.toml".to_string(), vec!["Rust".to_string()]);
    tech_stack.insert("*.rs".to_string(), vec!["Rust".to_string()]);

    // Node.js projects
    tech_stack.insert("package.json".to_string(), vec!["Node.js".to_string()]);
    tech_stack.insert("package-lock.json".to_string(), vec!["Node.js".to_string()]);
    tech_stack.insert("yarn.lock".to_string(), vec!["Node.js".to_string()]);

    // Python projects
    tech_stack.insert("requirements.txt".to_string(), vec!["Python".to_string()]);
    tech_stack.insert("Pipfile".to_string(), vec!["Python".to_string()]);
    tech_stack.insert("pyproject.toml".to_string(), vec!["Python".to_string()]);

    // Go projects
    tech_stack.insert("go.mod".to_string(), vec!["Go".to_string()]);
    tech_stack.insert("go.sum".to_string(), vec!["Go".to_string()]);

    // Java projects
    tech_stack.insert("pom.xml".to_string(), vec!["Java".to_string()]);
    tech_stack.insert("build.gradle".to_string(), vec!["Java".to_string()]);
    tech_stack.insert("gradle.properties".to_string(), vec!["Java".to_string()]);

    // Docker projects
    tech_stack.insert("Dockerfile".to_string(), vec!["Docker".to_string()]);
    tech_stack.insert("docker-compose.yml".to_string(), vec!["Docker".to_string()]);
    tech_stack.insert(
        "docker-compose.yaml".to_string(),
        vec!["Docker".to_string()],
    );

    // Frontend projects
    tech_stack.insert("*.html".to_string(), vec!["HTML".to_string()]);
    tech_stack.insert("*.css".to_string(), vec!["CSS".to_string()]);
    tech_stack.insert("*.js".to_string(), vec!["JavaScript".to_string()]);
    tech_stack.insert("*.ts".to_string(), vec!["TypeScript".to_string()]);
    tech_stack.insert("*.vue".to_string(), vec!["Vue.js".to_string()]);
    tech_stack.insert("*.jsx".to_string(), vec!["React".to_string()]);
    tech_stack.insert("*.tsx".to_string(), vec!["React".to_string()]);

    tech_stack
}
