use super::*;
use std::fs::{self, FileTimes};
use std::process::Command;
use std::time::SystemTime;

fn run_git(folder: &Path, args: &[&str]) {
    let result = Command::new("/usr/bin/git")
        .args(args)
        .current_dir(folder)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn init(folder: &Path) {
    run_git(folder, &["init", "--initial-branch=main"]);
    run_git(folder, &["config", "user.name", "PawCommander Test"]);
    run_git(folder, &["config", "user.email", "test@example.invalid"]);
}

fn read(folder: &Path) -> Option<Snapshot> {
    Snapshot::read(folder, &AtomicBool::new(false))
}

fn kind(snapshot: &Snapshot, name: &str) -> Option<FileStatus> {
    snapshot.entry(&snapshot.folder.join(name).into())
}

#[test]
fn reads_staged_worktree_and_ignore_status_without_touching_git_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let folder = temp.path();
    init(folder);
    fs::create_dir(folder.join("src")).unwrap();
    fs::write(folder.join("src/пример\tфайла.txt"), "first").unwrap();
    fs::write(folder.join("stable.txt"), "stable").unwrap();
    fs::write(folder.join(".gitignore"), "cache/\n*.ignored\n").unwrap();
    run_git(folder, &["add", "--all"]);
    run_git(folder, &["commit", "-m", "initial"]);

    fs::write(folder.join("src/пример\tфайла.txt"), "second").unwrap();
    fs::write(folder.join("src/scratch\nnotes.txt"), "work").unwrap();
    fs::write(folder.join("src/log.ignored"), "log").unwrap();
    fs::write(folder.join("staged.txt"), "added").unwrap();
    run_git(folder, &["add", "staged.txt"]);
    fs::create_dir(folder.join("cache")).unwrap();
    fs::write(folder.join("cache/cached.txt"), "cached").unwrap();
    std::fs::File::open(folder.join("stable.txt"))
        .unwrap()
        .set_times(FileTimes::new().set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1)))
        .unwrap();

    let git = folder.join(".git");
    let git_files = ["index", "HEAD", "refs/heads/main", "config"];
    let unchanged = git_files.map(|name| {
        let path = git.join(name);
        (
            fs::read(&path).unwrap(),
            path.metadata().unwrap().modified().unwrap(),
        )
    });
    let root = read(folder).unwrap();
    assert_eq!(root.head, Head::Branch("main".into()));
    assert_eq!(kind(&root, "cache"), Some(FileStatus::Ignored));
    assert_eq!(kind(&root, "src"), Some(FileStatus::Modified));
    assert_eq!(kind(&root, "staged.txt"), Some(FileStatus::Modified));
    assert_eq!(kind(&root, "stable.txt"), None);
    assert_eq!(kind(&root, ".git"), None);

    let nested = read(&folder.join("src")).unwrap();
    assert_eq!(
        kind(&nested, "пример\tфайла.txt"),
        Some(FileStatus::Modified)
    );
    assert_eq!(
        kind(&nested, "scratch\nnotes.txt"),
        Some(FileStatus::Untracked)
    );
    assert_eq!(kind(&nested, "log.ignored"), Some(FileStatus::Ignored));
    assert_eq!(kind(&nested, "../staged.txt"), None);

    for (name, (contents, mtime)) in git_files.into_iter().zip(unchanged) {
        let path = git.join(name);
        assert_eq!(fs::read(&path).unwrap(), contents, "{name} contents");
        assert_eq!(
            path.metadata().unwrap().modified().unwrap(),
            mtime,
            "{name} mtime"
        );
    }
    assert!(!git.join("index.lock").exists());
}

#[test]
fn reads_inside_untracked_and_ignored_folders_and_handles_literal_names() {
    let temp = tempfile::tempdir().unwrap();
    let folder = temp.path();
    init(folder);
    fs::write(folder.join(".gitignore"), "cache/\n*.ignored\n").unwrap();
    fs::create_dir_all(folder.join("plain/inner")).unwrap();
    fs::create_dir_all(folder.join("literal [a]/inner")).unwrap();
    fs::create_dir_all(folder.join("cache/inner")).unwrap();
    fs::write(folder.join("cache/force-tracked.txt"), "tracked").unwrap();
    fs::write(folder.join("cache/cached.txt"), "cached").unwrap();
    run_git(folder, &["add", "-f", "cache/force-tracked.txt"]);
    run_git(folder, &["add", ".gitignore"]);
    run_git(folder, &["commit", "-m", "initial"]);
    for child in ["plain/inner", "literal [a]/inner", "cache/inner"] {
        fs::write(folder.join(child).join("temp.txt"), "contents").unwrap();
        fs::write(folder.join(child).join("temp.ignored"), "ignored").unwrap();
        let snapshot = read(&folder.join(child)).unwrap();
        assert_eq!(
            kind(&snapshot, "temp.txt"),
            Some(if child.starts_with("cache/") {
                FileStatus::Ignored
            } else {
                FileStatus::Untracked
            }),
            "{child}"
        );
        assert_eq!(
            kind(&snapshot, "temp.ignored"),
            Some(FileStatus::Ignored),
            "{child}"
        );
    }
    assert_eq!(
        kind(&read(&folder.join("cache")).unwrap(), "force-tracked.txt"),
        None
    );
    assert_eq!(
        kind(&read(&folder.join("cache")).unwrap(), "cached.txt"),
        Some(FileStatus::Ignored)
    );
    assert_eq!(
        kind(&read(folder).unwrap(), "cache"),
        Some(FileStatus::Ignored)
    );
}

#[test]
fn shows_unborn_detached_and_linked_worktree_heads() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let linked = temp.path().join("linked");
    fs::create_dir(&repo).unwrap();
    assert!(read(&repo).is_none());
    init(&repo);
    let unborn = read(&repo).unwrap();
    assert_eq!(unborn.head, Head::Branch("main".into()));
    run_git(&repo, &["commit", "--allow-empty", "-m", "initial"]);
    run_git(
        &repo,
        &["worktree", "add", "-b", "other", linked.to_str().unwrap()],
    );
    assert_eq!(read(&linked).unwrap().head, Head::Branch("other".into()));
    run_git(&repo, &["switch", "--detach", "HEAD"]);
    let detached = read(&repo).unwrap();
    let Head::Detached(oid) = detached.head else {
        panic!("expected detached HEAD");
    };
    assert_eq!(oid.len(), 7);
    assert!(oid.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert!(read(&repo.join(".git")).is_none());
}

fn finish_scan(worker: &mut Background, ctx: &egui::Context, tab: &mut Tab, now: Instant) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while worker.scan.is_some() && Instant::now() < deadline {
        worker.update(ctx, Some(tab), now);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(worker.scan.is_none(), "Git status scan did not complete");
}

#[test]
fn navigation_discards_a_completed_scan_and_periodic_refresh_updates_the_branch() {
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("first");
    fs::create_dir(&first).unwrap();
    init(&first);
    let second = temp.path().join("second");
    fs::create_dir(&second).unwrap();
    init(&second);
    run_git(&second, &["switch", "-c", "second"]);
    let ctx = egui::Context::default();
    let now = Instant::now();
    let mut tab = Tab::new(first.clone().into(), &ctx);

    let (tx, result) = mpsc::channel();
    tx.send(read(&first)).unwrap();
    let mut worker = Background {
        scope: Some((tab.id, first)),
        next_refresh: None,
        scan: Some(Scan {
            cancelled: Arc::new(AtomicBool::new(false)),
            result,
        }),
    };
    tab.navigate(second.clone().into(), &ctx);
    worker.update(&ctx, Some(&mut tab), now);
    assert!(
        tab.git_snapshot.is_none(),
        "the old result must be discarded"
    );
    finish_scan(&mut worker, &ctx, &mut tab, now);
    assert_eq!(
        tab.git_snapshot().unwrap().head,
        Head::Branch("second".into())
    );

    run_git(&second, &["switch", "-c", "refreshed"]);
    fs::write(second.join("new.txt"), "new").unwrap();
    let now = now + REFRESH_INTERVAL;
    worker.update(&ctx, Some(&mut tab), now);
    finish_scan(&mut worker, &ctx, &mut tab, now);
    let snapshot = tab.git_snapshot().unwrap();
    assert_eq!(snapshot.head, Head::Branch("refreshed".into()));
    assert_eq!(kind(snapshot, "new.txt"), Some(FileStatus::Untracked));
}

#[test]
fn file_pane_draws_the_branch_and_all_three_badges_in_each_view() {
    use super::super::state::{FolderMode, FolderViewChange};
    use super::super::PawCommander;
    use crate::test_ui::ContextExt as _;

    let temp = tempfile::tempdir().unwrap();
    let folder = temp.path();
    init(folder);
    fs::write(folder.join(".gitignore"), "ignored.log\n").unwrap();
    fs::write(folder.join("tracked.txt"), "old").unwrap();
    run_git(folder, &["add", "--all"]);
    run_git(folder, &["commit", "-m", "initial"]);
    fs::write(folder.join("tracked.txt"), "modified").unwrap();
    fs::write(folder.join("untracked.txt"), "new").unwrap();
    fs::write(folder.join("ignored.log"), "ignored").unwrap();

    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(folder.to_owned()));
    app.settings.split = false;
    app.settings.preview = false;
    app.settings.pane_layout.sidebar_visible = false;
    let mut frame = eframe::Frame::_new_kittest();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        eframe::App::logic(&mut app, &ctx, &mut frame);
        if app.tab().has_complete_listing && app.tab().git_snapshot().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(app.tab().has_complete_listing && app.tab().git_snapshot().is_some());

    for (mode, height) in [
        (FolderMode::List, 600.0),
        (FolderMode::Grid, 600.0),
        (FolderMode::Grid, 268.0),
        (FolderMode::Columns, 600.0),
    ] {
        app.change_folder_view(0, FolderViewChange::Mode(mode));
        let output = ctx.run_test_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(960.0, height),
                )),
                ..Default::default()
            },
            |ui| eframe::App::ui(&mut app, ui, &mut frame),
        );
        for expected in ["Git · main", "M", "?", "!"] {
            assert!(
                output.shapes.iter().any(|clipped| {
                    matches!(&clipped.shape, egui::Shape::Text(text)
                        if text.galley.text() == expected && clipped.clip_rect.contains(text.pos))
                }),
                "{mode:?} at {height} points should show {expected}"
            );
        }
    }
}
