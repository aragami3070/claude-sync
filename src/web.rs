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
    git::check_ssh_url,
    sessions::Session,
    tmux::Pane,
    util::{ago, find_git_root, resolve_user_path, tilde, truncate_chars},
};

pub fn router(app: Arc<App>) -> Router {
    let protected = Router::new()
        .route("/", get(index))
        .route("/sessions", get(list))
        .route("/new", post(new_session))
        .route("/sessions/{id}/row", get(row))
        .route("/sessions/{id}/screen", get(screen))
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
        let state = app.git.read_state(&id);
        let has_state = state.is_some();
        let repo_ok = s.is_none_or(|s| s.git_root().is_some());
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
            .or_else(|| state.map(|st| st.cwd));
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
            stop_label: if alive { "Stop" } else { "Вывезти изменения" },
            has_pane: pane.is_some(),
            rc_url,
            conflict: s.is_some_and(|s| s.has_conflicts),
            no_repo: !repo_ok,
            continued_in: s.and_then(|s| s.parsed.continued_in.map(|id| id.to_string())),
            msg,
            msg_error,
        }
    }
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTpl {
    node: String,
    dirs: Vec<String>,
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

    let mut rows: Vec<RowView> = extra
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
        let keep = app.cfg.list_limit.max(rows.iter().filter(|r| r.status != "stopped").count());
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
        .filter_map(Session::git_root)
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
    if s.is_none() && pane.is_none() && op.is_none() && app.git.read_state(&id).is_none() {
        return None;
    }
    Some(RowView::build(app, id, s.as_ref(), fresh.as_ref(), pane, op.as_ref()))
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
) -> Result<Html<String>, AppError> {
    let id = parse_id(&id)?;
    if row_view(&app, id).await.is_none() {
        return Err(AppError(StatusCode::NOT_FOUND, "сессия не найдена".into()));
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
            if find_git_root(&cwd).is_none() {
                anyhow::bail!("в {} нет git-репозитория", tilde(&cwd, &app.cfg.home));
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
