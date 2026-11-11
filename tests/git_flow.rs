//! Сценарии git на VPS на временных репо: bare remote + клон «ноута» + клон «VPS».

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use claude_sync::git::{GitCtx, check_ssh_url};
use tempfile::TempDir;
use uuid::Uuid;

fn sh(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn identity(dir: &Path, name: &str) {
    sh(dir, &["config", "user.name", name]);
    sh(dir, &["config", "user.email", &format!("{name}@test")]);
    sh(dir, &["config", "commit.gpgsign", "false"]);
}

struct Env {
    _tmp: TempDir,
    remote: PathBuf,
    laptop: PathBuf,
    vps: PathBuf,
    ctx: GitCtx,
    id: Uuid,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let remote = root.join("remote.git");
        let laptop = root.join("laptop");
        let vps = root.join("vps");
        std::fs::create_dir_all(&remote).unwrap();
        sh(&remote, &["init", "--bare", "-b", "main"]);
        sh(root, &["clone", remote.to_str().unwrap(), "laptop"]);
        identity(&laptop, "laptop");
        std::fs::write(laptop.join("a.txt"), "1\n").unwrap();
        std::fs::write(laptop.join(".gitignore"), "target/\n").unwrap();
        sh(&laptop, &["add", "-A"]);
        sh(&laptop, &["commit", "-m", "init"]);
        sh(&laptop, &["push", "-u", "origin", "main"]);
        sh(root, &["clone", remote.to_str().unwrap(), "vps"]);
        identity(&vps, "vps");

        Self {
            ctx: GitCtx {
                remote: "origin".into(),
                wip_prefix: "vps-wip/".into(),
                node: "vps1".into(),
                state_dir: root.join("state"),
            },
            id: "09df6a91-6bb3-49d7-a350-17a48b802364".parse().unwrap(),
            _tmp: tmp,
            remote,
            laptop,
            vps,
        }
    }

    fn wip(&self) -> String {
        self.ctx.wip_branch(&self.id)
    }

    fn remote_has(&self, branch: &str) -> bool {
        !sh(&self.remote, &["branch", "--list", branch]).is_empty()
    }

    fn assert_vps_clean_on_main(&self) {
        assert_eq!(sh(&self.vps, &["status", "--porcelain"]), "");
        assert_eq!(sh(&self.vps, &["symbolic-ref", "--short", "HEAD"]), "main");
        assert_eq!(
            sh(&self.vps, &["rev-parse", "HEAD"]),
            sh(&self.vps, &["rev-parse", "origin/main"])
        );
        assert_eq!(sh(&self.vps, &["branch", "--list", &self.wip()]), "");
        assert!(self.ctx.read_state(&self.id).is_none());
    }
}

#[tokio::test]
async fn no_changes_nothing_pushed() {
    let e = Env::new();
    e.ctx.prepare(&e.id, &e.vps).await.unwrap();
    let st = e.ctx.read_state(&e.id).unwrap();
    assert_eq!(st.base_branch, "main");
    assert_eq!(st.node, "vps1");

    let msg = e.ctx.finalize(&e.id, &e.vps, "t").await.unwrap();
    assert_eq!(msg, "изменений нет");
    assert!(!e.remote_has(&e.wip()));
    e.assert_vps_clean_on_main();
}

#[tokio::test]
async fn pulls_laptop_changes_on_prepare() {
    let e = Env::new();
    std::fs::write(e.laptop.join("b.txt"), "from laptop\n").unwrap();
    sh(&e.laptop, &["add", "-A"]);
    sh(&e.laptop, &["commit", "-m", "laptop wip"]);
    sh(&e.laptop, &["push"]);

    e.ctx.prepare(&e.id, &e.vps).await.unwrap();
    assert!(e.vps.join("b.txt").exists());
}

#[tokio::test]
async fn changes_go_to_wip_and_clone_is_reset() {
    let e = Env::new();
    e.ctx.prepare(&e.id, &e.vps).await.unwrap();

    // claude закоммитил в main и оставил незакоммиченное + мусор в target/
    std::fs::write(e.vps.join("c.txt"), "commit by claude\n").unwrap();
    sh(&e.vps, &["add", "-A"]);
    sh(&e.vps, &["commit", "-m", "claude commit"]);
    std::fs::write(e.vps.join("a.txt"), "edited\n").unwrap();
    std::fs::write(e.vps.join("new.txt"), "untracked\n").unwrap();
    std::fs::create_dir_all(e.vps.join("target")).unwrap();
    std::fs::write(e.vps.join("target/x"), "build").unwrap();

    let msg = e
        .ctx
        .finalize(&e.id, &e.vps, "Разбор дейликов")
        .await
        .unwrap();
    assert!(msg.contains(&e.wip()), "{msg}");
    assert!(msg.contains("2"), "{msg}");
    assert!(e.remote_has(&e.wip()));
    e.assert_vps_clean_on_main();
    assert!(!e.vps.join("c.txt").exists());
    assert_eq!(std::fs::read_to_string(e.vps.join("a.txt")).unwrap(), "1\n");

    // на remote: main не тронут, в wip всё лежит
    let main_log = sh(&e.remote, &["log", "--format=%s", "main"]);
    assert_eq!(main_log, "init");
    let wip_files = sh(&e.remote, &["ls-tree", "--name-only", &e.wip()]);
    for f in ["a.txt", "c.txt", "new.txt"] {
        assert!(wip_files.contains(f), "{wip_files}");
    }
    assert!(!wip_files.contains("target"));
    let subject = sh(&e.remote, &["log", "-1", "--format=%s%n%b", &e.wip()]);
    assert!(
        subject.contains("vps-wip(vps1): Разбор дейликов"),
        "{subject}"
    );
    assert!(subject.contains(&e.id.to_string()), "{subject}");
}

#[tokio::test]
async fn second_resume_continues_wip_fast_forward() {
    let e = Env::new();
    e.ctx.prepare(&e.id, &e.vps).await.unwrap();
    std::fs::write(e.vps.join("c.txt"), "1\n").unwrap();
    e.ctx.finalize(&e.id, &e.vps, "t").await.unwrap();
    let first = sh(&e.remote, &["rev-parse", &e.wip()]);

    let msg = e.ctx.prepare(&e.id, &e.vps).await.unwrap();
    assert!(msg.contains("невлитой"), "{msg}");
    assert!(e.vps.join("c.txt").exists());
    assert_eq!(e.ctx.read_state(&e.id).unwrap().base_branch, "main");

    std::fs::write(e.vps.join("d.txt"), "2\n").unwrap();
    e.ctx.finalize(&e.id, &e.vps, "t").await.unwrap();
    let second = sh(&e.remote, &["rev-parse", &e.wip()]);
    assert_ne!(first, second);
    // fast-forward: первый коммит — предок второго
    sh(&e.remote, &["merge-base", "--is-ancestor", &first, &second]);
    e.assert_vps_clean_on_main();
}

#[tokio::test]
async fn rejected_push_keeps_everything() {
    let e = Env::new();
    e.ctx.prepare(&e.id, &e.vps).await.unwrap();
    std::fs::write(e.vps.join("c.txt"), "vps\n").unwrap();

    // кто-то другой успел запушить в ту же wip-ветку
    sh(&e.laptop, &["switch", "-c", &e.wip()]);
    std::fs::write(e.laptop.join("z.txt"), "other\n").unwrap();
    sh(&e.laptop, &["add", "-A"]);
    sh(&e.laptop, &["commit", "-m", "other"]);
    sh(&e.laptop, &["push", "origin", &e.wip()]);

    let err = e.ctx.finalize(&e.id, &e.vps, "t").await.unwrap_err();
    assert!(format!("{err:#}").contains("push"), "{err:#}");
    // ничего не сброшено: коммит с правкой на месте, запись о запуске жива
    assert_eq!(
        sh(&e.vps, &["log", "-1", "--format=%s"]),
        "vps-wip(vps1): t"
    );
    assert!(e.vps.join("c.txt").exists());
    assert!(e.ctx.read_state(&e.id).is_some());
}

#[tokio::test]
async fn junk_blocks_finalize_and_keeps_clone() {
    let e = Env::new();
    e.ctx.prepare(&e.id, &e.vps).await.unwrap();
    std::fs::write(e.vps.join("a.txt"), "changed\n").unwrap();
    std::fs::create_dir_all(e.vps.join(".venv/lib")).unwrap();
    std::fs::write(e.vps.join(".venv/pyvenv.cfg"), "home = /usr\n").unwrap();
    std::fs::write(e.vps.join(".venv/lib/x.py"), "").unwrap();
    std::fs::create_dir_all(e.vps.join("web/node_modules/a")).unwrap();
    std::fs::write(e.vps.join("web/node_modules/a/i.js"), "").unwrap();
    let big = std::fs::File::create(e.vps.join("dump.bin")).unwrap();
    big.set_len(60 * 1024 * 1024).unwrap();

    let err = format!(
        "{:#}",
        e.ctx.finalize(&e.id, &e.vps, "t").await.unwrap_err()
    );
    assert!(err.contains(".venv/ (python venv)"), "{err}");
    assert!(!err.contains("x.py"), "{err}");
    assert!(err.contains("web/node_modules/ (node_modules)"), "{err}");
    assert!(err.contains("dump.bin (60 МБ)"), "{err}");
    // ничего не закоммичено и не запушено, запуск не завершён
    assert_eq!(
        sh(&e.vps, &["rev-parse", "HEAD"]),
        sh(&e.vps, &["rev-parse", "origin/main"])
    );
    assert!(!e.remote_has(&e.wip()));
    assert!(e.ctx.read_state(&e.id).is_some());

    // после .gitignore Stop проходит
    std::fs::write(
        e.vps.join(".git/info/exclude"),
        ".venv/\nnode_modules/\ndump.bin\n",
    )
    .unwrap();
    e.ctx.finalize(&e.id, &e.vps, "t").await.unwrap();
    assert!(e.remote_has(&e.wip()));
}

#[tokio::test]
async fn dirty_clone_blocks_prepare() {
    let e = Env::new();
    std::fs::write(e.vps.join("a.txt"), "dirty\n").unwrap();
    let err = e.ctx.prepare(&e.id, &e.vps).await.unwrap_err();
    assert!(format!("{err:#}").contains("незакоммиченные"), "{err:#}");
    assert!(e.ctx.read_state(&e.id).is_none());
}

#[tokio::test]
async fn diverged_base_blocks_prepare() {
    let e = Env::new();
    std::fs::write(e.vps.join("v.txt"), "local\n").unwrap();
    sh(&e.vps, &["add", "-A"]);
    sh(&e.vps, &["commit", "-m", "local only"]);
    std::fs::write(e.laptop.join("l.txt"), "remote\n").unwrap();
    sh(&e.laptop, &["add", "-A"]);
    sh(&e.laptop, &["commit", "-m", "remote"]);
    sh(&e.laptop, &["push"]);

    let err = e.ctx.prepare(&e.id, &e.vps).await.unwrap_err();
    assert!(format!("{err:#}").contains("разошлась"), "{err:#}");
    assert!(e.ctx.read_state(&e.id).is_none());
}

#[tokio::test]
async fn unfinished_run_blocks_prepare() {
    let e = Env::new();
    e.ctx.prepare(&e.id, &e.vps).await.unwrap();
    let err = e.ctx.prepare(&e.id, &e.vps).await.unwrap_err();
    assert!(format!("{err:#}").contains("Stop"), "{err:#}");
}

#[tokio::test]
async fn base_moved_during_session_resets_to_new_base() {
    let e = Env::new();
    e.ctx.prepare(&e.id, &e.vps).await.unwrap();
    std::fs::write(e.laptop.join("l.txt"), "pushed meanwhile\n").unwrap();
    sh(&e.laptop, &["add", "-A"]);
    sh(&e.laptop, &["commit", "-m", "meanwhile"]);
    sh(&e.laptop, &["push"]);

    let msg = e.ctx.finalize(&e.id, &e.vps, "t").await.unwrap();
    assert_eq!(msg, "изменений нет");
    e.assert_vps_clean_on_main();
    assert!(e.vps.join("l.txt").exists());
}

#[tokio::test]
async fn feature_branch_is_base() {
    let e = Env::new();
    sh(&e.laptop, &["switch", "-c", "feat"]);
    sh(&e.laptop, &["push", "-u", "origin", "feat"]);
    sh(&e.vps, &["fetch"]);
    sh(&e.vps, &["switch", "feat"]);

    e.ctx.prepare(&e.id, &e.vps).await.unwrap();
    assert_eq!(e.ctx.read_state(&e.id).unwrap().base_branch, "feat");
    std::fs::write(e.vps.join("f.txt"), "x\n").unwrap();
    e.ctx.finalize(&e.id, &e.vps, "t").await.unwrap();
    assert_eq!(sh(&e.vps, &["symbolic-ref", "--short", "HEAD"]), "feat");
    assert_eq!(sh(&e.vps, &["status", "--porcelain"]), "");
    assert!(e.remote_has(&e.wip()));
}

#[tokio::test]
async fn clone_into_new_dir_then_full_cycle() {
    let e = Env::new();
    let url = e.remote.to_str().unwrap();
    let dest = e.remote.parent().unwrap().join("projects/new/repo");

    let msg = e.ctx.clone_into(url, &dest).await.unwrap();
    assert!(msg.contains("склонировано"), "{msg}");
    assert!(dest.join("a.txt").exists());
    // повторно — тот же репо, ничего не делаем
    let msg = e.ctx.clone_into(url, &dest).await.unwrap();
    assert_eq!(msg, "репо уже склонирован");

    identity(&dest, "vps");
    e.ctx.prepare(&e.id, &dest).await.unwrap();
    std::fs::write(dest.join("n.txt"), "new session\n").unwrap();
    let msg = e.ctx.finalize(&e.id, &dest, "Новая сессия").await.unwrap();
    assert!(msg.contains(&e.wip()), "{msg}");
    assert!(e.remote_has(&e.wip()));
}

#[tokio::test]
async fn clone_into_refuses_occupied_dirs() {
    let e = Env::new();
    let url = e.remote.to_str().unwrap();
    let root = e.remote.parent().unwrap();

    let busy = root.join("busy");
    std::fs::create_dir_all(&busy).unwrap();
    std::fs::write(busy.join("x"), "x").unwrap();
    let err = e.ctx.clone_into(url, &busy).await.unwrap_err().to_string();
    assert!(err.contains("не пустая"), "{err}");

    // в папке другой репо
    let other = format!("{url}-other");
    let err = e
        .ctx
        .clone_into(&other, &e.vps)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("другой репо"), "{err}");

    // пустая папка подходит; при ошибке clone она не остаётся мусором
    let empty = root.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    assert!(
        e.ctx
            .clone_into("/nonexistent/repo.git", &empty)
            .await
            .is_err()
    );
    let missing = root.join("missing/repo");
    assert!(
        e.ctx
            .clone_into("/nonexistent/repo.git", &missing)
            .await
            .is_err()
    );
    assert!(!missing.exists());
}

#[test]
fn ssh_urls() {
    for ok in [
        "git@github.com:aragami3070/claude-sync.git",
        "git@git.csit.sgu.ru:group/sub/repo",
        "ssh://git@git.csit.sgu.ru:2222/group/repo.git",
    ] {
        assert!(check_ssh_url(ok).is_ok(), "{ok}");
    }
    for bad in [
        "",
        "https://github.com/a/b.git",
        "github.com:a/b",
        "git@github.com:",
        "-oProxyCommand=x@h:p",
        "git@github.com:a/b c",
        "/home/u/repo",
        "ssh://host",
        "file:///tmp/x",
    ] {
        assert!(check_ssh_url(bad).is_err(), "{bad}");
    }
}

#[tokio::test]
async fn workspace_repo_has_own_state() {
    let e = Env::new();
    let ws = e.vps.parent().unwrap().to_path_buf();
    e.ctx.prepare_repo(&e.id, &e.vps, &ws).await.unwrap();
    // обычного state нет, но запуск числится незавершённым
    assert!(e.ctx.read_state(&e.id).is_none());
    let states = e.ctx.states_of(&e.id);
    assert_eq!(states.len(), 1);
    assert_eq!(states[0].cwd, e.vps);
    assert_eq!(states[0].workspace.as_deref(), Some(ws.as_path()));
    assert!(e.ctx.prepare_repo(&e.id, &e.vps, &ws).await.is_err());

    std::fs::write(e.vps.join("a.txt"), "ws change\n").unwrap();
    let msg = e.ctx.finalize_repo(&e.id, &e.vps, "t").await.unwrap();
    assert!(msg.contains("увезены"), "{msg}");
    assert!(e.remote_has(&e.wip()));
    assert!(!e.ctx.has_state(&e.id));
    e.assert_vps_clean_on_main();
}
