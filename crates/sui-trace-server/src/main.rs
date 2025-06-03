use std::collections::HashMap;
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
use serde::{Deserialize, Serialize};
use sui_replay::call_trace::CallTraceWithSource;

pub const DEFAULT_PORT: u16 = 9301;

#[derive(Clone)]
struct AppConfig {
    networks: HashMap<String, NetworkConfig>
}

impl AppConfig {
    async fn new(rpc_urls: String) -> Self {
        let chain_to_endpoint: Vec<&str> = rpc_urls.split(",").collect();
        let mut networks: HashMap<String, NetworkConfig> = HashMap::new();
        for i in chain_to_endpoint.iter() {
            let t: Vec<&str> = i.split('=').collect();
            let network = NetworkConfig::new(t[1].to_string()).await;
            networks.insert(t[0].to_string(), network);
        }
        Self {
            networks
        }
    }
}

#[derive(Clone)]
struct NetworkConfig {
    rpc_url: String,
    local_exec: sui_replay::replay::LocalExec,
}

impl NetworkConfig {
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
    let rpc_urls = env::args().nth(1).unwrap_or_else(|| "sui_mainnet=https://fullnode.mainnet.sui.io:443".to_string());
    let config = AppConfig::new(rpc_urls.clone()).await;
    // Log configuration
    println!("RPC Urls: {:?}", rpc_urls);
    // build our application with a single route
    println!("listening on http://localhost:{}", DEFAULT_PORT);

    let app = Router::new()
        .route("/{chain_id}/call_trace/by_tx_digest/{hash}", get(call_trace).with_state(config.clone()));

    axum_server::Server::bind(format!("0.0.0.0:{}", DEFAULT_PORT).parse().unwrap())
        .serve(app.into_make_service())
        .await.unwrap();
}

async fn call_trace(
    extract::Path((chain_id, tx_digest)): extract::Path<(String, String)>,
    State(config): State<AppConfig>,
) -> Result<Json<Option<Vec<CallTraceWithSource>>>, MyError> {
    let conf = config.networks.get(&chain_id)
        .ok_or(MyError::SomethingWentWrong {
            message: format!("Network {} not found", chain_id),
        })?;
    let trace_result = sui_replay::execute_call_trace(
        conf.local_exec.clone(),
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

enum MyError {
    SomethingWentWrong {
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
struct ErrorResponse {
    error: String
}

impl IntoResponse for MyError {
    fn into_response(self) -> Response {
        let body = match self {
            MyError::SomethingWentWrong { message } => {
                Json(ErrorResponse { error: message })
            }
        };

        // its often easiest to implement `IntoResponse` by calling other implementations
        (StatusCode::INTERNAL_SERVER_ERROR, body).into_response()
    }
}
