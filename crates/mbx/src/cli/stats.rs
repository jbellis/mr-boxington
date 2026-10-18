use crate::config::{Config, SavingsStyle};
use eyre::Result;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(usage_rs::Args)]
pub(super) struct StatsArgs {
    /// Print a stable machine-readable report, with byte counts and nanoseconds.
    #[usage(long)]
    json: bool,
    /// Show compile memory estimates and a read-only scheduler activity snapshot.
    #[usage(long)]
    scheduler: bool,
}

pub(super) fn run(config: &Config, args: StatsArgs, style: SavingsStyle) -> Result<ExitCode> {
    let mut report = crate::stats::collect(&config.store_dir(), &config.target.root)?;
    if args.scheduler {
        // A workspace `.mbx.toml` can change the scheduler settings its builds
        // use, so the charges are computed with the nearest one, and say so.
        let settings = nearest_workspace_settings(&std::env::current_dir()?);
        let mut config = config.clone();
        if let Some(workspace) = settings.as_deref().and_then(Path::parent) {
            config.apply_workspace_policy(workspace)?;
        }
        report.scheduler = Some(crate::scheduler::scheduler_report(
            &config,
            settings.as_deref(),
        ));
    }
    if args.json {
        super::cache::print_json(&report)?;
    } else {
        println!("{}", report.text(style == SavingsStyle::Quips));
    }
    Ok(ExitCode::SUCCESS)
}

/// The `.mbx.toml` of the closest enclosing directory that has one.
fn nearest_workspace_settings(working_dir: &Path) -> Option<PathBuf> {
    working_dir
        .ancestors()
        .map(|directory| directory.join(".mbx.toml"))
        .find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nearest_workspace_settings_file_is_used() {
        let root = tempfile::tempdir().unwrap();
        let member = root.path().join("crates/member");
        std::fs::create_dir_all(&member).unwrap();
        assert_eq!(nearest_workspace_settings(&member), None);
        std::fs::write(root.path().join(".mbx.toml"), "").unwrap();
        assert_eq!(
            nearest_workspace_settings(&member),
            Some(root.path().join(".mbx.toml"))
        );
    }
}
