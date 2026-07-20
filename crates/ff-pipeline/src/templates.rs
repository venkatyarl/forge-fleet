//! Pre-built pipeline templates.
//!
//! Common multi-step workflows that can be used out of the box or customised.

use std::time::Duration;

use crate::graph::PipelineGraph;
use crate::step::Step;

/// Cargo environment variables that route dependency fetches through the
/// ForgeFleet local mirror.
///
/// When a mirror URL is provided, crates.io is replaced with the
/// `forgefleet-mirror` source so that every `cargo` fetch checks the mirror
/// first.
fn mirror_env(mirror_url: Option<&str>) -> Vec<(String, String)> {
    mirror_url
        .map(|url| {
            vec![
                (
                    "CARGO_SOURCE_CRATES_IO_REPLACE_WITH".to_string(),
                    "forgefleet-mirror".to_string(),
                ),
                (
                    "CARGO_SOURCE_FORGEFLEET_MIRROR_REGISTRY".to_string(),
                    url.to_string(),
                ),
            ]
        })
        .unwrap_or_default()
}

/// Create a Rust build pipeline: cargo check → cargo build → cargo test.
///
/// Optionally scoped to a specific package with `package`.
/// The `cwd` sets the working directory for all commands.
///
/// If `FORGEFLEET_MIRROR_URL` is set in the environment, a `mirror-fetch`
/// step is inserted first and all cargo steps are configured to fetch
/// dependencies through that mirror.
pub fn build_pipeline(cwd: Option<&str>, package: Option<&str>) -> PipelineGraph {
    let mirror_url = std::env::var("FORGEFLEET_MIRROR_URL").ok();
    build_pipeline_with_mirror(cwd, package, mirror_url.as_deref())
}

/// Create a Rust build pipeline with an explicit mirror URL.
///
/// `mirror_url` takes precedence over `FORGEFLEET_MIRROR_URL`. Pass `None`
/// to disable mirror fetching even when the environment variable is set.
pub fn build_pipeline_with_mirror(
    cwd: Option<&str>,
    package: Option<&str>,
    mirror_url: Option<&str>,
) -> PipelineGraph {
    let pkg_flag = package.map(|p| format!(" -p {p}")).unwrap_or_default();
    let cwd_owned = cwd.map(|s| s.to_string());

    let apply_cwd_and_mirror = |mut step: Step| -> Step {
        if let crate::step::StepKind::Shell {
            ref mut cwd,
            ref mut env,
            ..
        } = step.kind
        {
            *cwd = cwd_owned.clone();
            env.extend(mirror_env(mirror_url));
        }
        step
    };

    let check = apply_cwd_and_mirror(
        Step::shell(
            "cargo-check",
            "Cargo Check",
            format!("cargo check{pkg_flag}"),
        )
        .with_timeout(Duration::from_secs(120)),
    );

    let build = apply_cwd_and_mirror(
        Step::shell(
            "cargo-build",
            "Cargo Build",
            format!("cargo build{pkg_flag}"),
        )
        .with_timeout(Duration::from_secs(300)),
    );

    let test = apply_cwd_and_mirror(
        Step::shell("cargo-test", "Cargo Test", format!("cargo test{pkg_flag}"))
            .with_timeout(Duration::from_secs(300)),
    );

    let mut g = PipelineGraph::new();

    if mirror_url.is_some() {
        let fetch = apply_cwd_and_mirror(
            Step::shell(
                "mirror-fetch",
                "Mirror Fetch",
                format!("cargo fetch{pkg_flag}"),
            )
            .with_timeout(Duration::from_secs(300)),
        );
        g.add_step(fetch).unwrap();
    }

    g.add_step(check).unwrap();
    g.add_step(build).unwrap();
    g.add_step(test).unwrap();

    if mirror_url.is_some() {
        g.add_dependency(&"cargo-check".into(), &"mirror-fetch".into())
            .unwrap();
        g.add_dependency(&"cargo-build".into(), &"mirror-fetch".into())
            .unwrap();
        g.add_dependency(&"cargo-test".into(), &"mirror-fetch".into())
            .unwrap();
    }

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
    fn build_pipeline_with_mirror_injects_fetch_step() {
        let g = build_pipeline_with_mirror(Some("/tmp"), None, Some("http://localhost:8765"));
        assert_eq!(g.len(), 4);
        let sorted = g.topological_sort().unwrap();
        let names: Vec<&str> = sorted.iter().map(|id| id.0.as_str()).collect();
        assert_eq!(
            names,
            vec!["mirror-fetch", "cargo-check", "cargo-build", "cargo-test"]
        );

        let fetch = g.get_step(&"mirror-fetch".into()).unwrap();
        if let crate::step::StepKind::Shell { env, .. } = &fetch.kind {
            assert!(env.iter().any(|(k, v)| {
                k == "CARGO_SOURCE_CRATES_IO_REPLACE_WITH" && v == "forgefleet-mirror"
            }));
            assert!(env.iter().any(|(k, v)| {
                k == "CARGO_SOURCE_FORGEFLEET_MIRROR_REGISTRY" && v == "http://localhost:8765"
            }));
        } else {
            panic!("expected shell step");
        }

        let check = g.get_step(&"cargo-check".into()).unwrap();
        if let crate::step::StepKind::Shell { env, .. } = &check.kind {
            assert!(
                env.iter()
                    .any(|(k, _)| k == "CARGO_SOURCE_CRATES_IO_REPLACE_WITH")
            );
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
