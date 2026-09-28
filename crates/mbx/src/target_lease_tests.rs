//! A wrapped command holds a usage lease on its managed target directory for
//! as long as it runs, so collection cannot delete the executables a test,
//! bench, or run command is still using after Cargo has released its lock.
//!
//! On Linux these are `flock` locks, which belong to an open file description:
//! two files opened separately in one process conflict just as two processes
//! would, so most of these tests stand in for the wrapped command in-process.
//! Each test still runs in a process of its own: see [`in_own_process`].

use super::*;
use crate::config::TargetSettings;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread::JoinHandle;

/// Tells a re-run of this test binary to hold a view's lease and wait.
const HOLD_VIEW_LOCK_ENV: &str = "MBX_TEST_HOLD_VIEW_LOCK";

/// Tells a re-run of this test binary that it is the process a test runs in.
const OWN_PROCESS_ENV: &str = "MBX_TEST_OWN_PROCESS";

/// The name the test harness knows a test by, from its module's
/// `module_path!()` and its function name.
///
/// `module_path!` starts with the crate's name, which the harness leaves out.
fn test_name(module: &str, test: &str) -> String {
    let module = module.split_once("::").map_or(module, |(_, rest)| rest);
    format!("{module}::{test}")
}

/// Run the rest of the calling test in a process of its own.
///
/// `flock` locks belong to the open file description, and a process that forks
/// hands the child a copy of every descriptor until it execs. The test binary
/// forks all the time -- `place` runs `git`, other tests run Cargo -- so in the
/// shared process a lease this test has just released can still be held for a
/// moment by an unrelated child, and a non-blocking reservation would find the
/// view in use. Alone in its process, the only children are this test's own,
/// and each is waited for before the next step.
///
/// `module` is the caller's `module_path!()`, so the sibling test module can
/// use this too. Returns true in that process, where the caller goes on with
/// the test, and false in the one that started it, where the caller returns.
pub(super) fn in_own_process(module: &str, test: &str) -> bool {
    if std::env::var_os(OWN_PROCESS_ENV).is_some() {
        return true;
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            test_name(module, test).as_str(),
            "--exact",
            "--test-threads=1",
        ])
        .env(OWN_PROCESS_ENV, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // A name that matched nothing would run no test and still succeed.
    assert!(
        output.status.success() && stdout.contains("test result: ok. 1 passed"),
        "{test} failed in its own process ({}):\n{stdout}\n{stderr}",
        output.status
    );
    false
}

fn lease_test_config(root: &Path, views: bool) -> Config {
    Config {
        cache_dir: root.join("cache"),
        shims_dir: root.join("cache").join("shims"),
        stats_report: None,
        ar_determinism: "auto".into(),
        verify: false,
        verify_sample_rate: 0,
        incremental: false,
        eager_incremental: false,
        share_out_dir: false,
        restore_hardlink: true,
        share_workspace_root: false,
        build_script_execution: false,
        events: false,
        cc: false,
        cc_store_path_specific: true,
        forward_compiler_notifications: true,
        remote: Default::default(),
        http: Default::default(),
        gc: Default::default(),
        scheduler: Default::default(),
        linker: Default::default(),
        target: TargetSettings {
            views,
            seed: false,
            root: root.join("targets"),
        },
    }
}

/// A checkout with the default target directory, ready to be managed.
fn lease_checkout(root: &Path, name: &str) -> PathBuf {
    let workspace = root.join(name);
    std::fs::create_dir_all(&workspace).unwrap();
    workspace
}

fn age_lease_view(root: &Path, workspace_root: &Path, updated_secs: u64) {
    let path = view_record_path(root, workspace_root);
    let mut record: ViewRecord = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    record.updated_secs = updated_secs;
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
}

/// Place a managed target for a new checkout, give it some outputs, and date
/// its record far enough back that an age limit selects it.
///
/// No `.cargo-lock` is created anywhere: compilation has finished, so only a
/// usage lease can keep the view.
fn expired_view(root: &Path, config: &Config, name: &str) -> (PathBuf, PathBuf) {
    let workspace = lease_checkout(root, name);
    let view = place(config, &workspace, &workspace.join("target"), false).unwrap();
    std::fs::write(view.join("artifact"), vec![0_u8; 4_096]).unwrap();
    age_lease_view(&config.target.root, &workspace, 1);
    (workspace, view)
}

/// Collect with an age limit every view set up by [`expired_view`] exceeds.
fn collect_expired(root: &Path) -> CollectionOutcome {
    collect(root, None, Some(Duration::from_secs(10)), false).unwrap()
}

/// Compilation has finished, so Cargo holds no lock in the view, but the
/// wrapped command is still running the executables it built. Collection must
/// leave the view alone until the command releases its lease.
#[test]
fn a_view_a_wrapped_command_is_still_using_is_kept() {
    if !in_own_process(
        module_path!(),
        "a_view_a_wrapped_command_is_still_using_is_kept",
    ) {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let config = lease_test_config(directory.path(), true);
    let (workspace, view) = expired_view(directory.path(), &config, "project");

    let lease = ViewLease::acquire(&config.target.root, &workspace).unwrap();
    assert!(
        view_lock_path(&view).exists(),
        "the lease lives in a lock file beside the view's record"
    );
    assert!(is_recorded(&config.target.root, &workspace));

    let outcome = collect_expired(&config.target.root);

    assert_eq!(outcome.kept_active_views, 1);
    assert_eq!(outcome.removed_views, 0);
    assert!(view.join("artifact").exists());
    assert!(view_record_path(&config.target.root, &workspace).exists());
    assert!(is_recorded(&config.target.root, &workspace));

    // The command has finished: nothing protects the expired view any more.
    drop(lease);
    let outcome = collect_expired(&config.target.root);

    assert_eq!(outcome.removed_live_views, 1);
    assert!(!view.exists());
    assert!(!is_recorded(&config.target.root, &workspace));
}

/// A command can start after collection has chosen what to remove. The lease
/// it takes in that window must still win over the removal.
#[test]
fn a_lease_taken_after_the_selection_is_kept() {
    if !in_own_process(module_path!(), "a_lease_taken_after_the_selection_is_kept") {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let config = lease_test_config(directory.path(), true);
    let (workspace, view) = expired_view(directory.path(), &config, "project");

    let mut lease = None;
    let outcome = collect_with(
        &config.target.root,
        None,
        Some(Duration::from_secs(10)),
        &Precedence::default(),
        false,
        now_secs(),
        || {
            // The command arrives between the selection and the removal.
            lease = Some(ViewLease::acquire(&config.target.root, &workspace).unwrap());
        },
        |_| {},
        || {},
    )
    .unwrap();

    assert_eq!(outcome.kept_active_views, 1);
    assert_eq!(outcome.removed_views, 0);
    assert!(view.join("artifact").exists());

    let lease = lease.take().expect("the command took its lease");
    drop(lease);
    let outcome = collect_expired(&config.target.root);

    assert_eq!(outcome.removed_views, 1);
    assert!(!view.exists());
}

/// A command that arrives while collection holds the view exclusively must
/// wait, and by the time it gets its lease the view has been moved out of the
/// way. It must never see a directory that is being deleted under it.
#[test]
fn a_command_arriving_during_the_removal_waits_and_finds_the_view_gone() {
    if !in_own_process(
        module_path!(),
        "a_command_arriving_during_the_removal_waits_and_finds_the_view_gone",
    ) {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let config = lease_test_config(directory.path(), true);
    let (_workspace, view) = expired_view(directory.path(), &config, "project");

    let mut user: Option<JoinHandle<bool>> = None;
    let outcome = collect_with(
        &config.target.root,
        None,
        Some(Duration::from_secs(10)),
        &Precedence::default(),
        false,
        now_secs(),
        || {},
        |directory: &Path| {
            let (refused, was_refused) = mpsc::channel();
            let directory = directory.to_path_buf();
            user = Some(std::thread::spawn(move || {
                let file = open_view_lock(&view_lock_path(&directory)).unwrap();
                refused
                    .send(matches!(
                        file.try_lock_shared(),
                        Err(std::fs::TryLockError::WouldBlock)
                    ))
                    .unwrap();
                // Blocks until collection releases its exclusive hold.
                file.lock_shared().unwrap();
                directory.exists()
            }));
            // Let collection go on only once the command has been turned
            // away, so the removal really does overlap its arrival.
            assert!(
                was_refused.recv().unwrap(),
                "collection holds the view exclusively"
            );
        },
        || {},
    )
    .unwrap();

    let user = user.take().expect("collection reserved the selected view");
    assert!(
        !user.join().unwrap(),
        "the view was moved aside before collection released it"
    );
    assert_eq!(outcome.removed_views, 1);
    assert!(!view.exists());
}

/// Two commands can use one view at once, such as `cargo test` and
/// `cargo run` in the same checkout. The view stays until both have finished.
#[test]
fn stacked_leases_keep_a_view_until_the_last_is_released() {
    if !in_own_process(
        module_path!(),
        "stacked_leases_keep_a_view_until_the_last_is_released",
    ) {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let config = lease_test_config(directory.path(), true);
    let (workspace, view) = expired_view(directory.path(), &config, "project");

    let first = ViewLease::acquire(&config.target.root, &workspace).unwrap();
    let second = ViewLease::acquire(&config.target.root, &workspace).unwrap();

    let outcome = collect_expired(&config.target.root);
    assert_eq!(outcome.kept_active_views, 1);
    assert!(view.exists());

    drop(first);
    let outcome = collect_expired(&config.target.root);
    assert_eq!(
        outcome.kept_active_views, 1,
        "the second command still runs"
    );
    assert!(view.exists());

    drop(second);
    let outcome = collect_expired(&config.target.root);
    assert_eq!(outcome.removed_views, 1);
    assert!(!view.exists());
}

/// A command that is killed never gets to release its lease itself. The
/// operating system releases it with the process, so the view does not stay
/// pinned forever, while any other command still using it stays protected.
#[test]
fn a_lease_dies_with_its_process() {
    if let Some(path) = std::env::var_os(HOLD_VIEW_LOCK_ENV) {
        // The child: hold the lease until the parent kills this process.
        let file = open_view_lock(Path::new(&path)).unwrap();
        file.lock_shared().unwrap();
        // Written through the handle rather than `println!`, which the test
        // harness captures. The leading newline keeps the marker on a line of
        // its own whatever the harness has already printed.
        let mut out = std::io::stdout();
        out.write_all(b"\nheld\n").unwrap();
        out.flush().unwrap();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).unwrap();
        drop(file);
        return;
    }
    // After the check above: the holder inherits this process's environment.
    if !in_own_process(module_path!(), "a_lease_dies_with_its_process") {
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    let config = lease_test_config(directory.path(), true);
    let (workspace, view) = expired_view(directory.path(), &config, "project");

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            test_name(module_path!(), "a_lease_dies_with_its_process").as_str(),
            "--exact",
            "--test-threads=1",
        ])
        .env(HOLD_VIEW_LOCK_ENV, view_lock_path(&view))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Wait for the child to report that it holds the lease. The harness
    // prints its own lines first.
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        if output.read_line(&mut line).unwrap() == 0 {
            let status = child.wait().unwrap();
            panic!(
                "the child exited ({status}) without taking the lease; \
                 check that the test name it was given still matches"
            );
        }
        if line.trim_end() == "held" {
            break;
        }
    }

    let outcome = collect_expired(&config.target.root);
    assert_eq!(
        outcome.kept_active_views, 1,
        "the child's lease protects the view"
    );
    assert!(view.exists());

    let mine = ViewLease::acquire(&config.target.root, &workspace).unwrap();
    child.kill().unwrap();
    child.wait().unwrap();

    let outcome = collect_expired(&config.target.root);
    assert_eq!(
        outcome.kept_active_views, 1,
        "the command that is still running is still protected"
    );
    assert!(view.exists());

    drop(mine);
    let outcome = collect_expired(&config.target.root);
    assert_eq!(
        outcome.removed_views, 1,
        "the killed child's lease went with it"
    );
    assert!(!view.exists());
}

/// A command that fails by panicking unwinds through its lease, and the view
/// becomes collectable again.
#[test]
fn a_panicking_thread_releases_its_lease() {
    if !in_own_process(module_path!(), "a_panicking_thread_releases_its_lease") {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let config = lease_test_config(directory.path(), true);
    let (workspace, view) = expired_view(directory.path(), &config, "project");

    let (held, was_held) = mpsc::channel();
    let root = config.target.root.clone();
    let checkout = workspace.clone();
    let command = std::thread::spawn(move || {
        let _lease = ViewLease::acquire(&root, &checkout).unwrap();
        held.send(()).unwrap();
        panic!("the command failed while holding its lease");
    });
    was_held.recv().unwrap();
    assert!(command.join().is_err());

    let outcome = collect_expired(&config.target.root);

    assert_eq!(outcome.removed_live_views, 1);
    assert!(!view.exists());
}

/// An explicit clean of one checkout must not delete outputs a command in that
/// checkout is still running. Once the command finishes, the clean works.
#[test]
fn remove_workspace_keeps_a_view_in_use() {
    if !in_own_process(module_path!(), "remove_workspace_keeps_a_view_in_use") {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let config = lease_test_config(directory.path(), true);
    let workspace = lease_checkout(directory.path(), "project");
    let view = place(&config, &workspace, &workspace.join("target"), false).unwrap();
    std::fs::write(view.join("artifact"), vec![0_u8; 4_096]).unwrap();
    let record = view_record_path(&config.target.root, &workspace);
    let link = workspace.join("target");

    let lease = ViewLease::acquire(&config.target.root, &workspace).unwrap();

    assert_eq!(
        remove_workspace(&config.target.root, &workspace).unwrap(),
        RemoveOutcome::Active
    );
    assert!(view.join("artifact").exists());
    assert!(record.exists());
    assert_eq!(std::fs::read_link(&link).unwrap(), view);

    drop(lease);

    assert!(matches!(
        remove_workspace(&config.target.root, &workspace).unwrap(),
        RemoveOutcome::Removed(bytes) if bytes > 0
    ));
    assert!(!view.exists());
    assert!(!record.exists());
    assert!(std::fs::symlink_metadata(&link).is_err());
    assert_eq!(
        remove_workspace(&config.target.root, &workspace).unwrap(),
        RemoveOutcome::Missing
    );
}

/// A lease protects only its own checkout's view. Collection still frees the
/// space held by views nobody is using.
#[test]
fn unrelated_views_are_collected_while_another_is_in_use() {
    if !in_own_process(
        module_path!(),
        "unrelated_views_are_collected_while_another_is_in_use",
    ) {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let config = lease_test_config(directory.path(), true);
    let (busy_workspace, busy) = expired_view(directory.path(), &config, "busy");
    let (_idle_workspace, idle) = expired_view(directory.path(), &config, "idle");

    let _lease = ViewLease::acquire(&config.target.root, &busy_workspace).unwrap();
    let outcome = collect_expired(&config.target.root);

    assert_eq!(outcome.kept_active_views, 1);
    assert_eq!(outcome.removed_views, 1);
    assert!(busy.join("artifact").exists());
    assert!(!idle.exists());
}
