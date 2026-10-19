use std::{collections::HashMap, sync::Arc};

use askama::Template;
use axum::{
    Router,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    middleware,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    app::{App, Op},
    auth,
    sessions::Session,
    tmux::Pane,
    util::{ago, tilde},
};

pub fn router(app: Arc<App>) -> Router {
    let protected = Router::new()
        .route("/", get(index))
        .route("/sessions", get(list))
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
    fn build(app: &App, s: &Session, pane: Option<&Pane>, op: Option<&Op>) -> Self {
        let has_state = app.git.read_state(&s.id).is_some();
        let repo_ok = s.git_root().is_some();
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

        Self {
            id: s.id,
            title: s.title(),
            cwd: s
                .cwd()
                .map(|c| tilde(c, &app.cfg.home))
                .unwrap_or_else(|| "?".into()),
            branch: s.parsed.git_branch.clone(),
            ago: ago(s.mtime),
            status,
            status_label,
            poll,
            can_start: !poll && pane.is_none() && !has_state && repo_ok,
            can_stop: !poll && (pane.is_some() || has_state),
            stop_label: if alive { "Stop" } else { "Вывезти изменения" },
            has_pane: pane.is_some(),
            rc_url: if alive { s.rc_url() } else { None },
            conflict: s.has_conflicts,
            no_repo: !repo_ok,
            continued_in: s.parsed.continued_in.map(|id| id.to_string()),
            msg,
            msg_error,
        }
    }
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTpl {
    node: String,
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

    let mut rows: Vec<RowView> = sessions
        .iter()
        .map(|s| RowView::build(app, s, panes.get(&s.id), ops.get(&s.id)))
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

async fn index(
    State(app): State<Arc<App>>,
    Query(q): Query<ListQuery>,
) -> Result<Html<String>, AppError> {
    let all = q.all.is_some();
    let (rows, hidden, warning) = collect_rows(&app, all).await;
    render(IndexTpl {
        node: app.cfg.node_name.clone(),
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

async fn row_html(app: &App, id: Uuid) -> Result<Html<String>, AppError> {
    let s = app
        .session(id)
        .await
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "сессия не найдена".into()))?;
    let panes = app.tmux.list().await.unwrap_or_default();
    let op = app.op(&id);
    render(RowTpl {
        row: RowView::build(app, &s, panes.get(&id), op.as_ref()),
    })
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
    if app.session(id).await.is_none() {
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
    row_html(&app, id).await
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
