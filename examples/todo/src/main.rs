#[macro_use] extern crate diesel;

#[cfg(test)]
mod tests;
mod task;

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post, put};
use axum::{Form, Router};

use cookie::{Cookie, SameSite};

use diesel::r2d2::{ConnectionManager, Pool};
use diesel::SqliteConnection;

use serde::Serialize;
use tera::Tera;
use tower_http::services::ServeDir;

use crate::task::{Task, Todo};

/// Where the SQLite database lives, relative to the crate's working directory.
pub const DATABASE_URL: &str = "db/db.sqlite";

/// Directory holding both the templates and the static assets.
const STATIC_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/static");
const TEMPLATE_GLOB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/static/**/*.tera");
const INDEX_TEMPLATE: &str = "index.html.tera";

/// Name of the one-shot flash cookie.
const FLASH_COOKIE: &str = "_flash";

/// A blocking Diesel connection pool with an async `run` shim.
#[derive(Clone)]
pub struct DbConn(Pool<ConnectionManager<SqliteConnection>>);

impl DbConn {
    pub fn new(url: &str) -> DbConn {
        let manager = ConnectionManager::<SqliteConnection>::new(url);
        let pool = Pool::builder().build(manager).expect("database pool");
        DbConn(pool)
    }

    /// Runs `f` against a pooled connection on a blocking thread.
    pub async fn run<F, R>(&self, f: F) -> R
        where F: FnOnce(&mut SqliteConnection) -> R + Send + 'static,
              R: Send + 'static
    {
        let pool = self.0.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().expect("database connection");
            f(&mut conn)
        }).await.expect("database task")
    }
}

#[derive(Clone)]
struct AppState {
    conn: DbConn,
    tera: Arc<Tera>,
}

#[derive(Debug, Serialize)]
struct Context {
    flash: Option<(String, String)>,
    tasks: Vec<Task>
}

impl Context {
    pub async fn err<M: std::fmt::Display>(conn: &DbConn, msg: M) -> Context {
        Context {
            flash: Some(("error".into(), msg.to_string())),
            tasks: Task::all(conn).await.unwrap_or_default()
        }
    }

    pub async fn raw(conn: &DbConn, flash: Option<(String, String)>) -> Context {
        match Task::all(conn).await {
            Ok(tasks) => Context { flash, tasks },
            Err(e) => {
                eprintln!("DB Task::all() error: {}", e);
                Context {
                    flash: Some(("error".into(), "Fail to access database.".into())),
                    tasks: vec![]
                }
            }
        }
    }
}

/// Renders the index template, mirroring `Template::render("index", ..)`.
fn render(tera: &Tera, context: Context) -> Response {
    let ctx = tera::Context::from_serialize(&context).expect("template context");
    match tera.render(INDEX_TEMPLATE, &ctx) {
        Ok(body) => Html(body).into_response(),
        Err(e) => {
            eprintln!("Template render error: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Serializes a flash message into the wire format the reader below expects:
/// the kind's length, a colon, the kind, then the message.
fn flash_header(kind: &str, message: &str) -> HeaderValue {
    let content = format!("{}{}{}{}", kind.len(), ':', kind, message);
    let cookie = Cookie::build((FLASH_COOKIE, content))
        .max_age(cookie::time::Duration::minutes(5))
        .path("/")
        .same_site(SameSite::Strict)
        .build();

    HeaderValue::from_str(&cookie.encoded().to_string()).expect("flash cookie")
}

/// The expiring counterpart of `flash_header`: consuming a flash removes it.
fn flash_removal_header() -> HeaderValue {
    let mut cookie = Cookie::new(FLASH_COOKIE, "");
    cookie.set_path("/");
    cookie.set_same_site(SameSite::Lax);
    cookie.make_removal();

    HeaderValue::from_str(&cookie.encoded().to_string()).expect("flash cookie")
}

/// Reads and parses the flash cookie, if any, from the request headers.
fn read_flash(headers: &HeaderMap) -> Option<(String, String)> {
    for raw in headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()) {
        for part in raw.split(';') {
            let cookie = match Cookie::parse_encoded(part.trim().to_string()) {
                Ok(cookie) => cookie,
                Err(_) => continue,
            };

            if cookie.name() != FLASH_COOKIE {
                continue;
            }

            let (len_str, kv) = match cookie.value().split_once(':') {
                Some(split) => split,
                None => continue,
            };

            let len: usize = match len_str.parse() {
                Ok(len) => len,
                Err(_) => continue,
            };

            if len > kv.len() {
                continue;
            }

            let (kind, message) = kv.split_at(len);
            return Some((kind.to_string(), message.to_string()));
        }
    }

    None
}

/// A redirect that also sets a flash message.
fn flash_redirect(kind: &str, message: &str) -> Response {
    let mut response = Redirect::to("/").into_response();
    response.headers_mut().insert(header::SET_COOKIE, flash_header(kind, message));
    response
}

async fn new(State(state): State<AppState>, Form(todo): Form<Todo>) -> Response {
    if todo.description.is_empty() {
        flash_redirect("error", "Description cannot be empty.")
    } else if let Err(e) = Task::insert(todo, &state.conn).await {
        eprintln!("DB insertion error: {}", e);
        flash_redirect("error", "Todo could not be inserted due an internal error.")
    } else {
        flash_redirect("success", "Todo successfully added.")
    }
}

async fn toggle(State(state): State<AppState>, Path(id): Path<i32>) -> Response {
    match Task::toggle_with_id(id, &state.conn).await {
        Ok(_) => Redirect::to("/").into_response(),
        Err(e) => {
            eprintln!("DB toggle({}) error: {}", id, e);
            render(&state.tera, Context::err(&state.conn, "Failed to toggle task.").await)
        }
    }
}

async fn delete(State(state): State<AppState>, Path(id): Path<i32>) -> Response {
    match Task::delete_with_id(id, &state.conn).await {
        Ok(_) => flash_redirect("success", "Todo was deleted."),
        Err(e) => {
            eprintln!("DB deletion({}) error: {}", id, e);
            render(&state.tera, Context::err(&state.conn, "Failed to delete task.").await)
        }
    }
}

async fn index(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let flash = read_flash(&headers);
    let consumed = flash.is_some();
    let mut response = render(&state.tera, Context::raw(&state.conn, flash).await);
    if consumed {
        response.headers_mut().append(header::SET_COOKIE, flash_removal_header());
    }

    response
}

/// Longest override directive that is honoured: `_method=delete`.
const MAX_METHOD_OVERRIDE: usize = 14;

/// Rewrites the method of a form POST whose *first* field is `_method`, so the
/// browser-only form verbs in the template keep reaching PUT and DELETE routes.
async fn method_override(request: Request, next: Next) -> Response {
    let is_form = request.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or("").trim())
        .map_or(false, |ty| ty.eq_ignore_ascii_case("application/x-www-form-urlencoded"));

    if request.method() != Method::POST || !is_form {
        return next.run(request).await;
    }

    let (mut parts, body) = request.into_parts();
    let bytes: Bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    let peek = &bytes[..std::cmp::min(bytes.len(), MAX_METHOD_OVERRIDE)];
    if let Ok(peek) = std::str::from_utf8(peek) {
        if let Some((name, value)) = peek.split('&').next().and_then(|f| f.split_once('=')) {
            if name == "_method" {
                if let Ok(method) = Method::from_bytes(value.to_uppercase().as_bytes()) {
                    parts.method = method;
                }
            }
        }
    }

    next.run(Request::from_parts(parts, Body::from(bytes))).await
}

/// Applies pending migrations and assembles the application.
pub async fn build(conn: DbConn) -> Router {
    use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};

    const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

    conn.run(|c| { c.run_pending_migrations(MIGRATIONS).expect("diesel migrations"); }).await;

    let tera = Tera::new(TEMPLATE_GLOB).expect("templates");
    let state = AppState { conn, tera: Arc::new(tera) };

    let routes = Router::new()
        .route("/", get(index))
        .route("/todo", post(new))
        .route("/todo/:id", put(toggle).delete(delete))
        .fallback_service(ServeDir::new(STATIC_DIR))
        .with_state(state);

    // The method override has to rewrite the method *before* anything is matched,
    // so it wraps the routing table instead of living inside it.
    Router::new()
        .fallback_service(routes)
        .layer(middleware::from_fn(method_override))
}

#[tokio::main]
async fn main() {
    let app = build(DbConn::new(DATABASE_URL)).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8000").await
        .expect("failed to bind address");

    println!("Serving on http://{}", listener.local_addr().expect("local address"));
    axum::serve(listener, app).await.expect("failed to start server");
}
