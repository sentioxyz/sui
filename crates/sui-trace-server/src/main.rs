// Copyright (c) Sentio
// SPDX-License-Identifier: Apache-2.0

mod trace_compat;

use std::{
    collections::HashMap,
    env,
    fs::File,
    net::SocketAddr,
    path::{Path as FsPath, PathBuf},
    process::Output,
    str::FromStr,
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use move_core_types::account_address::AccountAddress;
use move_trace_format::format::MoveTraceReader;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sui_data_store::{
    Node, ObjectKey, ObjectStore, VersionQuery,
    node::{MAINNET_GQL_URL, MAINNET_RPC_URL, TESTNET_GQL_URL, TESTNET_RPC_URL},
    stores::{ArchiveRpcDataStore, DataStore},
};
use sui_replay_2::{
    ReplayConfigExperimental, ReplayConfigStableInternal, StoreMode, handle_replay_config,
    run_replay,
};
use sui_types::{
    base_types::SuiAddress,
    coin::Coin,
    object::Object,
    transaction::{
        Argument, CallArg, Command as SuiCommand, ObjectArg, ProgrammableTransaction,
        TransactionData, TransactionDataAPI, TransactionKind,
    },
};
use tempfile::TempDir;
use tokio::process::Command;
use tower_http::compression::CompressionLayer;
use trace_compat::{CallTraceWithSource, TraceCompatibilityContext};

pub const DEFAULT_PORT: u16 = 9301;
const REPLAY_CHILD_ARG: &str = "__sentio-replay-call-trace";
const CHILD_STDIO_TAIL_BYTES: usize = 16 * 1024;

#[derive(Clone)]
struct AppConfig {
    networks: HashMap<String, NetworkConfig>,
}

impl AppConfig {
    fn new(networks: String) -> Result<Self> {
        let mut parsed = HashMap::new();
        for entry in networks.split(',').filter(|entry| !entry.trim().is_empty()) {
            let Some((chain_id, endpoint)) = entry.split_once('=') else {
                anyhow::bail!("invalid network entry '{entry}', expected chain_id=endpoint");
            };
            parsed.insert(
                chain_id.trim().to_owned(),
                NetworkConfig::new(chain_id.trim(), endpoint.trim())?,
            );
        }
        Ok(Self { networks: parsed })
    }
}

#[derive(Clone)]
struct NetworkConfig {
    node: Node,
    archive_rpc_url: Option<String>,
    chain_id: String,
    endpoint: String,
}

impl NetworkConfig {
    fn new(chain_id: &str, endpoint: &str) -> Result<Self> {
        Ok(Self {
            node: node_from_legacy_config(chain_id, endpoint)?,
            archive_rpc_url: archive_rpc_url_from_legacy_config(chain_id, endpoint),
            chain_id: chain_id.to_owned(),
            endpoint: endpoint.to_owned(),
        })
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::FmtSubscriber::builder()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()
        .expect("setting default subscriber failed");

    let mut args = env::args().skip(1);
    let first_arg = args.next();
    if first_arg.as_deref() == Some(REPLAY_CHILD_ARG) {
        if let Err(err) = run_replay_child(args).await {
            eprintln!("{err:?}");
            std::process::exit(1);
        }
        return;
    }

    let networks = first_arg.unwrap_or_else(|| format!("sui_mainnet={MAINNET_RPC_URL}"));
    let port = env::var("SUI_TRACE_SERVER_PORT")
        .ok()
        .and_then(|port| port.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);
    let config = AppConfig::new(networks).expect("invalid network configuration");

    let mut chain_ids = config.networks.keys().cloned().collect::<Vec<_>>();
    chain_ids.sort();
    println!("configured networks: {}", chain_ids.join(","));
    println!("listening on http://localhost:{port}");

    let app = Router::new()
        .route(
            "/{chain_id}/call_trace/by_tx_digest/{hash}",
            get(call_trace).with_state(config),
        )
        .layer(CompressionLayer::new().gzip(true));

    let addr: SocketAddr = format!("0.0.0.0:{port}").parse().unwrap();
    axum_server::Server::bind(addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

async fn call_trace(
    Path((chain_id, tx_digest)): Path<(String, String)>,
    State(config): State<AppConfig>,
) -> Result<Json<Option<Vec<CallTraceWithSource>>>, MyError> {
    let network = config
        .networks
        .get(&chain_id)
        .ok_or_else(|| MyError::new(format!("Network {chain_id} not found")))?;

    let result = execute_call_trace_isolated(network, &tx_digest)
        .await
        .map_err(|err| MyError::new(format!("{err:?}")))?;
    Ok(Json(result))
}

async fn run_replay_child(args: impl Iterator<Item = String>) -> Result<()> {
    let mut args = args;
    let chain_id = args.next().context("missing replay child chain id")?;
    let endpoint = args.next().context("missing replay child endpoint")?;
    let tx_digest = args
        .next()
        .context("missing replay child transaction digest")?;
    let output_path = PathBuf::from(args.next().context("missing replay child output path")?);
    if let Some(extra) = args.next() {
        anyhow::bail!("unexpected replay child argument '{extra}'");
    }

    let network = NetworkConfig::new(&chain_id, &endpoint)?;
    let result = execute_call_trace_in_process(&network, &tx_digest).await?;
    let output_file = File::create(&output_path)
        .with_context(|| format!("failed to create {}", output_path.display()))?;
    serde_json::to_writer(output_file, &result)
        .with_context(|| format!("failed to write {}", output_path.display()))?;
    Ok(())
}

async fn execute_call_trace_isolated(
    network: &NetworkConfig,
    tx_digest: &str,
) -> Result<Option<Vec<CallTraceWithSource>>> {
    let output_dir = TempDir::new().context("failed to create replay child temp dir")?;
    let output_path = output_dir.path().join("call_trace.json");
    let executable = env::current_exe().context("failed to locate current executable")?;
    let output = Command::new(executable)
        .arg(REPLAY_CHILD_ARG)
        .arg(&network.chain_id)
        .arg(&network.endpoint)
        .arg(tx_digest)
        .arg(&output_path)
        .kill_on_drop(true)
        .output()
        .await
        .context("failed to run replay child process")?;

    if !output.status.success() {
        anyhow::bail!("{}", replay_child_error(&output));
    }

    let output_file = File::open(&output_path)
        .with_context(|| format!("failed to open {}", output_path.display()))?;
    serde_json::from_reader(output_file)
        .with_context(|| format!("failed to parse {}", output_path.display()))
}

async fn execute_call_trace_in_process(
    network: &NetworkConfig,
    tx_digest: &str,
) -> Result<Option<Vec<CallTraceWithSource>>> {
    let output_dir = TempDir::new().context("failed to create replay temp dir")?;
    let stable_config = ReplayConfigStableInternal {
        digest: Some(tx_digest.to_owned()),
        digests_path: None,
        terminate_early: true,
        trace: true,
        output_dir: Some(output_dir.path().to_path_buf()),
        show_effects: false,
        overwrite: true,
    };
    let output_root = if let Some(archive_rpc_url) = &network.archive_rpc_url {
        let store = ArchiveRpcDataStore::new(
            network.node.clone(),
            archive_rpc_url,
            env!("CARGO_PKG_VERSION"),
        )?;
        let digests = vec![tx_digest.to_owned()];
        run_replay(
            &store,
            output_dir.path(),
            &digests,
            &network.node,
            stable_config.overwrite,
            stable_config.trace,
            false,
            stable_config.terminate_early,
            false,
            true,
        )
        .await?;
        output_dir.path().to_path_buf()
    } else {
        let experimental_config = ReplayConfigExperimental {
            node: network.node.clone(),
            verbose: false,
            store_mode: StoreMode::GqlOnly,
            track_time: false,
            cache_executor: true,
        };

        handle_replay_config(
            &stable_config,
            &experimental_config,
            env!("CARGO_PKG_VERSION"),
        )
        .await?
    };
    let trace_path = output_root
        .join(tx_digest)
        .join(PathBuf::from("trace.json.zst"));
    let transaction_data_path = output_root.join(tx_digest).join("transaction_data.json");
    let transaction_data = transaction_data_from_path(&transaction_data_path)
        .with_context(|| format!("failed to read {}", transaction_data_path.display()))?;
    if !trace_path.exists() {
        return Ok(Some(fallback_transfer_objects_traces(
            network,
            &transaction_data,
        )?));
    }
    let trace_context = trace_context_from_transaction_data(&transaction_data);
    let trace_file = std::fs::File::open(&trace_path)
        .with_context(|| format!("failed to open trace artifact {}", trace_path.display()))?;
    let reader = MoveTraceReader::new(trace_file)
        .with_context(|| format!("failed to read trace artifact {}", trace_path.display()))?;

    let mut roots = trace_compat::call_trace_from_reader_with_context(reader, trace_context)?;
    if matches!(&roots, Some(roots) if roots.is_empty()) {
        roots = Some(fallback_transfer_objects_traces(
            network,
            &transaction_data,
        )?);
    }
    Ok(roots)
}

fn transaction_data_from_path(path: &FsPath) -> Result<TransactionData> {
    let transaction_data_file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    serde_json::from_reader(transaction_data_file)
        .with_context(|| format!("failed to parse {}", path.display()))
}

fn trace_context_from_transaction_data(
    transaction_data: &TransactionData,
) -> TraceCompatibilityContext {
    TraceCompatibilityContext::new(transfer_recipients_from_transaction_data(transaction_data))
}

fn transfer_recipients_from_transaction_data(
    transaction_data: &TransactionData,
) -> Vec<Option<serde_json::Value>> {
    let TransactionKind::ProgrammableTransaction(ptb) = transaction_data.kind() else {
        return vec![];
    };
    ptb.commands
        .iter()
        .filter_map(|command| {
            if let SuiCommand::TransferObjects(_, recipient) = command {
                Some(transfer_recipient_to_json(ptb, recipient))
            } else {
                None
            }
        })
        .collect()
}

fn transfer_recipient_to_json(
    ptb: &ProgrammableTransaction,
    recipient: &Argument,
) -> Option<serde_json::Value> {
    let Argument::Input(index) = recipient else {
        return None;
    };
    let Some(CallArg::Pure(bytes)) = ptb.inputs.get(*index as usize) else {
        return None;
    };
    let recipient = bcs::from_bytes::<SuiAddress>(bytes).ok()?;
    let recipient: AccountAddress = recipient.into();
    serde_json::to_value(recipient).ok()
}

fn fallback_transfer_objects_traces(
    network: &NetworkConfig,
    transaction_data: &TransactionData,
) -> Result<Vec<CallTraceWithSource>> {
    let TransactionKind::ProgrammableTransaction(ptb) = transaction_data.kind() else {
        return Ok(vec![]);
    };
    if !ptb
        .commands
        .iter()
        .any(|command| matches!(command, SuiCommand::TransferObjects(_, _)))
    {
        return Ok(vec![]);
    }

    if let Some(archive_rpc_url) = &network.archive_rpc_url {
        let object_store = ArchiveRpcDataStore::new(
            network.node.clone(),
            archive_rpc_url,
            env!("CARGO_PKG_VERSION"),
        )
        .context("failed to create archive RPC object store for TransferObjects fallback")?;
        return fallback_transfer_objects_traces_with_store(&object_store, transaction_data, ptb);
    }

    let object_store = DataStore::new(network.node.clone(), env!("CARGO_PKG_VERSION"))
        .context("failed to create GraphQL object store for TransferObjects fallback")?;
    fallback_transfer_objects_traces_with_store(&object_store, transaction_data, ptb)
}

fn fallback_transfer_objects_traces_with_store(
    object_store: &dyn ObjectStore,
    transaction_data: &TransactionData,
    ptb: &ProgrammableTransaction,
) -> Result<Vec<CallTraceWithSource>> {
    ptb.commands
        .iter()
        .filter_map(|command| {
            if let SuiCommand::TransferObjects(objects, recipient) = command {
                Some((objects, recipient))
            } else {
                None
            }
        })
        .map(|(objects, recipient)| {
            let mut inputs = Vec::with_capacity(objects.len() + 1);
            for object_arg in objects {
                inputs.push(transfer_object_argument_to_json(
                    transaction_data,
                    ptb,
                    object_arg,
                    object_store,
                )?);
            }
            inputs.push(transfer_recipient_to_json(ptb, recipient).unwrap_or_else(unknown_value));
            Ok(trace_compat::transfer_objects_trace(inputs))
        })
        .collect()
}

fn transfer_object_argument_to_json(
    transaction_data: &TransactionData,
    ptb: &ProgrammableTransaction,
    argument: &Argument,
    object_store: &dyn ObjectStore,
) -> Result<Value> {
    let Some(key) = object_key_for_argument(transaction_data, ptb, argument) else {
        return Ok(unknown_value());
    };
    let objects = object_store
        .get_objects(&[key])
        .context("failed to load TransferObjects input object")?;
    let Some((object, _version)) = objects.into_iter().next().flatten() else {
        return Ok(unknown_value());
    };
    Ok(transfer_object_to_json(&object).unwrap_or_else(unknown_value))
}

fn object_key_for_argument(
    transaction_data: &TransactionData,
    ptb: &ProgrammableTransaction,
    argument: &Argument,
) -> Option<ObjectKey> {
    match argument {
        Argument::GasCoin => gas_object_key(transaction_data),
        Argument::Input(index) => ptb
            .inputs
            .get(*index as usize)
            .and_then(object_key_for_call_arg),
        Argument::Result(_) | Argument::NestedResult(_, _) => None,
    }
}

fn object_key_for_call_arg(call_arg: &CallArg) -> Option<ObjectKey> {
    let CallArg::Object(object_arg) = call_arg else {
        return None;
    };
    Some(match object_arg {
        ObjectArg::ImmOrOwnedObject((id, version, _digest))
        | ObjectArg::Receiving((id, version, _digest)) => ObjectKey {
            object_id: *id,
            version_query: VersionQuery::Version(version.value()),
        },
        ObjectArg::SharedObject {
            id,
            initial_shared_version,
            ..
        } => ObjectKey {
            object_id: *id,
            version_query: VersionQuery::Version(initial_shared_version.value()),
        },
    })
}

fn gas_object_key(transaction_data: &TransactionData) -> Option<ObjectKey> {
    let (id, version, _digest) = transaction_data.gas_data().payment.first()?;
    Some(ObjectKey {
        object_id: *id,
        version_query: VersionQuery::Version(version.value()),
    })
}

fn transfer_object_to_json(object: &Object) -> Option<Value> {
    let struct_tag = object.struct_tag()?;
    if !Coin::is_coin(&struct_tag) {
        return None;
    }
    let coin = object.as_coin_maybe()?;
    let coin_id: AccountAddress = object.id().into();
    Some(json!({
        "type": struct_tag,
        "fields": {
            "id": coin_id,
            "balance": coin.value().to_string(),
        }
    }))
}

fn unknown_value() -> Value {
    json!("?")
}

fn replay_child_error(output: &Output) -> String {
    let mut message = format!("replay child exited with {}", output.status);
    if !output.stderr.is_empty() {
        message.push_str("\nstderr:\n");
        message.push_str(&tail_lossy_utf8(&output.stderr, CHILD_STDIO_TAIL_BYTES));
    }
    if !output.stdout.is_empty() {
        message.push_str("\nstdout:\n");
        message.push_str(&tail_lossy_utf8(&output.stdout, CHILD_STDIO_TAIL_BYTES));
    }
    message
}

fn tail_lossy_utf8(bytes: &[u8], max_bytes: usize) -> String {
    let start = bytes.len().saturating_sub(max_bytes);
    let prefix = if start == 0 { "" } else { "..." };
    format!("{prefix}{}", String::from_utf8_lossy(&bytes[start..]))
}

fn node_from_legacy_config(chain_id: &str, endpoint: &str) -> Result<Node> {
    let endpoint = normalized_endpoint(endpoint);
    match endpoint {
        "mainnet" | MAINNET_RPC_URL | MAINNET_GQL_URL => Ok(Node::Mainnet),
        "testnet" | TESTNET_RPC_URL | TESTNET_GQL_URL => Ok(Node::Testnet),
        other if looks_like_graphql_endpoint(other) => {
            Node::from_str(other).map_err(anyhow::Error::msg)
        }
        _ if is_mainnet_chain_id(chain_id) => Ok(Node::Mainnet),
        _ if is_testnet_chain_id(chain_id) => Ok(Node::Testnet),
        other => Node::from_str(other).map_err(anyhow::Error::msg),
    }
}

fn archive_rpc_url_from_legacy_config(chain_id: &str, endpoint: &str) -> Option<String> {
    let endpoint = normalized_endpoint(endpoint);
    match endpoint {
        "mainnet" | MAINNET_RPC_URL => return Some(MAINNET_RPC_URL.to_owned()),
        "testnet" | TESTNET_RPC_URL => return Some(TESTNET_RPC_URL.to_owned()),
        MAINNET_GQL_URL | TESTNET_GQL_URL => return None,
        _ if looks_like_graphql_endpoint(endpoint) => return None,
        _ => {}
    }
    if is_mainnet_chain_id(chain_id) || is_testnet_chain_id(chain_id) {
        return Some(endpoint.to_owned());
    }
    None
}

fn normalized_endpoint(endpoint: &str) -> &str {
    endpoint
        .strip_prefix("graphql:")
        .or_else(|| endpoint.strip_prefix("gql:"))
        .unwrap_or(endpoint)
}

fn looks_like_graphql_endpoint(endpoint: &str) -> bool {
    endpoint.contains("/graphql")
}

// Sentio addresses sui networks by numeric id (1001 = mainnet, 1002 = testnet),
// and the trace server is invoked with that numeric id in the request path. Treat
// those ids as their chains so a custom (archive) endpoint resolves to the correct
// Node/Chain instead of falling through to the GraphQL default.
fn is_mainnet_chain_id(chain_id: &str) -> bool {
    chain_id == "1001"
        || chain_id.eq_ignore_ascii_case("mainnet")
        || chain_id.to_ascii_lowercase().contains("mainnet")
}

fn is_testnet_chain_id(chain_id: &str) -> bool {
    chain_id == "1002"
        || chain_id.eq_ignore_ascii_case("testnet")
        || chain_id.to_ascii_lowercase().contains("testnet")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body;
    use serde_json::json;
    use std::collections::HashMap;
    use sui_types::{
        base_types::{ObjectDigest, ObjectID, SequenceNumber, TransactionDigest},
        gas_coin::GAS,
        object::{MoveObject, Owner},
    };

    struct FakeObjectStore {
        objects: HashMap<ObjectID, Object>,
    }

    impl ObjectStore for FakeObjectStore {
        fn get_objects(&self, keys: &[ObjectKey]) -> anyhow::Result<Vec<Option<(Object, u64)>>> {
            Ok(keys
                .iter()
                .map(|key| {
                    self.objects.get(&key.object_id).map(|object| {
                        let version = match key.version_query {
                            VersionQuery::Version(version)
                            | VersionQuery::RootVersion(version)
                            | VersionQuery::AtCheckpoint(version) => version,
                        };
                        (object.clone(), version)
                    })
                })
                .collect())
        }
    }

    #[test]
    fn known_legacy_fullnode_urls_map_to_network_nodes() {
        assert!(matches!(
            node_from_legacy_config("sui_mainnet", MAINNET_RPC_URL).unwrap(),
            Node::Mainnet
        ));
        assert_eq!(
            archive_rpc_url_from_legacy_config("sui_mainnet", MAINNET_RPC_URL).as_deref(),
            Some(MAINNET_RPC_URL)
        );
        assert!(matches!(
            node_from_legacy_config("sui_testnet", TESTNET_RPC_URL).unwrap(),
            Node::Testnet
        ));
        assert_eq!(
            archive_rpc_url_from_legacy_config("sui_testnet", TESTNET_RPC_URL).as_deref(),
            Some(TESTNET_RPC_URL)
        );
    }

    #[test]
    fn mainnet_chain_id_with_custom_rpc_uses_mainnet_node() {
        assert!(matches!(
            node_from_legacy_config("sui_mainnet", "https://archive.example.com:443").unwrap(),
            Node::Mainnet
        ));
        assert_eq!(
            archive_rpc_url_from_legacy_config("sui_mainnet", "https://archive.example.com:443")
                .as_deref(),
            Some("https://archive.example.com:443")
        );
    }

    #[test]
    fn sentio_numeric_chain_ids_with_custom_rpc_use_network_nodes() {
        // 1001/1002 are Sentio's numeric ids for sui mainnet/testnet; with a custom
        // archive endpoint they must resolve to the network node and use that endpoint
        // as the archive RPC (not fall through to the GraphQL default).
        assert!(matches!(
            node_from_legacy_config("1001", "https://archive.example.com:443").unwrap(),
            Node::Mainnet
        ));
        assert_eq!(
            archive_rpc_url_from_legacy_config("1001", "https://archive.example.com:443").as_deref(),
            Some("https://archive.example.com:443")
        );
        assert!(matches!(
            node_from_legacy_config("1002", "https://archive.example.com:443").unwrap(),
            Node::Testnet
        ));
        assert_eq!(
            archive_rpc_url_from_legacy_config("1002", "https://archive.example.com:443").as_deref(),
            Some("https://archive.example.com:443")
        );
    }

    #[test]
    fn custom_graphql_endpoint_is_preserved() {
        match node_from_legacy_config("sui_mainnet", "https://example.com/graphql").unwrap() {
            Node::Custom(url) => assert_eq!(url, "https://example.com/graphql"),
            other => panic!("expected custom GraphQL endpoint, got {other:?}"),
        }
        assert_eq!(
            archive_rpc_url_from_legacy_config("sui_mainnet", "https://example.com/graphql"),
            None
        );
        match node_from_legacy_config("custom", "gql:https://example.com/graphql").unwrap() {
            Node::Custom(url) => assert_eq!(url, "https://example.com/graphql"),
            other => panic!("expected custom GraphQL endpoint, got {other:?}"),
        }
    }

    #[test]
    fn app_config_parses_comma_separated_networks() {
        let config = AppConfig::new(format!(
            "sui_mainnet={MAINNET_RPC_URL},sui_testnet={TESTNET_RPC_URL}"
        ))
        .unwrap();
        assert!(matches!(
            config.networks.get("sui_mainnet").unwrap().node,
            Node::Mainnet
        ));
        assert!(matches!(
            config.networks.get("sui_testnet").unwrap().node,
            Node::Testnet
        ));
    }

    #[test]
    fn app_config_rejects_malformed_entries() {
        assert!(AppConfig::new("sui_mainnet".to_string()).is_err());
    }

    #[test]
    fn tail_lossy_utf8_truncates_long_child_output() {
        assert_eq!(tail_lossy_utf8(b"short", 16), "short");
        assert_eq!(tail_lossy_utf8(b"0123456789", 4), "...6789");
    }

    #[test]
    fn transfer_recipient_input_pure_address_matches_old_address_json() {
        let recipient = SuiAddress::from(AccountAddress::from_hex_literal("0x42").unwrap());
        let ptb = ProgrammableTransaction {
            inputs: vec![CallArg::Pure(bcs::to_bytes(&recipient).unwrap())],
            commands: vec![],
        };

        assert_eq!(
            transfer_recipient_to_json(&ptb, &Argument::Input(0)).unwrap(),
            serde_json::to_value(AccountAddress::from(recipient)).unwrap()
        );
    }

    #[test]
    fn transfer_recipient_unsupported_argument_is_preserved_as_missing_slot() {
        let transaction_data = TransactionData::new_programmable(
            SuiAddress::ZERO,
            vec![],
            ProgrammableTransaction {
                inputs: vec![],
                commands: vec![SuiCommand::TransferObjects(vec![], Argument::Result(0))],
            },
            0,
            0,
        );

        assert_eq!(
            transfer_recipients_from_transaction_data(&transaction_data),
            vec![None]
        );
    }

    #[test]
    fn transfer_object_coin_json_matches_old_trace_v2_shape() {
        let object_id = ObjectID::from_hex_literal("0x77").unwrap();
        let object = Object::new_move(
            MoveObject::new_coin(
                GAS::type_tag(),
                SequenceNumber::from_u64(7),
                object_id,
                1234,
            ),
            Owner::AddressOwner(SuiAddress::ZERO),
            TransactionDigest::genesis_marker(),
        );

        assert_eq!(
            transfer_object_to_json(&object).unwrap(),
            json!({
                "type": {
                    "address": "0000000000000000000000000000000000000000000000000000000000000002",
                    "module": "coin",
                    "name": "Coin",
                    "type_args": [{
                        "struct": {
                            "address": "0000000000000000000000000000000000000000000000000000000000000002",
                            "module": "sui",
                            "name": "SUI",
                            "type_args": []
                        }
                    }]
                },
                "fields": {
                    "id": "0000000000000000000000000000000000000000000000000000000000000077",
                    "balance": "1234"
                }
            })
        );
    }

    #[test]
    fn transfer_object_argument_loads_direct_input_coin() {
        let object_id = ObjectID::from_hex_literal("0x88").unwrap();
        let version = SequenceNumber::from_u64(9);
        let object = Object::new_move(
            MoveObject::new_coin(GAS::type_tag(), version, object_id, 555),
            Owner::AddressOwner(SuiAddress::ZERO),
            TransactionDigest::genesis_marker(),
        );
        let store = FakeObjectStore {
            objects: HashMap::from([(object_id, object)]),
        };
        let ptb = ProgrammableTransaction {
            inputs: vec![CallArg::Object(ObjectArg::ImmOrOwnedObject((
                object_id,
                version,
                ObjectDigest::MIN,
            )))],
            commands: vec![],
        };
        let transaction_data =
            TransactionData::new_programmable(SuiAddress::ZERO, vec![], ptb.clone(), 0, 0);

        let value =
            transfer_object_argument_to_json(&transaction_data, &ptb, &Argument::Input(0), &store)
                .unwrap();

        assert_eq!(value["fields"]["balance"], json!("555"));
        assert_eq!(
            value["fields"]["id"],
            json!("0000000000000000000000000000000000000000000000000000000000000088")
        );
    }

    #[test]
    fn transfer_object_argument_result_falls_back_to_unknown_value() {
        let store = FakeObjectStore {
            objects: HashMap::new(),
        };
        let ptb = ProgrammableTransaction {
            inputs: vec![],
            commands: vec![],
        };
        let transaction_data =
            TransactionData::new_programmable(SuiAddress::ZERO, vec![], ptb.clone(), 0, 0);

        assert_eq!(
            transfer_object_argument_to_json(&transaction_data, &ptb, &Argument::Result(0), &store)
                .unwrap(),
            json!("?")
        );
    }

    #[tokio::test]
    async fn call_trace_unknown_network_returns_old_error_shape() {
        let config = AppConfig::new(format!("sui_mainnet={MAINNET_RPC_URL}")).unwrap();
        let err = call_trace(
            Path(("missing".to_string(), "tx".to_string())),
            State(config),
        )
        .await
        .unwrap_err();

        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let body = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value, json!({ "error": "Network missing not found" }));
    }
}

enum MyError {
    SomethingWentWrong { message: String },
}

impl MyError {
    fn new(message: String) -> Self {
        Self::SomethingWentWrong { message }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
struct ErrorResponse {
    error: String,
}

impl IntoResponse for MyError {
    fn into_response(self) -> Response {
        let body = match self {
            MyError::SomethingWentWrong { message } => Json(ErrorResponse { error: message }),
        };
        (StatusCode::INTERNAL_SERVER_ERROR, body).into_response()
    }
}
