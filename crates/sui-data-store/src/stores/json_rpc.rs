// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Hybrid archive store for replay.
//!
//! Transaction and epoch data stay on the existing GraphQL store. Exact-version
//! and root-version object reads use JSON-RPC so archive/full-history endpoints
//! preserve the same object lookup semantics as the legacy replay path.

use crate::{
    EpochData, EpochStore, ObjectKey, ObjectStore, SetupStore, StoreSummary, TransactionInfo,
    TransactionStore, VersionQuery, node::Node, stores::DataStore,
};
use anyhow::{Error, Result, anyhow};
use std::{env, io::Write, time::Duration};
use sui_json_rpc_types::{
    SuiGetPastObjectRequest, SuiObjectData, SuiObjectDataOptions, SuiPastObjectResponse,
};
use sui_sdk::{SuiClient, SuiClientBuilder};
use sui_types::{base_types::SequenceNumber, object::Object};

const JSON_RPC_OBJECT_CHUNK_SIZE: usize = 50;

macro_rules! block_on {
    ($expr:expr) => {{
        #[allow(clippy::disallowed_methods, clippy::result_large_err)]
        {
            if tokio::runtime::Handle::try_current().is_ok() {
                std::thread::scope(|scope| {
                    scope
                        .spawn(|| {
                            let rt = tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build()
                                .expect("failed to build Tokio runtime");
                            rt.block_on($expr)
                        })
                        .join()
                        .expect("failed to join scoped thread running nested runtime")
                })
            } else {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to build Tokio runtime");
                rt.block_on($expr)
            }
        }
    }};
}

fn env_u64(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

async fn retry_rpc_query(attempt: u64, retries: u64, backoff_ms: u64) {
    if attempt < retries {
        tokio::time::sleep(Duration::from_millis(
            backoff_ms.saturating_mul(attempt + 1),
        ))
        .await;
    }
}

/// A replay data store that mirrors legacy archive JSON-RPC object lookups.
pub struct ArchiveDataStore {
    gql: DataStore,
    rpc_client: SuiClient,
}

impl ArchiveDataStore {
    pub fn new(gql_node: Node, rpc_url: &str, version: &str) -> Result<Self, Error> {
        let gql = DataStore::new(gql_node, version)?;
        let timeout = Duration::from_secs(env_u64("SUI_DATA_STORE_RPC_TIMEOUT_SECS", 60));
        let max_concurrent_requests = env_u64("SUI_DATA_STORE_RPC_MAX_CONCURRENT_REQUESTS", 256)
            .try_into()
            .unwrap_or(256);
        let rpc_client = block_on!(
            SuiClientBuilder::default()
                .request_timeout(timeout)
                .max_concurrent_requests(max_concurrent_requests)
                .build(rpc_url)
        )?;
        Ok(Self { gql, rpc_client })
    }

    async fn version_objects(
        &self,
        indexed: &[(usize, ObjectKey)],
    ) -> Result<Vec<(usize, Option<(Object, u64)>)>, Error> {
        let options = SuiObjectDataOptions::bcs_lossless();
        let mut results = Vec::with_capacity(indexed.len());

        for chunk in indexed.chunks(JSON_RPC_OBJECT_CHUNK_SIZE) {
            let requests = chunk
                .iter()
                .map(|(_, key)| {
                    let VersionQuery::Version(version) = key.version_query else {
                        unreachable!("version_objects only accepts Version queries");
                    };
                    SuiGetPastObjectRequest {
                        object_id: key.object_id,
                        version: SequenceNumber::from_u64(version),
                    }
                })
                .collect::<Vec<_>>();
            let retries = env_u64("SUI_DATA_STORE_RPC_RETRIES", 4);
            let backoff_ms = env_u64("SUI_DATA_STORE_RPC_RETRY_BACKOFF_MS", 250);
            let mut responses = None;
            let mut last_error = None;
            for attempt in 0..=retries {
                match self
                    .rpc_client
                    .read_api()
                    .try_multi_get_parsed_past_object(requests.clone(), options.clone())
                    .await
                {
                    Ok(value) => {
                        responses = Some(value);
                        break;
                    }
                    Err(err) => {
                        last_error = Some(err.to_string());
                        retry_rpc_query(attempt, retries, backoff_ms).await;
                    }
                }
            }
            let responses = responses.ok_or_else(|| {
                anyhow!(
                    "failed to fetch versioned objects from archive JSON-RPC: {}",
                    last_error.unwrap_or_else(|| "unknown error".to_string())
                )
            })?;
            for ((index, _), response) in chunk.iter().zip(responses.into_iter()) {
                results.push((*index, past_object_response(response)?));
            }
        }

        Ok(results)
    }

    async fn root_version_object(&self, key: &ObjectKey) -> Result<Option<(Object, u64)>, Error> {
        let VersionQuery::RootVersion(version) = key.version_query else {
            unreachable!("root_version_object only accepts RootVersion queries");
        };
        let retries = env_u64("SUI_DATA_STORE_RPC_RETRIES", 4);
        let backoff_ms = env_u64("SUI_DATA_STORE_RPC_RETRY_BACKOFF_MS", 250);
        let mut response = None;
        let mut last_error = None;
        for attempt in 0..=retries {
            match self
                .rpc_client
                .read_api()
                .try_get_object_before_version(key.object_id, SequenceNumber::from_u64(version))
                .await
            {
                Ok(value) => {
                    response = Some(value);
                    break;
                }
                Err(err) => {
                    last_error = Some(err.to_string());
                    retry_rpc_query(attempt, retries, backoff_ms).await;
                }
            }
        }
        let response = response.ok_or_else(|| {
            anyhow!(
                "failed to fetch root-version object {} <= {} from archive JSON-RPC: {}",
                key.object_id,
                version,
                last_error.unwrap_or_else(|| "unknown error".to_string())
            )
        })?;
        past_object_response(response)
    }
}

impl TransactionStore for ArchiveDataStore {
    fn transaction_data_and_effects(
        &self,
        tx_digest: &str,
    ) -> Result<Option<TransactionInfo>, Error> {
        self.gql.transaction_data_and_effects(tx_digest)
    }
}

impl EpochStore for ArchiveDataStore {
    fn epoch_info(&self, epoch: u64) -> Result<Option<EpochData>, Error> {
        self.gql.epoch_info(epoch)
    }

    fn protocol_config(
        &self,
        epoch: u64,
    ) -> Result<Option<sui_types::supported_protocol_versions::ProtocolConfig>, Error> {
        self.gql.protocol_config(epoch)
    }
}

impl ObjectStore for ArchiveDataStore {
    fn get_objects(&self, keys: &[ObjectKey]) -> Result<Vec<Option<(Object, u64)>>, Error> {
        let mut results = vec![None; keys.len()];
        let mut version_keys = Vec::new();
        let mut gql_indices = Vec::new();
        let mut gql_keys = Vec::new();

        for (index, key) in keys.iter().cloned().enumerate() {
            match key.version_query {
                VersionQuery::Version(_) => version_keys.push((index, key)),
                VersionQuery::RootVersion(_) => {
                    results[index] = block_on!(self.root_version_object(&key))?;
                }
                VersionQuery::AtCheckpoint(_) => {
                    gql_indices.push(index);
                    gql_keys.push(key);
                }
            }
        }

        for (index, object) in block_on!(self.version_objects(&version_keys))? {
            results[index] = object;
        }

        if !gql_keys.is_empty() {
            let gql_results = self.gql.get_objects(&gql_keys)?;
            for (index, object) in gql_indices.into_iter().zip(gql_results.into_iter()) {
                results[index] = object;
            }
        }

        Ok(results)
    }
}

impl SetupStore for ArchiveDataStore {
    fn setup(&self, chain_id: Option<String>) -> Result<Option<String>, Error> {
        self.gql.setup(chain_id)
    }
}

impl StoreSummary for ArchiveDataStore {
    fn summary<W: Write>(&self, writer: &mut W) -> Result<()> {
        writeln!(writer, "ArchiveDataStore (GraphQL + archive JSON-RPC)")?;
        writeln!(writer, "  JSON-RPC: <configured>")?;
        self.gql.summary(writer)
    }
}

fn past_object_response(response: SuiPastObjectResponse) -> Result<Option<(Object, u64)>, Error> {
    match response {
        SuiPastObjectResponse::VersionFound(data) => {
            let object = object_from_sui_data(&data)?;
            let version = object.version().value();
            Ok(Some((object, version)))
        }
        SuiPastObjectResponse::ObjectDeleted(_) | SuiPastObjectResponse::ObjectNotExists(_) => {
            Ok(None)
        }
        SuiPastObjectResponse::VersionNotFound(_, _) => Ok(None),
        SuiPastObjectResponse::VersionTooHigh { .. } => Ok(None),
    }
}

fn object_from_sui_data(data: &SuiObjectData) -> Result<Object, Error> {
    TryInto::<Object>::try_into(data.clone()).map_err(|err| anyhow!("{err}"))
}
