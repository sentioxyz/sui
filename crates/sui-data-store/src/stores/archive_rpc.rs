// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Archive JSON-RPC only data store for replay.
//!
//! This store intentionally does not depend on GraphQL. It mirrors the legacy
//! replay fetcher semantics: exact versions use past-object RPCs, child objects
//! use upper-bound version RPCs, non-system package lookups use latest objects,
//! and system package versions are derived from epoch-change transactions.

use crate::{
    EpochData, EpochStore, ObjectKey, ObjectStore, SetupStore, StoreSummary, TransactionInfo,
    TransactionStore, VersionQuery, node::Node,
};
use anyhow::{Context, Error, Result, anyhow, bail};
use move_core_types::language_storage::StructTag;
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    io::Write,
    str::FromStr,
    sync::RwLock,
    time::{Duration, Instant},
};
use sui_framework::BuiltInFramework;
use sui_json_rpc_types::{
    EventFilter, SuiEvent, SuiGetPastObjectRequest, SuiObjectData, SuiObjectDataOptions,
    SuiObjectResponse, SuiPastObjectResponse, SuiTransactionBlockResponse,
    SuiTransactionBlockResponseOptions,
};
use sui_sdk::{SuiClient, SuiClientBuilder};
use sui_types::{
    DEEPBOOK_PACKAGE_ID,
    base_types::{ObjectID, SequenceNumber},
    committee::ProtocolVersion,
    digests::TransactionDigest,
    effects::{TransactionEffects, TransactionEffectsAPI},
    object::Object,
    supported_protocol_versions::ProtocolConfig,
    transaction::{
        DEFAULT_VALIDATOR_GAS_PRICE, EndOfEpochTransactionKind, SenderSignedData,
        TransactionDataAPI, TransactionKind,
    },
};
use tracing::debug;

const JSON_RPC_OBJECT_CHUNK_SIZE: usize = 50;
const EPOCH_CHANGE_STRUCT_TAG: &str = "0x3::sui_system_state_inner::SystemEpochInfoEvent";

type SystemPackageTable = BTreeMap<u64, BTreeMap<ObjectID, SequenceNumber>>;

#[derive(Clone, Copy)]
struct SystemPackageRevision {
    version: SequenceNumber,
    previous_transaction: TransactionDigest,
}

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

/// Replay data store backed only by archive JSON-RPC.
pub struct ArchiveRpcDataStore {
    node: Node,
    rpc_client: SuiClient,
    epoch_map: RwLock<BTreeMap<u64, EpochData>>,
    epoch_change_event_map: RwLock<BTreeMap<u64, SuiEvent>>,
    checkpoint_protocol_map: RwLock<BTreeMap<u64, u64>>,
    system_package_table: RwLock<SystemPackageTable>,
    system_package_revision_map: RwLock<BTreeMap<ObjectID, Vec<SystemPackageRevision>>>,
    package_tx_protocol_map: RwLock<BTreeMap<TransactionDigest, u64>>,
    genesis_tx: RwLock<Option<TransactionDigest>>,
    latest_protocol_version: RwLock<Option<u64>>,
}

impl ArchiveRpcDataStore {
    pub fn new(node: Node, rpc_url: &str, _version: &str) -> Result<Self, Error> {
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
        Ok(Self {
            node,
            rpc_client,
            epoch_map: RwLock::new(BTreeMap::new()),
            epoch_change_event_map: RwLock::new(BTreeMap::new()),
            checkpoint_protocol_map: RwLock::new(BTreeMap::new()),
            system_package_table: RwLock::new(BTreeMap::new()),
            system_package_revision_map: RwLock::new(BTreeMap::new()),
            package_tx_protocol_map: RwLock::new(BTreeMap::new()),
            genesis_tx: RwLock::new(None),
            latest_protocol_version: RwLock::new(None),
        })
    }

    async fn transaction_response(
        &self,
        digest: TransactionDigest,
        options: SuiTransactionBlockResponseOptions,
    ) -> Result<SuiTransactionBlockResponse, Error> {
        let retries = env_u64("SUI_DATA_STORE_RPC_RETRIES", 4);
        let backoff_ms = env_u64("SUI_DATA_STORE_RPC_RETRY_BACKOFF_MS", 250);
        let mut response = None;
        let mut last_error = None;
        for attempt in 0..=retries {
            match self
                .rpc_client
                .read_api()
                .get_transaction_with_options(digest, options.clone())
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
        response.ok_or_else(|| {
            anyhow!(
                "failed to fetch transaction {} from archive JSON-RPC: {}",
                digest,
                last_error.unwrap_or_else(|| "unknown error".to_string())
            )
        })
    }

    async fn checkpoint(&self, checkpoint: u64) -> Result<sui_json_rpc_types::Checkpoint, Error> {
        let retries = env_u64("SUI_DATA_STORE_RPC_RETRIES", 4);
        let backoff_ms = env_u64("SUI_DATA_STORE_RPC_RETRY_BACKOFF_MS", 250);
        let mut response = None;
        let mut last_error = None;
        for attempt in 0..=retries {
            match self
                .rpc_client
                .read_api()
                .get_checkpoint(checkpoint.into())
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
        response.ok_or_else(|| {
            anyhow!(
                "failed to fetch checkpoint {} from archive JSON-RPC: {}",
                checkpoint,
                last_error.unwrap_or_else(|| "unknown error".to_string())
            )
        })
    }

    async fn version_objects(
        &self,
        indexed: &[(usize, ObjectKey)],
    ) -> Result<Vec<(usize, Option<(Object, u64)>)>, Error> {
        let requests = indexed
            .iter()
            .map(|(index, key)| {
                let VersionQuery::Version(version) = key.version_query else {
                    unreachable!("version_objects only accepts Version queries");
                };
                (
                    *index,
                    SuiGetPastObjectRequest {
                        object_id: key.object_id,
                        version: SequenceNumber::from_u64(version),
                    },
                )
            })
            .collect::<Vec<_>>();
        let objects = self.version_objects_by_request(&requests).await?;
        Ok(requests
            .into_iter()
            .zip(objects.into_iter())
            .map(|((index, _), object)| (index, object))
            .collect())
    }

    async fn version_objects_by_request(
        &self,
        indexed: &[(usize, SuiGetPastObjectRequest)],
    ) -> Result<Vec<Option<(Object, u64)>>, Error> {
        let options = SuiObjectDataOptions::bcs_lossless();
        let mut results = Vec::with_capacity(indexed.len());

        for chunk in indexed.chunks(JSON_RPC_OBJECT_CHUNK_SIZE) {
            let requests = chunk
                .iter()
                .map(|(_, request)| request.clone())
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
            for response in responses.into_iter() {
                results.push(past_object_response(response)?);
            }
        }

        Ok(results)
    }

    async fn version_object_revisions_by_request(
        &self,
        indexed: &[(usize, SuiGetPastObjectRequest)],
    ) -> Result<Vec<Option<SystemPackageRevision>>, Error> {
        let options = SuiObjectDataOptions::new().with_previous_transaction();
        let mut results = Vec::with_capacity(indexed.len());

        for chunk in indexed.chunks(JSON_RPC_OBJECT_CHUNK_SIZE) {
            let start = Instant::now();
            let requests = chunk
                .iter()
                .map(|(_, request)| request.clone())
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
                    "failed to fetch system package revisions from archive JSON-RPC: {}",
                    last_error.unwrap_or_else(|| "unknown error".to_string())
                )
            })?;
            for response in responses.into_iter() {
                results.push(past_object_revision_response(response)?);
            }
            debug!(
                op = "archive_rpc_system_package_revisions",
                count = chunk.len(),
                elapsed_ms = start.elapsed().as_millis(),
                "fetched system package revision chunk"
            );
        }

        Ok(results)
    }

    async fn version_object(
        &self,
        object_id: ObjectID,
        version: SequenceNumber,
    ) -> Result<Option<(Object, u64)>, Error> {
        let mut objects = self
            .version_objects_by_request(&[(0, SuiGetPastObjectRequest { object_id, version })])
            .await?;
        Ok(objects.pop().unwrap_or(None))
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

    async fn latest_objects(
        &self,
        indexed: &[(usize, ObjectID)],
    ) -> Result<Vec<(usize, Option<(Object, u64)>)>, Error> {
        let options = SuiObjectDataOptions::bcs_lossless();
        let mut results = Vec::with_capacity(indexed.len());

        for chunk in indexed.chunks(JSON_RPC_OBJECT_CHUNK_SIZE) {
            let object_ids = chunk
                .iter()
                .map(|(_, object_id)| *object_id)
                .collect::<Vec<_>>();
            let retries = env_u64("SUI_DATA_STORE_RPC_RETRIES", 4);
            let backoff_ms = env_u64("SUI_DATA_STORE_RPC_RETRY_BACKOFF_MS", 250);
            let mut responses = None;
            let mut last_error = None;
            for attempt in 0..=retries {
                match self
                    .rpc_client
                    .read_api()
                    .multi_get_object_with_options(object_ids.clone(), options.clone())
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
                    "failed to fetch latest objects from archive JSON-RPC: {}",
                    last_error.unwrap_or_else(|| "unknown error".to_string())
                )
            })?;
            for ((index, _), response) in chunk.iter().zip(responses.into_iter()) {
                results.push((*index, object_response(response)?));
            }
        }

        Ok(results)
    }

    async fn latest_object_revisions(
        &self,
        indexed: &[(usize, ObjectID)],
    ) -> Result<Vec<(usize, Option<SystemPackageRevision>)>, Error> {
        let options = SuiObjectDataOptions::new().with_previous_transaction();
        let mut results = Vec::with_capacity(indexed.len());

        for chunk in indexed.chunks(JSON_RPC_OBJECT_CHUNK_SIZE) {
            let object_ids = chunk
                .iter()
                .map(|(_, object_id)| *object_id)
                .collect::<Vec<_>>();
            let retries = env_u64("SUI_DATA_STORE_RPC_RETRIES", 4);
            let backoff_ms = env_u64("SUI_DATA_STORE_RPC_RETRY_BACKOFF_MS", 250);
            let mut responses = None;
            let mut last_error = None;
            for attempt in 0..=retries {
                match self
                    .rpc_client
                    .read_api()
                    .multi_get_object_with_options(object_ids.clone(), options.clone())
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
                    "failed to fetch latest system package revisions from archive JSON-RPC: {}",
                    last_error.unwrap_or_else(|| "unknown error".to_string())
                )
            })?;
            for ((index, _), response) in chunk.iter().zip(responses.into_iter()) {
                results.push((*index, object_revision_response(response)?));
            }
        }

        Ok(results)
    }

    async fn checkpoint_object(&self, key: &ObjectKey) -> Result<Option<(Object, u64)>, Error> {
        let start = Instant::now();
        let VersionQuery::AtCheckpoint(checkpoint) = key.version_query else {
            unreachable!("checkpoint_object only accepts AtCheckpoint queries");
        };
        let protocol_version = self.protocol_version_for_checkpoint(checkpoint).await?;
        debug!(
            op = "archive_rpc_checkpoint_object",
            object_id = %key.object_id,
            checkpoint,
            protocol_version,
            elapsed_ms = start.elapsed().as_millis(),
            "resolved checkpoint protocol"
        );
        if system_package_ids(protocol_version).contains(&key.object_id) {
            if protocol_version >= self.latest_protocol_version().await? {
                let mut objects = self.latest_objects(&[(0, key.object_id)]).await?;
                return Ok(objects.pop().and_then(|(_, object)| object));
            }
            let version = self
                .system_package_version(protocol_version, &key.object_id)
                .await?
                .with_context(|| {
                    format!(
                        "system package {} version missing for protocol {}",
                        key.object_id, protocol_version
                    )
                })?;
            return self.version_object(key.object_id, version).await;
        }

        let mut objects = self.latest_objects(&[(0, key.object_id)]).await?;
        Ok(objects.pop().and_then(|(_, object)| object))
    }

    async fn protocol_version_for_checkpoint(&self, checkpoint: u64) -> Result<u64, Error> {
        if let Some(protocol_version) = self
            .checkpoint_protocol_map
            .read()
            .unwrap()
            .get(&checkpoint)
            .copied()
        {
            return Ok(protocol_version);
        }

        if checkpoint == 0 {
            self.checkpoint_protocol_map
                .write()
                .unwrap()
                .insert(checkpoint, 1);
            return Ok(1);
        }
        let checkpoint_data = self.checkpoint(checkpoint).await?;
        let epoch = checkpoint_data.epoch;
        let protocol_version = self
            .epoch_data(epoch)
            .await?
            .with_context(|| format!("epoch {} not found", epoch))?
            .protocol_version;
        self.checkpoint_protocol_map
            .write()
            .unwrap()
            .insert(checkpoint, protocol_version);
        Ok(protocol_version)
    }

    async fn epoch_data(&self, epoch: u64) -> Result<Option<EpochData>, Error> {
        if let Some(epoch_data) = self.epoch_map.read().unwrap().get(&epoch) {
            return Ok(Some(epoch_data.clone()));
        }

        let epoch_data = self.fetch_epoch_data(epoch).await?;
        if let Some(epoch_data) = &epoch_data {
            self.epoch_map
                .write()
                .unwrap()
                .insert(epoch, epoch_data.clone());
        }
        Ok(epoch_data)
    }

    async fn fetch_epoch_data(&self, epoch: u64) -> Result<Option<EpochData>, Error> {
        if epoch == 0 {
            let checkpoint = self.checkpoint(0).await?;
            return Ok(Some(EpochData {
                epoch_id: 0,
                protocol_version: 1,
                rgp: DEFAULT_VALIDATOR_GAS_PRICE,
                start_timestamp: checkpoint.timestamp_ms,
            }));
        }

        let event = self.epoch_change_event(epoch).await?;
        let Some(event) = event else {
            return Ok(None);
        };
        let (_, protocol_version) = epoch_and_protocol_from_event(&event)?;
        let reference_gas_price = json_u64(&event.parsed_json, "reference_gas_price")?;
        let tx_response = self
            .transaction_response(
                event.id.tx_digest,
                SuiTransactionBlockResponseOptions::new().with_raw_input(),
            )
            .await?;
        let signed_data = sender_signed_data_from_response(&tx_response)?;
        let start_timestamp =
            epoch_start_timestamp_from_transaction(signed_data.transaction_data().kind())
                .with_context(|| format!("epoch change timestamp missing for epoch {}", epoch))?;

        Ok(Some(EpochData {
            epoch_id: epoch,
            protocol_version,
            rgp: reference_gas_price,
            start_timestamp,
        }))
    }

    async fn epoch_change_event(&self, epoch: u64) -> Result<Option<SuiEvent>, Error> {
        if let Some(event) = self.epoch_change_event_map.read().unwrap().get(&epoch) {
            return Ok(Some(event.clone()));
        }

        let struct_tag = StructTag::from_str(EPOCH_CHANGE_STRUCT_TAG)?;
        let filter = EventFilter::MoveEventType(struct_tag);
        let limit = env_u64("SUI_DATA_STORE_RPC_EVENT_PAGE_SIZE", 50)
            .try_into()
            .unwrap_or(50);
        let retries = env_u64("SUI_DATA_STORE_RPC_RETRIES", 4);
        let backoff_ms = env_u64("SUI_DATA_STORE_RPC_RETRY_BACKOFF_MS", 250);
        let mut cursor = None;
        let mut has_next_page = true;

        while has_next_page {
            let mut page = None;
            let mut last_error = None;
            for attempt in 0..=retries {
                match self
                    .rpc_client
                    .event_api()
                    .query_events(filter.clone(), cursor, Some(limit), true)
                    .await
                {
                    Ok(value) => {
                        page = Some(value);
                        break;
                    }
                    Err(err) => {
                        last_error = Some(err.to_string());
                        retry_rpc_query(attempt, retries, backoff_ms).await;
                    }
                }
            }
            let page = page.ok_or_else(|| {
                anyhow!(
                    "failed to query epoch change events from archive JSON-RPC: {}",
                    last_error.unwrap_or_else(|| "unknown error".to_string())
                )
            })?;
            for event in page.data {
                if let Ok((event_epoch, _)) = epoch_and_protocol_from_event(&event) {
                    self.epoch_change_event_map
                        .write()
                        .unwrap()
                        .insert(event_epoch, event.clone());
                    if event_epoch == epoch {
                        return Ok(Some(event));
                    }
                    if event_epoch < epoch {
                        return Ok(None);
                    }
                }
            }
            has_next_page = page.has_next_page;
            cursor = page.next_cursor;
        }

        Ok(None)
    }

    async fn epoch_change_event_for_transaction(
        &self,
        tx_digest: TransactionDigest,
    ) -> Result<Option<SuiEvent>, Error> {
        let filter = EventFilter::Transaction(tx_digest);
        let epoch_change_tag = StructTag::from_str(EPOCH_CHANGE_STRUCT_TAG)?;
        let retries = env_u64("SUI_DATA_STORE_RPC_RETRIES", 4);
        let backoff_ms = env_u64("SUI_DATA_STORE_RPC_RETRY_BACKOFF_MS", 250);
        let mut page = None;
        let mut last_error = None;
        for attempt in 0..=retries {
            match self
                .rpc_client
                .event_api()
                .query_events(filter.clone(), None, Some(1), true)
                .await
            {
                Ok(value) => {
                    page = Some(value);
                    break;
                }
                Err(err) => {
                    last_error = Some(err.to_string());
                    retry_rpc_query(attempt, retries, backoff_ms).await;
                }
            }
        }
        let page = page.ok_or_else(|| {
            anyhow!(
                "failed to query transaction events from archive JSON-RPC: {}",
                last_error.unwrap_or_else(|| "unknown error".to_string())
            )
        })?;
        if let Some(event) = page.data.into_iter().next() {
            if event.type_ == epoch_change_tag {
                if let Ok((event_epoch, _)) = epoch_and_protocol_from_event(&event) {
                    self.epoch_change_event_map
                        .write()
                        .unwrap()
                        .insert(event_epoch, event.clone());
                }
                return Ok(Some(event));
            }
        }

        let response = self
            .transaction_response(
                tx_digest,
                SuiTransactionBlockResponseOptions::new().with_events(),
            )
            .await?;
        if let Some(events) = response.events {
            for event in events.data {
                if event.type_ != epoch_change_tag {
                    continue;
                }
                if let Ok((event_epoch, _)) = epoch_and_protocol_from_event(&event) {
                    self.epoch_change_event_map
                        .write()
                        .unwrap()
                        .insert(event_epoch, event.clone());
                }
                return Ok(Some(event));
            }
        }

        Ok(None)
    }

    async fn latest_protocol_version(&self) -> Result<u64> {
        if let Some(protocol_version) = *self.latest_protocol_version.read().unwrap() {
            return Ok(protocol_version);
        }

        let struct_tag = StructTag::from_str(EPOCH_CHANGE_STRUCT_TAG)?;
        let retries = env_u64("SUI_DATA_STORE_RPC_RETRIES", 4);
        let backoff_ms = env_u64("SUI_DATA_STORE_RPC_RETRY_BACKOFF_MS", 250);
        let mut page = None;
        let mut last_error = None;
        for attempt in 0..=retries {
            match self
                .rpc_client
                .event_api()
                .query_events(
                    EventFilter::MoveEventType(struct_tag.clone()),
                    None,
                    Some(1),
                    true,
                )
                .await
            {
                Ok(value) => {
                    page = Some(value);
                    break;
                }
                Err(err) => {
                    last_error = Some(err.to_string());
                    retry_rpc_query(attempt, retries, backoff_ms).await;
                }
            }
        }
        let page = page.ok_or_else(|| {
            anyhow!(
                "failed to query latest epoch change event from archive JSON-RPC: {}",
                last_error.unwrap_or_else(|| "unknown error".to_string())
            )
        })?;
        let protocol_version = page
            .data
            .first()
            .map(|event| json_u64(&event.parsed_json, "protocol_version"))
            .transpose()?
            .context("latest epoch change event not found")?;
        self.latest_protocol_version
            .write()
            .unwrap()
            .replace(protocol_version);
        Ok(protocol_version)
    }

    async fn system_package_version(
        &self,
        protocol_version: u64,
        package_id: &ObjectID,
    ) -> Result<Option<SequenceNumber>, Error> {
        if let Some(version) = self
            .system_package_table
            .read()
            .unwrap()
            .get(&protocol_version)
            .and_then(|packages| packages.get(package_id))
            .copied()
        {
            return Ok(Some(version));
        }

        let version = self
            .resolve_system_package_version(protocol_version, package_id)
            .await?;
        if let Some(version) = version {
            self.system_package_table
                .write()
                .unwrap()
                .entry(protocol_version)
                .or_default()
                .insert(*package_id, version);
        }
        Ok(version)
    }

    async fn resolve_system_package_version(
        &self,
        protocol_version: u64,
        package_id: &ObjectID,
    ) -> Result<Option<SequenceNumber>, Error> {
        let start = Instant::now();
        let revisions = self.system_package_revision_history(package_id).await?;
        if revisions.is_empty() {
            return Ok(None);
        }

        let mut low = 0usize;
        let mut high = revisions.len();
        let mut candidate = None;
        let mut revision_protocols = vec![None; revisions.len()];

        while low < high {
            let mid = (low + high) / 2;
            let revision = revisions[mid];
            let revision_protocol = self
                .revision_protocol_for_search(&revisions, &mut revision_protocols, mid)
                .await?;
            let Some(revision_protocol) = revision_protocol else {
                high = mid;
                continue;
            };
            if revision_protocol <= protocol_version {
                candidate = Some(revision);
                low = mid + 1;
            } else {
                high = mid;
            }
        }

        let version = candidate.map(|revision| revision.version);
        debug!(
            op = "archive_rpc_resolve_system_package_version",
            package_id = %package_id,
            protocol_version,
            version = version.map(|version| version.value()),
            elapsed_ms = start.elapsed().as_millis(),
            "resolved system package version"
        );
        Ok(version)
    }

    async fn system_package_revision_history(
        &self,
        package_id: &ObjectID,
    ) -> Result<Vec<SystemPackageRevision>, Error> {
        if let Some(revisions) = self
            .system_package_revision_map
            .read()
            .unwrap()
            .get(package_id)
            .cloned()
        {
            return Ok(revisions);
        }

        let start = Instant::now();
        let latest = self
            .latest_object_revisions(&[(0, *package_id)])
            .await?
            .pop()
            .and_then(|(_, revision)| revision);
        let Some(latest) = latest else {
            return Ok(Vec::new());
        };

        let requests = (1..=latest.version.value())
            .map(|version| {
                (
                    0,
                    SuiGetPastObjectRequest {
                        object_id: *package_id,
                        version: SequenceNumber::from_u64(version),
                    },
                )
            })
            .collect::<Vec<_>>();

        let mut revisions = self
            .version_object_revisions_by_request(&requests)
            .await?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        revisions.sort_by_key(|revision| revision.version);
        debug!(
            op = "archive_rpc_system_package_revision_history",
            package_id = %package_id,
            count = revisions.len(),
            elapsed_ms = start.elapsed().as_millis(),
            "fetched system package revision history"
        );

        self.system_package_revision_map
            .write()
            .unwrap()
            .insert(*package_id, revisions.clone());
        Ok(revisions)
    }

    async fn revision_protocol_for_search(
        &self,
        revisions: &[SystemPackageRevision],
        revision_protocols: &mut [Option<Option<u64>>],
        index: usize,
    ) -> Result<Option<u64>, Error> {
        if let Some(protocol_version) = revision_protocols[index] {
            return Ok(protocol_version);
        }

        let protocol_version = self
            .try_protocol_version_for_package_tx(revisions[index].previous_transaction)
            .await?;
        if let Some(protocol_version) = protocol_version {
            revision_protocols[index] = Some(Some(protocol_version));
            return Ok(Some(protocol_version));
        }

        for prev_index in (0..index).rev() {
            if let Some(protocol_version) = revision_protocols[prev_index] {
                if let Some(protocol_version) = protocol_version {
                    revision_protocols[index] = Some(Some(protocol_version));
                    return Ok(Some(protocol_version));
                }
                continue;
            }

            let protocol_version = self
                .try_protocol_version_for_package_tx(revisions[prev_index].previous_transaction)
                .await?;
            revision_protocols[prev_index] = Some(protocol_version);
            if let Some(protocol_version) = protocol_version {
                revision_protocols[index] = Some(Some(protocol_version));
                return Ok(Some(protocol_version));
            }
        }

        revision_protocols[index] = Some(None);
        Ok(None)
    }

    async fn try_protocol_version_for_package_tx(
        &self,
        tx_digest: TransactionDigest,
    ) -> Result<Option<u64>, Error> {
        if let Some(protocol_version) = self
            .package_tx_protocol_map
            .read()
            .unwrap()
            .get(&tx_digest)
            .copied()
        {
            return Ok(Some(protocol_version));
        }

        let genesis_tx = self.genesis_tx().await?;
        if tx_digest == genesis_tx {
            self.package_tx_protocol_map
                .write()
                .unwrap()
                .insert(tx_digest, 1);
            return Ok(Some(1));
        }

        if let Some(event) = self.epoch_change_event_for_transaction(tx_digest).await? {
            let (_, protocol_version) = epoch_and_protocol_from_event(&event)?;
            self.package_tx_protocol_map
                .write()
                .unwrap()
                .insert(tx_digest, protocol_version);
            return Ok(Some(protocol_version));
        }

        let response = self
            .transaction_response(
                tx_digest,
                SuiTransactionBlockResponseOptions::new().with_raw_effects(),
            )
            .await?;
        let effects = effects_from_response(&response)?;
        let epoch = effects.executed_epoch().saturating_add(1);
        debug!(
            op = "archive_rpc_package_tx_protocol_fallback",
            tx_digest = %tx_digest,
            epoch,
            "falling back to raw effects for package transaction protocol"
        );
        let protocol_version = self
            .epoch_data(epoch)
            .await?
            .map(|epoch| epoch.protocol_version);
        let Some(protocol_version) = protocol_version else {
            return Ok(None);
        };
        self.package_tx_protocol_map
            .write()
            .unwrap()
            .insert(tx_digest, protocol_version);
        Ok(Some(protocol_version))
    }

    async fn genesis_tx(&self) -> Result<TransactionDigest> {
        if let Some(tx_digest) = *self.genesis_tx.read().unwrap() {
            return Ok(tx_digest);
        }
        let tx_digest = *self
            .checkpoint(0)
            .await?
            .transactions
            .first()
            .context("genesis checkpoint has no transaction")?;
        self.genesis_tx.write().unwrap().replace(tx_digest);
        Ok(tx_digest)
    }
}

impl TransactionStore for ArchiveRpcDataStore {
    fn transaction_data_and_effects(
        &self,
        tx_digest: &str,
    ) -> Result<Option<TransactionInfo>, Error> {
        let digest = TransactionDigest::from_str(tx_digest)
            .with_context(|| format!("invalid transaction digest {}", tx_digest))?;
        let response = block_on!(
            self.transaction_response(
                digest,
                SuiTransactionBlockResponseOptions::new()
                    .with_raw_input()
                    .with_raw_effects(),
            )
        )?;
        let signed_data = sender_signed_data_from_response(&response)?;
        let effects = effects_from_response(&response)?;
        let checkpoint = response
            .checkpoint
            .with_context(|| format!("transaction {} response missing checkpoint", tx_digest))?;

        Ok(Some(TransactionInfo {
            data: signed_data.transaction_data().clone(),
            effects,
            checkpoint,
        }))
    }
}

impl EpochStore for ArchiveRpcDataStore {
    fn epoch_info(&self, epoch: u64) -> Result<Option<EpochData>, Error> {
        block_on!(self.epoch_data(epoch))
    }

    fn protocol_config(&self, epoch: u64) -> Result<Option<ProtocolConfig>, Error> {
        Ok(self.epoch_info(epoch)?.map(|epoch| {
            ProtocolConfig::get_for_version(
                ProtocolVersion::new(epoch.protocol_version),
                self.node.chain(),
            )
        }))
    }
}

impl ObjectStore for ArchiveRpcDataStore {
    fn get_objects(&self, keys: &[ObjectKey]) -> Result<Vec<Option<(Object, u64)>>, Error> {
        let mut results = vec![None; keys.len()];
        let mut version_keys = Vec::new();

        for (index, key) in keys.iter().cloned().enumerate() {
            match key.version_query {
                VersionQuery::Version(_) => version_keys.push((index, key)),
                VersionQuery::RootVersion(_) => {
                    results[index] = block_on!(self.root_version_object(&key))?;
                }
                VersionQuery::AtCheckpoint(_) => {
                    results[index] = block_on!(self.checkpoint_object(&key))?;
                }
            }
        }

        for (index, object) in block_on!(self.version_objects(&version_keys))? {
            results[index] = object;
        }

        Ok(results)
    }
}

impl SetupStore for ArchiveRpcDataStore {
    fn setup(&self, _chain_id: Option<String>) -> Result<Option<String>, Error> {
        Ok(Some(block_on!(
            self.rpc_client.read_api().get_chain_identifier()
        )?))
    }
}

impl StoreSummary for ArchiveRpcDataStore {
    fn summary<W: Write>(&self, writer: &mut W) -> Result<()> {
        writeln!(writer, "ArchiveRpcDataStore (archive JSON-RPC only)")?;
        writeln!(writer, "  JSON-RPC: <configured>")?;
        writeln!(
            writer,
            "  Epochs cached: {}",
            self.epoch_map.read().unwrap().len()
        )?;
        Ok(())
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

fn object_response(response: SuiObjectResponse) -> Result<Option<(Object, u64)>, Error> {
    let data = match response.object() {
        Ok(data) => data,
        Err(_) => return Ok(None),
    };
    let object = object_from_sui_data(data)?;
    let version = object.version().value();
    Ok(Some((object, version)))
}

fn past_object_revision_response(
    response: SuiPastObjectResponse,
) -> Result<Option<SystemPackageRevision>> {
    match response {
        SuiPastObjectResponse::VersionFound(data) => object_revision_from_sui_data(&data),
        SuiPastObjectResponse::ObjectDeleted(_) | SuiPastObjectResponse::ObjectNotExists(_) => {
            Ok(None)
        }
        SuiPastObjectResponse::VersionNotFound(_, _) => Ok(None),
        SuiPastObjectResponse::VersionTooHigh { .. } => Ok(None),
    }
}

fn object_revision_response(response: SuiObjectResponse) -> Result<Option<SystemPackageRevision>> {
    let data = match response.object() {
        Ok(data) => data,
        Err(_) => return Ok(None),
    };
    object_revision_from_sui_data(data)
}

fn object_revision_from_sui_data(data: &SuiObjectData) -> Result<Option<SystemPackageRevision>> {
    Ok(Some(SystemPackageRevision {
        version: data.version,
        previous_transaction: data.previous_transaction.with_context(|| {
            format!(
                "system package {} version {} missing previous transaction",
                data.object_id, data.version
            )
        })?,
    }))
}

fn object_from_sui_data(data: &SuiObjectData) -> Result<Object, Error> {
    TryInto::<Object>::try_into(data.clone()).map_err(|err| anyhow!("{err}"))
}

fn sender_signed_data_from_response(
    response: &SuiTransactionBlockResponse,
) -> Result<SenderSignedData, Error> {
    if response.raw_transaction.is_empty() {
        bail!("transaction response missing raw transaction bytes");
    }
    bcs::from_bytes(&response.raw_transaction).context("failed to decode raw transaction bytes")
}

fn effects_from_response(response: &SuiTransactionBlockResponse) -> Result<TransactionEffects> {
    if response.raw_effects.is_empty() {
        bail!("transaction response missing raw effects bytes");
    }
    bcs::from_bytes(&response.raw_effects).context("failed to decode raw effects bytes")
}

fn epoch_start_timestamp_from_transaction(kind: &TransactionKind) -> Option<u64> {
    match kind {
        TransactionKind::ChangeEpoch(change) => Some(change.epoch_start_timestamp_ms),
        TransactionKind::EndOfEpochTransaction(kinds) => kinds.iter().find_map(|kind| {
            if let EndOfEpochTransactionKind::ChangeEpoch(change) = kind {
                Some(change.epoch_start_timestamp_ms)
            } else {
                None
            }
        }),
        _ => None,
    }
}

fn epoch_and_protocol_from_event(event: &SuiEvent) -> Result<(u64, u64)> {
    Ok((
        json_u64(&event.parsed_json, "epoch")?,
        json_u64(&event.parsed_json, "protocol_version")?,
    ))
}

fn json_u64(value: &serde_json::Value, field: &str) -> Result<u64> {
    let serde_json::Value::Object(map) = value else {
        bail!("unexpected event JSON shape");
    };
    match map.get(field) {
        Some(serde_json::Value::Number(number)) => number
            .as_u64()
            .with_context(|| format!("event field {} is not a u64", field)),
        Some(serde_json::Value::String(value)) => value
            .parse::<u64>()
            .with_context(|| format!("event field {} is not a u64", field)),
        Some(value) => bail!("event field {} has unexpected value {}", field, value),
        None => bail!("event field {} missing", field),
    }
}

fn system_package_ids(protocol_version: u64) -> BTreeSet<ObjectID> {
    let mut ids = BuiltInFramework::all_package_ids();
    if protocol_version < 5 {
        ids.retain(|id| *id != DEEPBOOK_PACKAGE_ID);
    }
    ids.into_iter().collect()
}
