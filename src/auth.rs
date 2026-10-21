use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use subtle::ConstantTimeEq;

use crate::app::App;

fn credentials_ok(headers: &HeaderMap, user: &str, pass: &str) -> bool {
    let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
    else {
        return false;
    };
    let Ok(decoded) = STANDARD.decode(value.trim()) else {
        return false;
    };
    let expected = format!("{user}:{pass}");
    decoded.ct_eq(expected.as_bytes()).into()
}

/// Basic auth на всё. На POST дополнительно нужен `HX-Request`: чужая страница
/// не может поставить такой заголовок, а basic-креды браузер приложил бы сам.
pub async fn layer(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    if !credentials_ok(req.headers(), &app.cfg.auth_user, &app.cfg.auth_pass) {
        return Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(
                header::WWW_AUTHENTICATE,
                r#"Basic realm="claude-sync", charset="UTF-8""#,
            )
            .body(Body::from("нужна авторизация"))
            .unwrap();
    }
    if req.method() == Method::POST && req.headers().get("hx-request").is_none() {
        return (StatusCode::FORBIDDEN, "POST только из интерфейса").into_response();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn h(v: &str) -> HeaderMap {
        let mut m = HeaderMap::new();
        m.insert(header::AUTHORIZATION, HeaderValue::from_str(v).unwrap());
        m
    }

    #[test]
    fn basic() {
        let good = format!("Basic {}", STANDARD.encode("u:p:x"));
        assert!(credentials_ok(&h(&good), "u", "p:x"));
        assert!(!credentials_ok(&h(&good), "u", "p"));
        assert!(!credentials_ok(&h("Basic !!!"), "u", "p:x"));
        assert!(!credentials_ok(&HeaderMap::new(), "u", "p:x"));
    }
}
