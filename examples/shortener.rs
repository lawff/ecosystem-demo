use axum::{
    extract::{Path, State},
    http::{
        header::{InvalidHeaderValue, LOCATION},
        HeaderMap, StatusCode,
    },
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use nanoid::nanoid;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{prelude::FromRow, PgPool};
use thiserror::Error;
use tokio::net::TcpListener;
use tracing::{info, level_filters::LevelFilter};
use tracing_subscriber::{fmt::Layer, layer::SubscriberExt, util::SubscriberInitExt, Layer as _};

const LISTEN_ADDR: &str = "0.0.0.0:8087";

#[derive(Debug, Error)]
enum AppError {
    #[error("sql error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("invalid header value: {0}")]
    InvalidHeaderValue(#[from] InvalidHeaderValue),
    #[error("retry limit exceeded: {0}")]
    RetryLimitExceeded(String),
}

#[derive(Debug, Clone)]
struct AppState {
    db: PgPool,
}

#[derive(Debug, Deserialize)]
struct ShortenReq {
    url: String,
}

#[derive(Debug, Serialize)]
struct ShortenRes {
    url: String,
}

#[derive(Debug, FromRow)]
struct RecordUrl {
    #[sqlx(default)]
    id: String,
    #[sqlx(default)]
    url: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let layer = Layer::new().with_filter(LevelFilter::INFO);
    tracing_subscriber::registry().with(layer).init();

    // 连接数据库
    let db_url = "postgres://lawliet:password@localhost:5432/shortener";
    let state = AppState::try_new(db_url).await?;
    info!("Connected to database {}", db_url);

    let listener = TcpListener::bind(LISTEN_ADDR).await?;
    info!("Listening on: {}", LISTEN_ADDR);

    let app = Router::new()
        .route("/", post(shorten))
        .route("/:id", get(redirect))
        .with_state(state);

    axum::serve(listener, app.into_make_service()).await?;

    Ok(())
}

async fn shorten(
    State(state): State<AppState>,
    Json(data): Json<ShortenReq>,
) -> Result<impl IntoResponse, AppError> {
    let url = state.shorten(&data.url).await?;

    Ok(Json(ShortenRes {
        url: format!("http://{}/{}", LISTEN_ADDR, url),
    }))
}

async fn redirect(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    let url = state.get_url(&id).await?;

    info!("Redirecting to {}", url);

    let mut headers = HeaderMap::new();
    headers.insert(LOCATION, url.parse()?);

    Ok((StatusCode::PERMANENT_REDIRECT, headers))
}

impl AppState {
    async fn try_new(url: &str) -> anyhow::Result<Self> {
        let db = PgPool::connect(url).await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS urls (
                id CHAR(6) PRIMARY KEY,
                url TEXT NOT NULL UNIQUE
            )
            "#,
        )
        .execute(&db)
        .await?;

        Ok(Self { db })
    }

    async fn shorten(&self, url: &str) -> Result<String, AppError> {
        let mut id = nanoid!(6);
        let mut retries = 0;
        let max_retries = 5;

        loop {
            match sqlx::query_as::<_, RecordUrl>("INSERT INTO urls (id, url) VALUES ($1, $2) ON CONFLICT(url) DO UPDATE SET url=EXCLUDED.url RETURNING id")
              .bind(&id)
              .bind(url)
              .fetch_one(&self.db)
              .await {
                Ok(ret) => return Ok(ret.id),
                Err(sqlx::Error::Database(pg_err)) => {
                    // 需要用到 pg_err.code().as_deref() == Some("23505") 吗? 23505 有没有更好的方式
                    if pg_err.constraint() == Some("urls_pkey") {
                        // 主键冲突，重试
                        if retries < max_retries {
                            retries += 1;
                            id = nanoid!(6);
                            continue;
                        } else {
                            return Err(AppError::RetryLimitExceeded(url.to_string()));
                        }
                    } else {
                        return Err(sqlx::Error::Database(pg_err).into());
                    }
                }
                Err(e) => return Err(e.into()),
              }
        }
    }

    async fn get_url(&self, id: &str) -> Result<String, AppError> {
        let ret: RecordUrl = sqlx::query_as("SELECT * FROM urls WHERE id = $1")
            .bind(id)
            .fetch_one(&self.db)
            .await?;
        Ok(ret.url)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match self {
            AppError::Sqlx(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::InvalidHeaderValue(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::RetryLimitExceeded(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };

        let body = Json(json!({ "error": self.to_string() }));

        (status, body).into_response()
    }
}
