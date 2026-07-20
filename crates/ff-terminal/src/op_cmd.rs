use std::ffi::{OsStr, OsString};
use std::process::Command;

use anyhow::{Context, Result};

const SERVICE_ACCOUNT_TOKEN_KEY: &str = "1Password:service_account_token";

pub async fn handle_op(args: Vec<OsString>) -> Result<()> {
    // Deliberately use the same pool, migrations, and query helper as
    // `ff secrets get`, without ever rendering the value.
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;
    let token = ff_db::pg_get_secret(&pool, SERVICE_ACCOUNT_TOKEN_KEY)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "1Password service-account token is not set; run \
                 `ff secrets set '1Password:service_account_token' <token>`"
            )
        })?;

    let op_args = translated_args(args);
    let mut command = Command::new("op");
    command.args(op_args).env("OP_SERVICE_ACCOUNT_TOKEN", token);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        Err(error).context("exec 1Password CLI `op`")
    }

    #[cfg(not(unix))]
    {
        let status = command.status().context("run 1Password CLI `op`")?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

fn translated_args(args: Vec<OsString>) -> Vec<OsString> {
    match args.first().map(OsString::as_os_str) {
        Some(first) if first == OsStr::new("get") => {
            let mut translated = vec![OsString::from("item"), OsString::from("get")];
            translated.extend(args.into_iter().skip(1));
            translated
        }
        Some(first) if first == OsStr::new("vaults") => {
            let mut translated = vec![OsString::from("vault"), OsString::from("list")];
            translated.extend(args.into_iter().skip(1));
            translated
        }
        _ => args,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn translates_convenience_commands_and_preserves_other_args() {
        assert_eq!(
            translated_args(vec!["get".into(), "My Item".into(), "--json".into()]),
            vec!["item", "get", "My Item", "--json"]
        );
        assert_eq!(
            translated_args(vec!["vaults".into(), "--format=json".into()]),
            vec!["vault", "list", "--format=json"]
        );
        assert_eq!(
            translated_args(vec!["item".into(), "list".into(), "-h".into()]),
            vec!["item", "list", "-h"]
        );
    }

    #[test]
    fn clap_forwards_hyphenated_op_arguments() {
        // The full CLI's generated clap parser is large enough to overflow the
        // test harness's small default stack in debug builds.
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                let cli =
                    crate::Cli::try_parse_from(["ff", "op", "item", "list", "-h", "--vault", "X"])
                        .expect("op arguments should bypass ff option parsing");
                let crate::Command::Op { args } = cli.command.expect("subcommand") else {
                    panic!("expected op subcommand");
                };
                assert_eq!(args, ["item", "list", "-h", "--vault", "X"]);
            })
            .expect("spawn parser test")
            .join()
            .expect("parser test thread");
    }
}
