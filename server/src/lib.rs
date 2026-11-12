use axum::{
    extract::{DefaultBodyLimit, Multipart, Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json},
    routing::{get, post},
    Router,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::io::AsyncWriteExt;
use tower_http::services::ServeDir;

const MAX_UPLOAD: usize = 2 * 1024 * 1024 * 1024; // 2 GB

#[derive(Clone)]
struct AppState {
    db: Arc<Mutex<Connection>>,
    video_dir: PathBuf,
}

#[derive(Serialize)]
struct Video {
    id: String,
    title: String,
    mime: String,
    size_bytes: i64,
    duration_ms: i64,
    views: i64,
    created_at: String,
    url: String,
    share_url: String,
}

fn row_to_video(r: &rusqlite::Row) -> rusqlite::Result<Video> {
    let id: String = r.get(0)?;
    let mime: String = r.get(2)?;
    let ext = ext_for(&mime);
    Ok(Video {
        url: format!("/media/{id}.{ext}"),
        share_url: format!("/v/{id}"),
        title: r.get(1)?,
        mime,
        size_bytes: r.get(3)?,
        duration_ms: r.get(4)?,
        views: r.get(5)?,
        created_at: r.get(6)?,
        id,
    })
}

const SELECT: &str =
    "SELECT id, title, mime, size_bytes, duration_ms, views, created_at FROM videos";

fn ext_for(mime: &str) -> &'static str {
    if mime.starts_with("video/mp4") {
        "mp4"
    } else {
        "webm"
    }
}

type ApiResult<T> = Result<T, (StatusCode, String)>;

fn internal<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// Build the HTTP router. `data_dir` holds `reka.db` and `videos/`.
pub fn router(data_dir: &std::path::Path) -> Router {
    let video_dir = data_dir.join("videos");
    std::fs::create_dir_all(&video_dir).expect("create data dir");

    let conn = Connection::open(data_dir.join("reka.db")).expect("open db");
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS videos (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            mime TEXT NOT NULL,
            size_bytes INTEGER NOT NULL,
            duration_ms INTEGER NOT NULL DEFAULT 0,
            views INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL
        );",
    )
    .expect("migrate");

    let state = AppState {
        db: Arc::new(Mutex::new(conn)),
        video_dir: video_dir.clone(),
    };

    Router::new()
        .route("/", get(|| async { Html(include_str!("../static/index.html")) }))
        .route("/v/:id", get(|| async { Html(include_str!("../static/watch.html")) }))
        .route("/api/videos", get(list_videos).post(upload_video))
        .route(
            "/api/videos/:id",
            get(get_video).patch(rename_video).delete(delete_video),
        )
        .route("/api/videos/:id/view", post(add_view))
        // ServeDir handles HTTP Range requests, so seeking works in <video>.
        .nest_service("/media", ServeDir::new(video_dir))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD))
        // The desktop app (tauri:// origin) calls this API from its webview.
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(state)
}

/// Serve until the process exits.
pub async fn serve(listener: tokio::net::TcpListener, data_dir: &std::path::Path) -> std::io::Result<()> {
    axum::serve(listener, router(data_dir)).await
}

async fn list_videos(State(s): State<AppState>) -> ApiResult<Json<Vec<Video>>> {
    let db = s.db.lock().unwrap();
    let mut stmt = db
        .prepare(&format!("{SELECT} ORDER BY created_at DESC"))
        .map_err(internal)?;
    let rows = stmt
        .query_map([], row_to_video)
        .map_err(internal)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(internal)?;
    Ok(Json(rows))
}

async fn get_video(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Video>> {
    let db = s.db.lock().unwrap();
    db.query_row(&format!("{SELECT} WHERE id = ?1"), [&id], row_to_video)
        .optional()
        .map_err(internal)?
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, "video not found".into()))
}

/// multipart fields: `file` (the recording), optional `title`, optional `duration_ms`.
async fn upload_video(
    State(s): State<AppState>,
    mut mp: Multipart,
) -> ApiResult<(StatusCode, Json<Video>)> {
    let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
    let mut title = String::new();
    let mut duration_ms: i64 = 0;
    let mut saved: Option<(PathBuf, String, i64)> = None;

    while let Some(mut field) = mp
        .next_field()
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
    {
        match field.name().unwrap_or("") {
            "title" => title = field.text().await.unwrap_or_default(),
            "duration_ms" => {
                duration_ms = field.text().await.unwrap_or_default().parse().unwrap_or(0)
            }
            "file" => {
                let mime = field
                    .content_type()
                    .unwrap_or("video/webm")
                    .split(';')
                    .next()
                    .unwrap()
                    .to_string();
                if !mime.starts_with("video/") {
                    return Err((StatusCode::BAD_REQUEST, "file must be a video".into()));
                }
                let path = s.video_dir.join(format!("{id}.{}", ext_for(&mime)));
                let mut f = tokio::fs::File::create(&path).await.map_err(internal)?;
                let mut size = 0i64;
                // Stream chunks to disk instead of buffering the whole video in RAM.
                while let Some(chunk) = field
                    .chunk()
                    .await
                    .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
                {
                    size += chunk.len() as i64;
                    f.write_all(&chunk).await.map_err(internal)?;
                }
                f.flush().await.map_err(internal)?;
                saved = Some((path, mime, size));
            }
            _ => {}
        }
    }

    let (_path, mime, size) =
        saved.ok_or((StatusCode::BAD_REQUEST, "missing `file` field".into()))?;
    if title.trim().is_empty() {
        title = format!("Recording {}", chrono::Local::now().format("%d %b %Y %H:%M"));
    }
    let created_at = chrono::Utc::now().to_rfc3339();

    let db = s.db.lock().unwrap();
    db.execute(
        "INSERT INTO videos (id, title, mime, size_bytes, duration_ms, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, title.trim(), mime, size, duration_ms, created_at],
    )
    .map_err(internal)?;
    let v = db
        .query_row(&format!("{SELECT} WHERE id = ?1"), [&id], row_to_video)
        .map_err(internal)?;
    Ok((StatusCode::CREATED, Json(v)))
}

#[derive(Deserialize)]
struct Rename {
    title: String,
}

async fn rename_video(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Rename>,
) -> ApiResult<StatusCode> {
    let t = body.title.trim();
    if t.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "title is empty".into()));
    }
    let n = s
        .db
        .lock()
        .unwrap()
        .execute("UPDATE videos SET title = ?1 WHERE id = ?2", params![t, id])
        .map_err(internal)?;
    Ok(if n == 0 { StatusCode::NOT_FOUND } else { StatusCode::NO_CONTENT })
}

async fn delete_video(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let mime: Option<String> = {
        let db = s.db.lock().unwrap();
        let mime = db
            .query_row("SELECT mime FROM videos WHERE id = ?1", [&id], |r| r.get(0))
            .optional()
            .map_err(internal)?;
        db.execute("DELETE FROM videos WHERE id = ?1", [&id])
            .map_err(internal)?;
        mime
    };
    match mime {
        None => Ok(StatusCode::NOT_FOUND),
        Some(m) => {
            let _ = tokio::fs::remove_file(s.video_dir.join(format!("{id}.{}", ext_for(&m)))).await;
            Ok(StatusCode::NO_CONTENT)
        }
    }
}

async fn add_view(State(s): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let _ = s
        .db
        .lock()
        .unwrap()
        .execute("UPDATE videos SET views = views + 1 WHERE id = ?1", [&id]);
    StatusCode::NO_CONTENT
}
