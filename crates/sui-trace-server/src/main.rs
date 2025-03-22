use std::env;
use axum::{
    extract,
    routing::get,
    Router,
    Json
};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sui_replay::call_trace::CallTraceWithSource;

pub const DEFAULT_PORT: u16 = 9301;

#[derive(Clone)]
struct AppConfig {
    rpc_url: String,
    local_exec: sui_replay::replay::LocalExec,
}

impl AppConfig {
    async fn new(rpc_url: String) -> Self {
        Self {
            rpc_url: rpc_url.clone(),
            local_exec: sui_replay::init_for_tracer(rpc_url).await.unwrap(),
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::FmtSubscriber::builder()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()
        .expect("setting default subscriber failed");

    // Get RPC URL from command-line arguments or use a default value
    let rpc_url = env::args().nth(1).unwrap_or_else(|| "https://fullnode.mainnet.sui.io:443".to_string());
    let config = AppConfig::new(rpc_url.clone()).await;
    // Log configuration
    println!("RPC Url: {:?}", config.rpc_url);
    // build our application with a single route
    println!("listening on http://localhost:{}", DEFAULT_PORT);

    let app = Router::new()
        .route("/call_trace/by_tx_digest/:hash", get(call_trace).with_state(config.clone()))
        .route("/call_trace/v2/by_tx_digest/:hash", get(call_trace).with_state(config.clone()));

    axum_server::Server::bind(format!("0.0.0.0:{}", DEFAULT_PORT).parse().unwrap())
        .serve(app.into_make_service())
        .await.unwrap();
}

async fn call_trace(
    extract::Path(tx_digest): extract::Path<String>,
    State(config): State<AppConfig>,
) -> Result<Json<Option<Vec<CallTraceWithSource>>>, MyError> {
    let trace_result = sui_replay::execute_call_trace(
        config.local_exec.clone(),
        tx_digest,
        false,
        false,
    ).await;
    match trace_result {
        Ok(res) => {
            Ok(Json(res))
        }
        Err(err) => {
            Err(MyError::SomethingWentWrong {
                message: format!("{:?}", err),
            })
        }
    }
}

async fn call_trace_v2(
    extract::Path(tx_digest): extract::Path<String>,
    State(config): State<AppConfig>,
) -> Result<Json<Option<Vec<CallTraceWithSource>>>, MyError> {
    call_trace(extract::Path(tx_digest), State(config)).await
}

enum MyError {
    SomethingWentWrong {
        message: String,
    },
}

impl IntoResponse for MyError {
    fn into_response(self) -> Response {
        let body = match self {
            MyError::SomethingWentWrong { message } => {
                format!("Something went wrong: {}", message)
            }
        };

        // its often easiest to implement `IntoResponse` by calling other implementations
        (StatusCode::INTERNAL_SERVER_ERROR, body).into_response()
    }
}
