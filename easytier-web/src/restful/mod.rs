mod auth;
pub(crate) mod captcha;
mod network;
pub(crate) mod oidc;
mod rpc;
mod users;

use std::{net::SocketAddr, sync::Arc};

use axum::extract::Path;
use axum::http::{Request, StatusCode, header};
use axum::middleware::{self as axum_mw, Next};
use axum::response::Response;
use axum::routing::{delete, post};
use axum::{Extension, Json, Router, extract::State, routing::get};
use axum_login::{AuthManagerLayerBuilder, AuthUser, AuthzBackend, login_required};
use axum_messages::MessagesManagerLayer;
use easytier::common::config::{ConfigLoader, TomlConfigLoader};
use easytier::launcher::NetworkConfig;
use easytier::proto::rpc_types;
use network::NetworkApi;
use sea_orm::DbErr;
use tokio::net::TcpListener;
use tokio_util::task::AbortOnDropHandle;
use tower_sessions::cookie::time::Duration;
use tower_sessions::cookie::{Key, SameSite};
use tower_sessions::session::{Id, Record};
use tower_sessions::session_store::{self, ExpiredDeletion};
use tower_sessions::{Expiry, SessionManagerLayer, SessionStore};
use tower_sessions_sqlx_store::{MySqlStore, PostgresStore, SqliteStore};
use users::{AuthSession, Backend};

use crate::FeatureFlags;
use crate::client_manager::ClientManager;
use crate::client_manager::storage::StorageToken;
use crate::db::{Db, DbPool, UserIdInDb};
use crate::webhook::SharedWebhookConfig;

/// Embed assets for web dashboard, build frontend first
#[cfg(feature = "embed")]
#[derive(rust_embed::RustEmbed, Clone)]
#[folder = "frontend/dist/"]
struct Assets;

pub struct RestfulServer {
    bind_addr: SocketAddr,
    client_mgr: Arc<ClientManager>,
    feature_flags: Arc<FeatureFlags>,
    webhook_config: SharedWebhookConfig,
    db: Db,
    oidc_config: oidc::OidcConfig,
    web_router: Option<Router>,
}

type AppStateInner = Arc<ClientManager>;
type AppState = State<AppStateInner>;

#[derive(Debug, Clone)]
enum SqlxSessionStore {
    Sqlite(SqliteStore),
    Postgres(PostgresStore),
    MySql(MySqlStore),
}

impl SqlxSessionStore {
    fn from_db(db: &Db) -> anyhow::Result<Self> {
        Ok(match db.pool() {
            DbPool::Sqlite(pool) => Self::Sqlite(SqliteStore::new(pool.clone())),
            DbPool::Postgres(pool) => Self::Postgres(
                PostgresStore::new(pool.clone())
                    .with_schema_name("public")
                    .map_err(|e| anyhow::anyhow!(e))?
                    .with_table_name("tower_sessions")
                    .map_err(|e| anyhow::anyhow!(e))?,
            ),
            DbPool::MySql(pool) => {
                let schema_name = db.database_name().ok_or_else(|| {
                    anyhow::anyhow!("mysql database URL must include a database name")
                })?;

                Self::MySql(
                    MySqlStore::new(pool.clone())
                        .with_schema_name(schema_name)
                        .map_err(|e| anyhow::anyhow!(e))?
                        .with_table_name("tower_sessions")
                        .map_err(|e| anyhow::anyhow!(e))?,
                )
            }
        })
    }

    async fn migrate(&self) -> sqlx::Result<()> {
        match self {
            Self::Sqlite(store) => store.migrate().await,
            Self::Postgres(store) => store.migrate().await,
            Self::MySql(store) => store.migrate().await,
        }
    }
}

#[async_trait::async_trait]
impl ExpiredDeletion for SqlxSessionStore {
    async fn delete_expired(&self) -> session_store::Result<()> {
        match self {
            Self::Sqlite(store) => store.delete_expired().await,
            Self::Postgres(store) => store.delete_expired().await,
            Self::MySql(store) => store.delete_expired().await,
        }
    }
}

#[async_trait::async_trait]
impl SessionStore for SqlxSessionStore {
    async fn create(&self, record: &mut Record) -> session_store::Result<()> {
        match self {
            Self::Sqlite(store) => store.create(record).await,
            Self::Postgres(store) => store.create(record).await,
            Self::MySql(store) => store.create(record).await,
        }
    }

    async fn save(&self, record: &Record) -> session_store::Result<()> {
        match self {
            Self::Sqlite(store) => store.save(record).await,
            Self::Postgres(store) => store.save(record).await,
            Self::MySql(store) => store.save(record).await,
        }
    }

    async fn load(&self, session_id: &Id) -> session_store::Result<Option<Record>> {
        match self {
            Self::Sqlite(store) => store.load(session_id).await,
            Self::Postgres(store) => store.load(session_id).await,
            Self::MySql(store) => store.load(session_id).await,
        }
    }

    async fn delete(&self, session_id: &Id) -> session_store::Result<()> {
        match self {
            Self::Sqlite(store) => store.delete(session_id).await,
            Self::Postgres(store) => store.delete(session_id).await,
            Self::MySql(store) => store.delete(session_id).await,
        }
    }
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct ListSessionJsonResp(Vec<StorageToken>);

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct GetSummaryJsonResp {
    device_count: u32,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct GenerateConfigRequest {
    config: NetworkConfig,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct GenerateConfigResponse {
    error: Option<String>,
    toml_config: Option<String>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct ParseConfigRequest {
    toml_config: String,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct ParseConfigResponse {
    error: Option<String>,
    config: Option<NetworkConfig>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct Error {
    message: String,
}
type RpcError = rpc_types::error::Error;
type HttpHandleError = (StatusCode, Json<Error>);

pub fn other_error<T: ToString>(error_message: T) -> Error {
    Error {
        message: error_message.to_string(),
    }
}

pub fn convert_db_error(e: DbErr) -> HttpHandleError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        other_error(format!("DB Error: {:#}", e)).into(),
    )
}

impl RestfulServer {
    pub async fn new(
        bind_addr: SocketAddr,
        client_mgr: Arc<ClientManager>,
        db: Db,
        web_router: Option<Router>,
        feature_flags: Arc<FeatureFlags>,
        oidc_config: oidc::OidcConfig,
        webhook_config: SharedWebhookConfig,
    ) -> anyhow::Result<Self> {
        assert!(client_mgr.is_running());

        Ok(RestfulServer {
            bind_addr,
            client_mgr,
            feature_flags,
            webhook_config,
            db,
            oidc_config,
            web_router,
        })
    }

    async fn handle_list_all_sessions(
        auth_session: AuthSession,
        State(client_mgr): AppState,
    ) -> Result<Json<ListSessionJsonResp>, HttpHandleError> {
        let perms = auth_session
            .backend
            .get_group_permissions(auth_session.user.as_ref().unwrap())
            .await
            .unwrap();
        println!("{:?}", perms);
        let ret = client_mgr.list_sessions().await;
        Ok(ListSessionJsonResp(ret).into())
    }

    async fn handle_get_summary(
        auth_session: AuthSession,
        State(client_mgr): AppState,
    ) -> Result<Json<GetSummaryJsonResp>, HttpHandleError> {
        let Some(user) = auth_session.user else {
            return Err((StatusCode::UNAUTHORIZED, other_error("No such user").into()));
        };

        let machines = client_mgr.list_machine_by_user_id(user.id()).await;

        Ok(GetSummaryJsonResp {
            device_count: machines.len() as u32,
        }
        .into())
    }

    async fn handle_generate_config(
        Json(req): Json<GenerateConfigRequest>,
    ) -> Result<Json<GenerateConfigResponse>, HttpHandleError> {
        let config = req.config.gen_config();
        match config {
            Ok(c) => Ok(GenerateConfigResponse {
                error: None,
                toml_config: Some(c.dump()),
            }
            .into()),
            Err(e) => Ok(GenerateConfigResponse {
                error: Some(format!("{:?}", e)),
                toml_config: None,
            }
            .into()),
        }
    }

    async fn handle_parse_config(
        Json(req): Json<ParseConfigRequest>,
    ) -> Result<Json<ParseConfigResponse>, HttpHandleError> {
        let config = TomlConfigLoader::new_from_str(&req.toml_config)
            .and_then(|config| NetworkConfig::new_from_config(&config));
        match config {
            Ok(c) => Ok(ParseConfigResponse {
                error: None,
                config: Some(c),
            }
            .into()),
            Err(e) => Ok(ParseConfigResponse {
                error: Some(format!("{:?}", e)),
                config: None,
            }
            .into()),
        }
    }

    #[allow(unused_mut)]
    pub async fn start(
        mut self,
    ) -> Result<
        (
            AbortOnDropHandle<()>,
            AbortOnDropHandle<tower_sessions::session_store::Result<()>>,
        ),
        anyhow::Error,
    > {
        let listener = TcpListener::bind(self.bind_addr).await?;

        // Session layer.
        //
        // This uses `tower-sessions` to establish a layer that will provide the session
        // as a request extension.
        let session_store = SqlxSessionStore::from_db(&self.db)?;
        session_store.migrate().await?;

        let delete_task = AbortOnDropHandle::new(tokio::task::spawn(
            session_store
                .clone()
                .continuously_delete_expired(tokio::time::Duration::from_secs(60)),
        ));

        // Generate a cryptographic key to sign the session cookie.
        let key = Key::generate();

        let session_layer = SessionManagerLayer::new(session_store)
            .with_secure(false)
            .with_same_site(SameSite::Lax)
            .with_expiry(Expiry::OnInactivity(Duration::days(1)))
            .with_signed(key);

        // Auth service.
        //
        // This combines the session layer with our backend to establish the auth
        // service which will provide the auth session as a request extension.
        let backend = Backend::new(self.db.clone());
        let auth_layer = AuthManagerLayerBuilder::new(backend, session_layer).build();
        let compression_layer = tower_http::compression::CompressionLayer::new()
            .br(true)
            .deflate(true)
            .gzip(true)
            .zstd(true)
            .quality(tower_http::compression::CompressionLevel::Default);

        // Token-authenticated management routes that bypass session auth.
        let internal_app = if self.webhook_config.has_internal_auth() {
            let internal_token = self.webhook_config.internal_auth_token.clone().unwrap();
            let internal_routes = Router::new()
                .route(
                    "/api/internal/sessions",
                    get(Self::handle_list_all_sessions_internal),
                )
                .route(
                    "/api/internal/users/:user-id/sessions/:machine-id",
                    delete(Self::handle_disconnect_session_internal),
                )
                .merge(NetworkApi::build_route_internal())
                .merge(rpc::router_internal())
                .with_state(self.client_mgr.clone())
                .layer(axum_mw::from_fn(move |req, next| {
                    let token = internal_token.clone();
                    internal_auth_middleware(token, req, next)
                }));
            Some(internal_routes)
        } else {
            None
        };

        let mut app = Router::new()
            .route("/api/v1/summary", get(Self::handle_get_summary))
            .route("/api/v1/sessions", get(Self::handle_list_all_sessions))
            .merge(NetworkApi::build_route())
            .merge(rpc::router())
            .route_layer(login_required!(Backend))
            .merge(auth::router().layer(Extension(self.feature_flags.clone())))
            .merge(oidc::router())
            .with_state(self.client_mgr.clone())
            .route(
                "/api/v1/generate-config",
                post(Self::handle_generate_config),
            )
            .route("/api/v1/parse-config", post(Self::handle_parse_config))
            .layer(Extension(self.oidc_config.clone()))
            .layer(MessagesManagerLayer)
            .layer(auth_layer)
            .layer(tower_http::cors::CorsLayer::very_permissive())
            .layer(compression_layer);

        if let Some(internal_routes) = internal_app {
            app = app.merge(internal_routes);
        }

        #[cfg(feature = "embed")]
        let app = if let Some(web_router) = self.web_router.take() {
            app.merge(web_router)
        } else {
            app
        };

        let serve_task = AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));

        Ok((serve_task, delete_task))
    }

    /// Session listing endpoint for token-authenticated management clients.
    async fn handle_list_all_sessions_internal(
        State(client_mgr): AppState,
    ) -> Result<Json<ListSessionJsonResp>, HttpHandleError> {
        let ret = client_mgr.list_sessions().await;
        Ok(ListSessionJsonResp(ret).into())
    }

    async fn handle_disconnect_session_internal(
        Path((user_id, machine_id)): Path<(UserIdInDb, uuid::Uuid)>,
        State(client_mgr): AppState,
    ) -> Result<StatusCode, HttpHandleError> {
        if client_mgr
            .disconnect_session_by_machine_id(user_id, &machine_id)
            .await
        {
            Ok(StatusCode::NO_CONTENT)
        } else {
            Err((
                StatusCode::NOT_FOUND,
                other_error("session not found").into(),
            ))
        }
    }
}

/// Middleware that validates X-Internal-Auth for token-authenticated routes.
async fn internal_auth_middleware(
    expected_token: String,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let auth_header = req
        .headers()
        .get("X-Internal-Auth")
        .and_then(|v| v.to_str().ok());

    match auth_header {
        Some(token) if token == expected_token => next.run(req).await,
        _ => Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(
                r#"{"error":"unauthorized: invalid or missing X-Internal-Auth header"}"#,
            ))
            .unwrap(),
    }
}
