//! Pre-built pipeline templates.
//!
//! Common multi-step workflows that can be used out of the box or customised.

use std::time::Duration;

use crate::graph::PipelineGraph;
use crate::step::Step;

/// Create a Rust build pipeline: cargo check → cargo build → cargo test.
///
/// Optionally scoped to a specific package with `package`.
/// The `cwd` sets the working directory for all commands.
pub fn build_pipeline(cwd: Option<&str>, package: Option<&str>) -> PipelineGraph {
    let pkg_flag = package.map(|p| format!(" -p {p}")).unwrap_or_default();
    let cwd_owned = cwd.map(|s| s.to_string());

    let mut check = Step::shell(
        "cargo-check",
        "Cargo Check",
        format!("cargo check{pkg_flag}"),
    )
    .with_timeout(Duration::from_secs(120));
    if let Some(ref d) = cwd_owned
        && let crate::step::StepKind::Shell { ref mut cwd, .. } = check.kind
    {
        *cwd = Some(d.clone());
    }

    let mut build = Step::shell(
        "cargo-build",
        "Cargo Build",
        format!("cargo build{pkg_flag}"),
    )
    .with_timeout(Duration::from_secs(300));
    if let Some(ref d) = cwd_owned
        && let crate::step::StepKind::Shell { ref mut cwd, .. } = build.kind
    {
        *cwd = Some(d.clone());
    }

    let mut test = Step::shell("cargo-test", "Cargo Test", format!("cargo test{pkg_flag}"))
        .with_timeout(Duration::from_secs(300));
    if let Some(ref d) = cwd_owned
        && let crate::step::StepKind::Shell { ref mut cwd, .. } = test.kind
    {
        *cwd = Some(d.clone());
    }

    let mut g = PipelineGraph::new();
    g.add_step(check).unwrap();
    g.add_step(build).unwrap();
    g.add_step(test).unwrap();
    g.add_dependency(&"cargo-build".into(), &"cargo-check".into())
        .unwrap();
    g.add_dependency(&"cargo-test".into(), &"cargo-build".into())
        .unwrap();
    g
}

/// Create a deployment pipeline:
///   build → verify → swap binary → health check
///
/// - `binary_src`: path to the built binary
/// - `binary_dst`: path where the live binary should be placed
/// - `health_url`: URL to GET for the health check
pub fn deploy_pipeline(
    binary_src: &str,
    binary_dst: &str,
    health_url: &str,
    cwd: Option<&str>,
) -> PipelineGraph {
    let cwd_owned = cwd.map(|s| s.to_string());

    let mut build = Step::shell("build", "Build Release", "cargo build --release")
        .with_timeout(Duration::from_secs(600));
    if let Some(ref d) = cwd_owned
        && let crate::step::StepKind::Shell { ref mut cwd, .. } = build.kind
    {
        *cwd = Some(d.clone());
    }

    let verify = Step::shell(
        "verify",
        "Verify Binary",
        format!("test -f {binary_src} && echo 'binary exists'"),
    );

    let swap = Step::shell(
        "swap",
        "Swap Binary",
        format!("cp {binary_src} {binary_dst}"),
    );

    let health = Step::shell(
        "health-check",
        "Health Check",
        format!("curl -sf {health_url} || exit 1"),
    )
    .with_retries(3, Duration::from_secs(5))
    .with_timeout(Duration::from_secs(30));

    let mut g = PipelineGraph::new();
    g.add_step(build).unwrap();
    g.add_step(verify).unwrap();
    g.add_step(swap).unwrap();
    g.add_step(health).unwrap();
    g.add_dependency(&"verify".into(), &"build".into()).unwrap();
    g.add_dependency(&"swap".into(), &"verify".into()).unwrap();
    g.add_dependency(&"health-check".into(), &"swap".into())
        .unwrap();
    g
}

/// Create an update pipeline:
///   git pull → build → test → swap binary → restart service
///
/// - `repo_dir`: path to the git repo
/// - `binary_name`: name of the binary target
/// - `service_name`: systemd/launchctl service to restart
/// - `install_dir`: where to copy the binary
pub fn update_pipeline(
    repo_dir: &str,
    binary_name: &str,
    service_name: &str,
    install_dir: &str,
) -> PipelineGraph {
    let pull = Step::shell("git-pull", "Git Pull", "git pull --ff-only")
        .with_timeout(Duration::from_secs(60));

    let build = Step::shell(
        "build",
        "Build Release",
        format!("cargo build --release -p {binary_name}"),
    )
    .with_timeout(Duration::from_secs(600));

    let test = Step::shell("test", "Run Tests", format!("cargo test -p {binary_name}"))
        .with_timeout(Duration::from_secs(300));

    let swap = Step::shell(
        "swap",
        "Swap Binary",
        format!("cp target/release/{binary_name} {install_dir}/{binary_name}"),
    );

    let restart = Step::shell(
        "restart",
        "Restart Service",
        format!("systemctl restart {service_name} 2>/dev/null || launchctl kickstart -k system/{service_name} 2>/dev/null || echo 'manual restart needed'"),
    )
    .with_timeout(Duration::from_secs(30));

    let mut g = PipelineGraph::new();

    // Set cwd on shell steps that need it.
    let set_cwd = |step: Step, dir: &str| -> Step {
        let mut s = step;
        if let crate::step::StepKind::Shell { ref mut cwd, .. } = s.kind {
            *cwd = Some(dir.to_string());
        }
        s
    };

    g.add_step(set_cwd(pull, repo_dir)).unwrap();
    g.add_step(set_cwd(build, repo_dir)).unwrap();
    g.add_step(set_cwd(test, repo_dir)).unwrap();
    g.add_step(set_cwd(swap, repo_dir)).unwrap();
    g.add_step(restart).unwrap();

    g.add_dependency(&"build".into(), &"git-pull".into())
        .unwrap();
    g.add_dependency(&"test".into(), &"build".into()).unwrap();
    g.add_dependency(&"swap".into(), &"test".into()).unwrap();
    g.add_dependency(&"restart".into(), &"swap".into()).unwrap();
    g
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_pipeline_structure() {
        let g = build_pipeline(Some("/tmp"), None);
        assert_eq!(g.len(), 3);
        let sorted = g.topological_sort().unwrap();
        let names: Vec<&str> = sorted.iter().map(|id| id.0.as_str()).collect();
        assert_eq!(names, vec!["cargo-check", "cargo-build", "cargo-test"]);
    }

    #[test]
    fn build_pipeline_with_package() {
        let g = build_pipeline(None, Some("ff-core"));
        let step = g.get_step(&"cargo-check".into()).unwrap();
        if let crate::step::StepKind::Shell { command, .. } = &step.kind {
            assert!(command.contains("-p ff-core"));
        } else {
            panic!("expected shell step");
        }
    }

    #[test]
    fn deploy_pipeline_structure() {
        let g = deploy_pipeline(
            "/tmp/binary",
            "/opt/binary",
            "http://localhost:8080/health",
            None,
        );
        assert_eq!(g.len(), 4);
        let sorted = g.topological_sort().unwrap();
        let names: Vec<&str> = sorted.iter().map(|id| id.0.as_str()).collect();
        assert_eq!(names, vec!["build", "verify", "swap", "health-check"]);
    }

    #[test]
    fn deploy_health_check_has_retries() {
        let g = deploy_pipeline(
            "/tmp/binary",
            "/opt/binary",
            "http://localhost:8080/health",
            None,
        );
        let step = g.get_step(&"health-check".into()).unwrap();
        assert_eq!(step.config.retries, 3);
    }

    #[test]
    fn update_pipeline_structure() {
        let g = update_pipeline("/opt/repo", "forgefleetd", "forgefleet", "/usr/local/bin");
        assert_eq!(g.len(), 5);
        let sorted = g.topological_sort().unwrap();
        let names: Vec<&str> = sorted.iter().map(|id| id.0.as_str()).collect();
        assert_eq!(names, vec!["git-pull", "build", "test", "swap", "restart"]);
    }

    #[test]
    fn update_pipeline_cwd_set() {
        let g = update_pipeline("/opt/repo", "forgefleetd", "forgefleet", "/usr/local/bin");
        let step = g.get_step(&"git-pull".into()).unwrap();
        if let crate::step::StepKind::Shell { cwd, .. } = &step.kind {
            assert_eq!(cwd.as_deref(), Some("/opt/repo"));
        } else {
            panic!("expected shell step");
        }
    }

    #[test]
    fn all_templates_are_dags() {
        // Verify none of the templates have cycles.
        build_pipeline(None, None).topological_sort().unwrap();
        deploy_pipeline("/a", "/b", "http://x", None)
            .topological_sort()
            .unwrap();
        update_pipeline("/a", "b", "c", "/d")
            .topological_sort()
            .unwrap();
    }
}
