use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path as FsPath, PathBuf},
    sync::Arc,
};

use askama::Template;
use axum::{
    Router,
    extract::{Form, Path, Query, State},
    http::{StatusCode, header},
    middleware,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    app::{App, Fresh, NewReq, Op},
    auth,
    conflicts::{self, Diff, Keep, Msg},
    git::check_ssh_url,
    sessions::Session,
    tmux::Pane,
    util::{ago, find_git_root, resolve_user_path, tilde, truncate_chars},
    workspace,
};

pub fn router(app: Arc<App>) -> Router {
    let protected = Router::new()
        .route("/", get(index))
        .route("/sessions", get(list))
        .route("/new", post(new_session))
        .route("/sessions/{id}/row", get(row))
        .route("/sessions/{id}/screen", get(screen))
        .route("/sessions/{id}/conflicts", get(conflict_diff))
        .route("/sessions/{id}/conflicts/resolve", post(conflict_resolve))
        .route("/conflicts", get(conflicts_page))
        .route("/sessions/{id}/start", post(start))
        .route("/sessions/{id}/stop", post(stop))
        .route("/sessions/{id}/dismiss", post(dismiss))
        .route("/static/htmx.min.js", get(htmx_js))
        .route("/static/style.css", get(style_css))
        .layer(middleware::from_fn_with_state(app.clone(), auth::layer));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(protected)
        .with_state(app)
}

pub struct RowView {
    pub id: Uuid,
    pub title: String,
    pub cwd: String,
    pub branch: Option<String>,
    pub ago: String,
    pub status: &'static str,
    pub status_label: &'static str,
    pub poll: bool,
    pub can_start: bool,
    pub can_stop: bool,
    pub stop_label: &'static str,
    pub has_pane: bool,
    pub rc_url: Option<String>,
    pub conflict: bool,
    pub no_repo: bool,
    pub continued_in: Option<String>,
    pub msg: Option<String>,
    pub msg_error: bool,
    /// Перед Stop нужно второе подтверждение: текст предупреждения.
    pub stop_warning: Option<String>,
}

impl RowView {
    /// `s` — транскрипт с сообщениями; `None` у новой сессии до первого промпта.
    fn build(
        app: &App,
        id: Uuid,
        s: Option<&Session>,
        fresh: Option<&Fresh>,
        pane: Option<&Pane>,
        op: Option<&Op>,
    ) -> Self {
        let state = app.git.states_of(&id).into_iter().next();
        let has_state = state.is_some();
        let repo_ok = s
            .is_none_or(|s| s.git_root().is_some() || s.cwd().is_some_and(workspace::is_workspace));
        let alive = pane.is_some_and(|p| !p.dead);

        let (status, status_label, poll) = match (op, pane) {
            (Some(Op::Starting), _) => ("busy", "запускается…", true),
            (Some(Op::Stopping), _) => ("busy", "останавливается…", true),
            (_, Some(p)) if !p.dead => ("running", "работает", false),
            (_, Some(_)) => ("dead", "claude завершился", false),
            _ if has_state => ("pending", "не вывезено", false),
            _ => ("stopped", "остановлена", false),
        };
        let (msg, msg_error) = match op {
            Some(Op::Done(m)) => (Some(m.clone()), false),
            Some(Op::Failed(m)) => (Some(m.clone()), true),
            _ => (None, false),
        };
        let cwd: Option<PathBuf> = s
            .and_then(Session::cwd)
            .map(FsPath::to_path_buf)
            .or_else(|| fresh.map(|f| f.cwd.clone()))
            .or_else(|| state.map(|st| st.workspace.unwrap_or(st.cwd)));
        let rc_url = match (alive, s) {
            (false, _) => None,
            (true, Some(s)) => s.rc_url(),
            (true, None) => app.scanner.rc_url_any(id),
        };

        Self {
            id,
            title: match s {
                Some(s) => s.title(),
                None => fresh
                    .and_then(|f| f.name.clone())
                    .unwrap_or_else(|| "Новая сессия".into()),
            },
            cwd: cwd
                .map(|c| tilde(&c, &app.cfg.home))
                .unwrap_or_else(|| "?".into()),
            branch: s.and_then(|s| s.parsed.git_branch.clone()),
            ago: s.map_or_else(|| "ещё без сообщений".into(), |s| ago(s.mtime)),
            status,
            status_label,
            poll,
            can_start: s.is_some() && !poll && pane.is_none() && !has_state && repo_ok,
            can_stop: !poll && (pane.is_some() || has_state),
            stop_label: if alive {
                "Stop"
            } else {
                "Вывезти изменения"
            },
            has_pane: pane.is_some(),
            rc_url,
            conflict: s.is_some_and(|s| !s.conflicts.is_empty()),
            no_repo: !repo_ok,
            continued_in: s.and_then(|s| s.parsed.continued_in.map(|id| id.to_string())),
            msg,
            msg_error,
            stop_warning: None,
        }
    }
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTpl {
    node: String,
    dirs: Vec<String>,
    conflicts: usize,
    all: bool,
    rows: Vec<RowView>,
    hidden: usize,
    warning: Option<String>,
}

#[derive(Template)]
#[template(path = "_list.html")]
struct ListTpl {
    all: bool,
    rows: Vec<RowView>,
    hidden: usize,
    warning: Option<String>,
}

#[derive(Template)]
#[template(path = "_row.html")]
struct RowTpl {
    row: RowView,
}

#[derive(Template)]
#[template(path = "_new_result.html")]
struct NewResultTpl {
    error: Option<String>,
    row: Option<RowView>,
}

#[derive(Template)]
#[template(path = "_screen.html")]
struct ScreenTpl {
    text: String,
}

struct AppError(StatusCode, String);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

fn render(t: impl Template) -> Result<Html<String>, AppError> {
    t.render()
        .map(Html)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

#[derive(Deserialize)]
struct ListQuery {
    all: Option<String>,
}

async fn collect_rows(app: &App, all: bool) -> (Vec<RowView>, usize, Option<String>) {
    let sessions = app.sessions().await;
    let (panes, warning) = match app.tmux.list().await {
        Ok(p) => (p, None),
        Err(e) => (HashMap::new(), Some(format!("tmux недоступен: {e:#}"))),
    };
    let ops = app.ops_snapshot();
    let fresh = app.fresh_snapshot();

    // Сессии без транскрипта с сообщениями (новые), о которых что-то известно.
    let known: HashSet<Uuid> = sessions.iter().map(|s| s.id).collect();
    let states: HashSet<Uuid> = app.git.all_states().into_iter().map(|(id, _)| id).collect();
    let extra: BTreeSet<Uuid> = panes
        .keys()
        .chain(&states)
        .chain(ops.keys())
        .copied()
        .filter(|id| !known.contains(id))
        .collect();

    let mut rows: Vec<RowView> =
        extra
            .into_iter()
            .map(|id| RowView::build(app, id, None, fresh.get(&id), panes.get(&id), ops.get(&id)))
            .chain(sessions.iter().map(|s| {
                RowView::build(app, s.id, Some(s), None, panes.get(&s.id), ops.get(&s.id))
            }))
            .collect();
    // Активные (запущенные, не вывезенные, с сообщением) наверх; дальше по свежести.
    rows.sort_by_key(|r| r.status == "stopped" && r.msg.is_none());
    let total = rows.len();
    if !all {
        let keep = app
            .cfg
            .list_limit
            .max(rows.iter().filter(|r| r.status != "stopped").count());
        rows.truncate(keep);
    }
    let hidden = total - rows.len();
    (rows, hidden, warning)
}

/// Подсказки для поля «папка»: git-корни из транскриптов, свежие первыми.
async fn known_dirs(app: &App) -> Vec<String> {
    let mut seen = HashSet::new();
    app.sessions()
        .await
        .iter()
        .filter_map(|s| {
            s.git_root().or_else(|| {
                s.cwd()
                    .filter(|c| workspace::is_workspace(c))
                    .map(FsPath::to_path_buf)
            })
        })
        .filter(|r| seen.insert(r.clone()))
        .map(|r| tilde(&r, &app.cfg.home))
        .take(40)
        .collect()
}

async fn index(
    State(app): State<Arc<App>>,
    Query(q): Query<ListQuery>,
) -> Result<Html<String>, AppError> {
    let all = q.all.is_some();
    let (rows, hidden, warning) = collect_rows(&app, all).await;
    render(IndexTpl {
        node: app.cfg.node_name.clone(),
        dirs: known_dirs(&app).await,
        conflicts: app
            .sessions()
            .await
            .iter()
            .filter(|s| !s.conflicts.is_empty())
            .count(),
        all,
        rows,
        hidden,
        warning,
    })
}

async fn list(
    State(app): State<Arc<App>>,
    Query(q): Query<ListQuery>,
) -> Result<Html<String>, AppError> {
    let all = q.all.is_some();
    let (rows, hidden, warning) = collect_rows(&app, all).await;
    render(ListTpl {
        all,
        rows,
        hidden,
        warning,
    })
}

fn parse_id(id: &str) -> Result<Uuid, AppError> {
    id.parse()
        .map_err(|_| AppError(StatusCode::BAD_REQUEST, "плохой id".into()))
}

/// Строка сессии. Пустая, если о сессии ничего не известно: htmx уберёт её со страницы.
async fn row_view(app: &App, id: Uuid) -> Option<RowView> {
    let s = app.session(id).await;
    let fresh = app.fresh(&id);
    let panes = app.tmux.list().await.unwrap_or_default();
    let op = app.op(&id);
    let pane = panes.get(&id);
    if s.is_none() && pane.is_none() && op.is_none() && !app.git.has_state(&id) {
        return None;
    }
    Some(RowView::build(
        app,
        id,
        s.as_ref(),
        fresh.as_ref(),
        pane,
        op.as_ref(),
    ))
}

async fn row_html(app: &App, id: Uuid) -> Result<Html<String>, AppError> {
    match row_view(app, id).await {
        Some(row) => render(RowTpl { row }),
        None => Ok(Html(String::new())),
    }
}

async fn row(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> Result<Html<String>, AppError> {
    row_html(&app, parse_id(&id)?).await
}

async fn start(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> Result<Html<String>, AppError> {
    let id = parse_id(&id)?;
    if app.session(id).await.is_none() {
        return Err(AppError(StatusCode::NOT_FOUND, "сессия не найдена".into()));
    }
    app.spawn_start(id);
    row_html(&app, id).await
}

async fn stop(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    body: String,
) -> Result<Html<String>, AppError> {
    let id = parse_id(&id)?;
    // Тело разбираем сами: у обычной кнопки Stop его нет вовсе.
    let confirmed = body.split('&').any(|kv| kv == "confirmed=1");
    let Some(mut row) = row_view(&app, id).await else {
        return Err(AppError(StatusCode::NOT_FOUND, "сессия не найдена".into()));
    };
    if !confirmed && let Some(w) = app.stop_warning(id).await {
        row.stop_warning = Some(w);
        return render(RowTpl { row });
    }
    app.spawn_stop(id);
    row_html(&app, id).await
}

async fn dismiss(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> Result<Html<String>, AppError> {
    let id = parse_id(&id)?;
    app.clear_op(&id);
    let row = row_view(&app, id).await;
    if row.is_none() {
        app.forget_fresh(&id);
    }
    match row {
        Some(row) => render(RowTpl { row }),
        None => Ok(Html(String::new())),
    }
}

#[derive(Deserialize)]
struct NewForm {
    path: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    name: String,
}

/// Проверка формы до запуска: ошибки показываются сразу под формой.
fn parse_new(app: &App, f: &NewForm) -> anyhow::Result<NewReq> {
    let cwd = resolve_user_path(&f.path, &app.cfg.home)?;
    let url = Some(f.url.trim().to_string()).filter(|u| !u.is_empty());
    match &url {
        Some(u) => check_ssh_url(u)?,
        None => {
            if !cwd.is_dir() {
                anyhow::bail!(
                    "нет папки {}: если репо ещё нет на VPS, укажи ssh-ссылку",
                    tilde(&cwd, &app.cfg.home)
                );
            }
            if find_git_root(&cwd).is_none() && workspace::child_repos(&cwd).is_empty() {
                anyhow::bail!(
                    "в {} нет git-репозитория, и в дочерних папках тоже",
                    tilde(&cwd, &app.cfg.home)
                );
            }
        }
    }
    let name: String = f.name.chars().filter(|c| !c.is_control()).collect();
    let name = Some(truncate_chars(&name, 80)).filter(|n| !n.is_empty());
    Ok(NewReq { cwd, url, name })
}

async fn new_session(
    State(app): State<Arc<App>>,
    Form(f): Form<NewForm>,
) -> Result<Html<String>, AppError> {
    let req = match parse_new(&app, &f) {
        Ok(r) => r,
        Err(e) => {
            return render(NewResultTpl {
                error: Some(format!("{e:#}")),
                row: None,
            });
        }
    };
    let id = app.spawn_new(req);
    render(NewResultTpl {
        error: None,
        row: row_view(&app, id).await,
    })
}

/// Сколько сообщений с каждой стороны показывать: старое обычно не интересно.
const DIFF_TAIL: usize = 30;

struct DiffView {
    label: String,
    file_name: String,
    ago: String,
    common: usize,
    main_total: usize,
    copy_total: usize,
    only_main: Vec<Msg>,
    only_copy: Vec<Msg>,
    hidden_main: usize,
    hidden_copy: usize,
}

fn tail(mut v: Vec<Msg>) -> (Vec<Msg>, usize) {
    let hidden = v.len().saturating_sub(DIFF_TAIL);
    v.drain(..hidden);
    (v, hidden)
}

impl From<Diff> for DiffView {
    fn from(d: Diff) -> Self {
        let (main_total, copy_total) = (d.only_main.len(), d.only_copy.len());
        let (only_main, hidden_main) = tail(d.only_main);
        let (only_copy, hidden_copy) = tail(d.only_copy);
        Self {
            label: d.label,
            file_name: d.file_name,
            ago: ago(d.mtime),
            common: d.common,
            main_total,
            copy_total,
            only_main,
            only_copy,
            hidden_main,
            hidden_copy,
        }
    }
}

#[derive(Template)]
#[template(path = "_conflict_diff.html")]
struct ConflictDiffTpl {
    id: Uuid,
    diffs: Vec<DiffView>,
    notice: Option<String>,
    error: Option<String>,
}

struct ConflictItem {
    id: Uuid,
    title: String,
    cwd: String,
    ago: String,
    copies: usize,
}

#[derive(Template)]
#[template(path = "conflicts.html")]
struct ConflictsTpl {
    node: String,
    items: Vec<ConflictItem>,
}

async fn conflicts_page(State(app): State<Arc<App>>) -> Result<Html<String>, AppError> {
    let items = app
        .sessions()
        .await
        .into_iter()
        .filter(|s| !s.conflicts.is_empty())
        .map(|s| ConflictItem {
            id: s.id,
            title: s.title(),
            cwd: s
                .cwd()
                .map(|c| tilde(c, &app.cfg.home))
                .unwrap_or_else(|| "?".into()),
            ago: ago(s.mtime),
            copies: s.conflicts.len(),
        })
        .collect();
    render(ConflictsTpl {
        node: app.cfg.node_name.clone(),
        items,
    })
}

async fn conflict_diff(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> Result<Html<String>, AppError> {
    let id = parse_id(&id)?;
    diff_html(&app, id, None, None).await
}

async fn diff_html(
    app: &App,
    id: Uuid,
    notice: Option<String>,
    error: Option<String>,
) -> Result<Html<String>, AppError> {
    let s = app
        .session(id)
        .await
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "сессия не найдена".into()))?;
    let (main, copies) = (s.path, s.conflicts);
    let res = tokio::task::spawn_blocking(move || conflicts::diff_all(&main, &copies)).await;
    let (diffs, diff_err) = match res {
        Ok(Ok(d)) => (d.into_iter().map(DiffView::from).collect(), None),
        Ok(Err(e)) => (Vec::new(), Some(format!("{e:#}"))),
        Err(e) => (Vec::new(), Some(e.to_string())),
    };
    render(ConflictDiffTpl {
        id,
        diffs,
        notice,
        error: error.or(diff_err),
    })
}

#[derive(Deserialize)]
struct ResolveForm {
    file: String,
    keep: String,
}

/// Оставить основной файл или копию. Только при остановленной сессии:
/// запущенный claude дописывает в основной файл.
async fn conflict_resolve(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Form(f): Form<ResolveForm>,
) -> Result<Html<String>, AppError> {
    let id = parse_id(&id)?;
    let keep = match f.keep.as_str() {
        "main" => Keep::Main,
        "copy" => Keep::Copy,
        _ => {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                "keep: main или copy".into(),
            ));
        }
    };
    let s = app
        .session(id)
        .await
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "сессия не найдена".into()))?;
    let alive = app
        .tmux
        .list()
        .await
        .ok()
        .and_then(|p| p.get(&id).map(|p| !p.dead))
        .unwrap_or(false);
    let busy = app.op(&id).is_some_and(|o| o.in_progress());
    if alive || busy {
        let e = "Сессия запущена на этой VPS: сначала Stop, потом выбирай вариант.".into();
        return diff_html(&app, id, None, Some(e)).await;
    }
    let Some(copy) = s
        .conflicts
        .iter()
        .find(|c| c.file_name().is_some_and(|n| *n == *f.file))
        .cloned()
    else {
        return diff_html(&app, id, None, Some(format!("копии {} уже нет", f.file))).await;
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let trash = app
        .cfg
        .state_dir
        .join("conflict-trash")
        .join(format!("{id}-{stamp}"));
    let main = s.path;
    let res =
        tokio::task::spawn_blocking(move || conflicts::resolve(&main, &copy, keep, &trash)).await;
    match res {
        Ok(Ok(msg)) => diff_html(&app, id, Some(msg), None).await,
        Ok(Err(e)) => diff_html(&app, id, None, Some(format!("{e:#}"))).await,
        Err(e) => diff_html(&app, id, None, Some(e.to_string())).await,
    }
}

async fn screen(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> Result<Html<String>, AppError> {
    let id = parse_id(&id)?;
    let dead = app
        .tmux
        .list()
        .await
        .ok()
        .and_then(|p| p.get(&id).map(|p| p.dead))
        .unwrap_or(false);
    let text = match app.tmux.capture(&id, dead).await {
        Ok(t) if t.trim().is_empty() => "(пусто)".into(),
        Ok(t) => t,
        Err(e) => format!("нет экрана: {e:#}"),
    };
    render(ScreenTpl { text })
}

async fn htmx_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_bytes!("../static/htmx.min.js").as_slice(),
    )
}

async fn style_css() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("../static/style.css"),
    )
}
