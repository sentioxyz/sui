// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! gRPC-only data store for replay, backed by the fullnode `sui.rpc.v2` API.
//!
//! This is the gRPC analog of [`super::ArchiveRpcDataStore`]. It intentionally
//! does not depend on JSON-RPC or GraphQL: it talks to a node's `LedgerService`
//! over gRPC, which upstream intends to keep supported after JSON-RPC is
//! deprecated.
//!
//! The fullnode gRPC API can only read objects at an exact version (or the
//! latest live version); it has no "latest version <= bound" query. The replay
//! engine, however, asks for child/dynamic-field objects via
//! [`VersionQuery::RootVersion`] whose exact version is discovered at runtime.
//! We recover those exact versions from the replayed transaction's effects:
//! `changed_objects[].input_version` plus `unchanged_loaded_runtime_objects[]`
//! together pin every object the transaction read, at the exact version it was
//! read at. Those hints are captured when the transaction is fetched and then
//! consumed to turn each `RootVersion` query into an exact-version read.
//!
//! System-package versioning at a checkpoint mirrors `ArchiveRpcDataStore`
//! (binary search over the package revision history dated by protocol version),
//! with epoch metadata resolved directly from `GetEpoch` instead of scanning
//! `SystemEpochInfoEvent`s.

use crate::{
    EpochData, EpochStore, ObjectKey, ObjectStore, SetupStore, StoreSummary, TransactionInfo,
    TransactionStore, VersionQuery, node::Node,
};
use anyhow::{Context, Error, Result, anyhow, bail};
use mysten_common::ZipDebugEqIteratorExt;
use prost_types::FieldMask;
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    future::Future,
    io::Write,
    str::FromStr,
    sync::RwLock,
    time::Duration,
};
use sui_framework::BuiltInFramework;
use sui_rpc::Client;
use sui_rpc::field::FieldMaskUtil;
use sui_rpc::proto::sui::rpc::v2 as proto;
use sui_types::{
    DEEPBOOK_PACKAGE_ID,
    base_types::{ObjectID, SequenceNumber},
    committee::ProtocolVersion,
    digests::TransactionDigest,
    effects::{TransactionEffects, TransactionEffectsAPI},
    object::Object,
    supported_protocol_versions::ProtocolConfig,
    transaction::{DEFAULT_VALIDATOR_GAS_PRICE, TransactionData},
};
use tracing::debug;

const GRPC_OBJECT_CHUNK_SIZE: usize = 256;

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

async fn retry_backoff(attempt: u64, retries: u64, backoff_ms: u64) {
    if attempt < retries {
        tokio::time::sleep(Duration::from_millis(
            backoff_ms.saturating_mul(attempt + 1),
        ))
        .await;
    }
}

/// Retry a gRPC call with the same linear backoff policy the JSON-RPC stores use.
async fn with_retry<T, F, Fut>(what: &str, op: F) -> Result<T, Error>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, tonic::Status>>,
{
    let retries = env_u64("SUI_DATA_STORE_RPC_RETRIES", 4);
    let backoff_ms = env_u64("SUI_DATA_STORE_RPC_RETRY_BACKOFF_MS", 250);
    let mut last_error = None;
    for attempt in 0..=retries {
        match op().await {
            Ok(value) => return Ok(value),
            Err(err) => {
                last_error = Some(err.to_string());
                retry_backoff(attempt, retries, backoff_ms).await;
            }
        }
    }
    bail!(
        "failed gRPC call ({}): {}",
        what,
        last_error.unwrap_or_else(|| "unknown error".to_string())
    )
}

/// Replay data store backed only by the fullnode gRPC `sui.rpc.v2` API.
pub struct GrpcDataStore {
    node: Node,
    client: Client,
    /// object_id -> exact version it was read at by the replayed transaction.
    /// Populated from transaction effects; used to resolve `RootVersion` queries.
    root_version_hints: RwLock<BTreeMap<ObjectID, u64>>,
    epoch_map: RwLock<BTreeMap<u64, EpochData>>,
    checkpoint_protocol_map: RwLock<BTreeMap<u64, u64>>,
    system_package_table: RwLock<SystemPackageTable>,
    system_package_revision_map: RwLock<BTreeMap<ObjectID, Vec<SystemPackageRevision>>>,
    package_tx_protocol_map: RwLock<BTreeMap<TransactionDigest, u64>>,
    genesis_tx: RwLock<Option<TransactionDigest>>,
    latest_protocol_version: RwLock<Option<u64>>,
}

impl GrpcDataStore {
    pub fn new(node: Node, grpc_url: &str, _version: &str) -> Result<Self, Error> {
        let uri = normalize_grpc_uri(grpc_url);
        let client = Client::new(uri.clone())
            .map_err(|err| anyhow!("failed to create gRPC client for {}: {}", uri, err))?;
        Ok(Self {
            node,
            client,
            root_version_hints: RwLock::new(BTreeMap::new()),
            epoch_map: RwLock::new(BTreeMap::new()),
            checkpoint_protocol_map: RwLock::new(BTreeMap::new()),
            system_package_table: RwLock::new(BTreeMap::new()),
            system_package_revision_map: RwLock::new(BTreeMap::new()),
            package_tx_protocol_map: RwLock::new(BTreeMap::new()),
            genesis_tx: RwLock::new(None),
            latest_protocol_version: RwLock::new(None),
        })
    }

    /// Fetch objects at exact (or latest, when `version` is `None`) versions.
    /// Preserves request order; a missing object maps to `None`.
    async fn fetch_objects(
        &self,
        requests: &[(ObjectID, Option<u64>)],
    ) -> Result<Vec<Option<Object>>, Error> {
        let mut results = Vec::with_capacity(requests.len());
        for chunk in requests.chunks(GRPC_OBJECT_CHUNK_SIZE) {
            let proto_requests = chunk
                .iter()
                .map(|(object_id, version)| {
                    let mut request = proto::GetObjectRequest::default()
                        .with_object_id(object_id.to_string())
                        .with_read_mask(FieldMask::from_paths(["bcs"]));
                    request.version = *version;
                    request
                })
                .collect::<Vec<_>>();
            let response = with_retry("batch_get_objects", || {
                let mut client = self.client.clone();
                let request = proto::BatchGetObjectsRequest::default()
                    .with_requests(proto_requests.clone())
                    .with_read_mask(FieldMask::from_paths(["bcs"]));
                async move {
                    client
                        .ledger_client()
                        .batch_get_objects(request)
                        .await
                        .map(|response| response.into_inner())
                }
            })
            .await?;

            for result in response.objects {
                match result.to_result() {
                    Ok(object) => results.push(Some(decode_object(&object)?)),
                    // A per-object error is a normal "not found", matching the
                    // JSON-RPC stores' None for deleted/missing objects.
                    Err(_) => results.push(None),
                }
            }
        }

        if results.len() != requests.len() {
            bail!(
                "gRPC batch_get_objects returned {} results for {} requests",
                results.len(),
                requests.len()
            );
        }
        Ok(results)
    }

    async fn fetch_object(
        &self,
        object_id: ObjectID,
        version: Option<u64>,
    ) -> Result<Option<(Object, u64)>, Error> {
        let object = self.fetch_objects(&[(object_id, version)]).await?;
        Ok(object.into_iter().next().flatten().map(|object| {
            let version = object.version().value();
            (object, version)
        }))
    }

    async fn version_objects(
        &self,
        indexed: &[(usize, ObjectKey)],
    ) -> Result<Vec<(usize, Option<(Object, u64)>)>, Error> {
        let requests = indexed
            .iter()
            .map(|(_, key)| {
                let VersionQuery::Version(version) = key.version_query else {
                    unreachable!("version_objects only accepts Version queries");
                };
                (key.object_id, Some(version))
            })
            .collect::<Vec<_>>();
        let objects = self.fetch_objects(&requests).await?;
        Ok(indexed
            .iter()
            .zip_debug_eq(objects)
            .map(|((index, _), object)| {
                (
                    *index,
                    object.map(|object| {
                        let version = object.version().value();
                        (object, version)
                    }),
                )
            })
            .collect())
    }

    /// Resolve a `RootVersion` (child object at version <= bound) read.
    ///
    /// The fullnode gRPC API has no bounded read, so we use the exact version
    /// the replayed transaction loaded this object at, captured from effects.
    /// An object absent from the hints was not loaded by the transaction (e.g. a
    /// `dynamic_field::exists_` probe of a missing field) and resolves to `None`,
    /// matching `try_get_object_before_version` returning nothing.
    async fn root_version_object(&self, key: &ObjectKey) -> Result<Option<(Object, u64)>, Error> {
        let VersionQuery::RootVersion(bound) = key.version_query else {
            unreachable!("root_version_object only accepts RootVersion queries");
        };
        let hint = self
            .root_version_hints
            .read()
            .unwrap()
            .get(&key.object_id)
            .copied();
        match hint {
            Some(version) => self.fetch_object(key.object_id, Some(version)).await,
            None => {
                debug!(
                    op = "grpc_root_version_miss",
                    object_id = %key.object_id,
                    bound,
                    "no effects-derived version hint; treating as not found"
                );
                Ok(None)
            }
        }
    }

    async fn checkpoint_object(&self, key: &ObjectKey) -> Result<Option<(Object, u64)>, Error> {
        let VersionQuery::AtCheckpoint(checkpoint) = key.version_query else {
            unreachable!("checkpoint_object only accepts AtCheckpoint queries");
        };
        let protocol_version = self.protocol_version_for_checkpoint(checkpoint).await?;
        if system_package_ids(protocol_version).contains(&key.object_id) {
            if protocol_version >= self.latest_protocol_version().await? {
                return self.fetch_object(key.object_id, None).await;
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
            return self
                .fetch_object(key.object_id, Some(version.value()))
                .await;
        }

        // Non-system objects mirror ArchiveRpcDataStore: read the latest version,
        // deliberately not bounding by the checkpoint.
        self.fetch_object(key.object_id, None).await
    }

    async fn get_epoch(&self, epoch: Option<u64>) -> Result<Option<proto::Epoch>, Error> {
        let response = with_retry("get_epoch", || {
            let mut client = self.client.clone();
            let mut request =
                proto::GetEpochRequest::default().with_read_mask(FieldMask::from_paths([
                    "epoch",
                    "protocol_config.protocol_version",
                    "reference_gas_price",
                    "start",
                ]));
            if let Some(epoch) = epoch {
                request.set_epoch(epoch);
            }
            async move {
                client
                    .ledger_client()
                    .get_epoch(request)
                    .await
                    .map(|response| response.into_inner())
            }
        })
        .await;
        match response {
            Ok(response) => Ok(response.epoch),
            Err(err) => {
                // GetEpoch for an unknown epoch surfaces as a tonic error; the
                // JSON-RPC store models a missing epoch as None.
                debug!(op = "grpc_get_epoch_error", ?epoch, error = %err, "epoch fetch failed");
                Ok(None)
            }
        }
    }

    async fn checkpoint_epoch(&self, checkpoint: u64) -> Result<u64, Error> {
        let response = with_retry("get_checkpoint", || {
            let mut client = self.client.clone();
            let request = proto::GetCheckpointRequest::by_sequence_number(checkpoint)
                .with_read_mask(FieldMask::from_paths(["summary.epoch"]));
            async move {
                client
                    .ledger_client()
                    .get_checkpoint(request)
                    .await
                    .map(|response| response.into_inner())
            }
        })
        .await?;
        response
            .checkpoint
            .and_then(|checkpoint| checkpoint.summary)
            .and_then(|summary| summary.epoch)
            .with_context(|| format!("checkpoint {} missing epoch", checkpoint))
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
        let epoch = self.checkpoint_epoch(checkpoint).await?;
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
        let Some(epoch_proto) = self.get_epoch(Some(epoch)).await? else {
            return Ok(None);
        };
        let start_timestamp = epoch_proto
            .start
            .map(timestamp_to_ms)
            .transpose()?
            .with_context(|| format!("epoch {} missing start timestamp", epoch))?;

        // Epoch 0 predates on-chain protocol/gas-price metadata; mirror the
        // JSON-RPC store's genesis defaults.
        if epoch == 0 {
            return Ok(Some(EpochData {
                epoch_id: 0,
                protocol_version: 1,
                rgp: DEFAULT_VALIDATOR_GAS_PRICE,
                start_timestamp,
            }));
        }

        let protocol_version = epoch_proto
            .protocol_config
            .as_ref()
            .and_then(|config| config.protocol_version)
            .with_context(|| format!("epoch {} missing protocol version", epoch))?;
        let rgp = epoch_proto
            .reference_gas_price
            .with_context(|| format!("epoch {} missing reference gas price", epoch))?;

        Ok(Some(EpochData {
            epoch_id: epoch,
            protocol_version,
            rgp,
            start_timestamp,
        }))
    }

    async fn latest_protocol_version(&self) -> Result<u64, Error> {
        if let Some(protocol_version) = *self.latest_protocol_version.read().unwrap() {
            return Ok(protocol_version);
        }
        let epoch = self
            .get_epoch(None)
            .await?
            .context("current epoch not found")?;
        let protocol_version = epoch
            .protocol_config
            .as_ref()
            .and_then(|config| config.protocol_version)
            .context("current epoch missing protocol version")?;
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

        Ok(candidate.map(|revision| revision.version))
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

        let Some((latest, _)) = self.fetch_object(*package_id, None).await? else {
            return Ok(Vec::new());
        };
        let latest_version = latest.version().value();

        let requests = (1..=latest_version)
            .map(|version| (*package_id, Some(version)))
            .collect::<Vec<_>>();
        let objects = self.fetch_objects(&requests).await?;
        let mut revisions = objects
            .into_iter()
            .flatten()
            .map(|object| SystemPackageRevision {
                version: object.version(),
                previous_transaction: object.previous_transaction,
            })
            .collect::<Vec<_>>();
        revisions.sort_by_key(|revision| revision.version);

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

    /// Date a system-package revision to the protocol version it was published
    /// under. The revision's `previous_transaction` is the epoch-change
    /// transaction that performed the framework upgrade; the new framework takes
    /// effect in the following epoch, whose protocol version we look up.
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

        if tx_digest == self.genesis_tx().await? {
            self.package_tx_protocol_map
                .write()
                .unwrap()
                .insert(tx_digest, 1);
            return Ok(Some(1));
        }

        let info = self.fetch_transaction(tx_digest).await?;
        let Some(info) = info else {
            return Ok(None);
        };
        let epoch = info.effects.executed_epoch().saturating_add(1);
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

    async fn genesis_tx(&self) -> Result<TransactionDigest, Error> {
        if let Some(tx_digest) = *self.genesis_tx.read().unwrap() {
            return Ok(tx_digest);
        }
        let response = with_retry("get_checkpoint", || {
            let mut client = self.client.clone();
            let request = proto::GetCheckpointRequest::by_sequence_number(0)
                .with_read_mask(FieldMask::from_paths(["transactions.digest"]));
            async move {
                client
                    .ledger_client()
                    .get_checkpoint(request)
                    .await
                    .map(|response| response.into_inner())
            }
        })
        .await?;
        let digest = response
            .checkpoint
            .and_then(|checkpoint| checkpoint.transactions.into_iter().next())
            .and_then(|transaction| transaction.digest)
            .context("genesis checkpoint has no transaction")?;
        let tx_digest = TransactionDigest::from_str(&digest)
            .with_context(|| format!("invalid genesis transaction digest {}", digest))?;
        self.genesis_tx.write().unwrap().replace(tx_digest);
        Ok(tx_digest)
    }

    /// Fetch a transaction, its effects and checkpoint, and (for the replayed
    /// transaction) record the exact versions of every object it read.
    async fn fetch_transaction(
        &self,
        digest: TransactionDigest,
    ) -> Result<Option<TransactionInfo>, Error> {
        let response = with_retry("get_transaction", || {
            let mut client = self.client.clone();
            let request = proto::GetTransactionRequest::default()
                .with_digest(digest.to_string())
                .with_read_mask(FieldMask::from_paths([
                    "transaction.bcs",
                    "effects.bcs",
                    "effects.changed_objects",
                    "effects.unchanged_loaded_runtime_objects",
                    "checkpoint",
                ]));
            async move {
                client
                    .ledger_client()
                    .get_transaction(request)
                    .await
                    .map(|response| response.into_inner())
            }
        })
        .await?;

        let Some(executed) = response.transaction else {
            return Ok(None);
        };
        let proto_transaction = executed
            .transaction
            .as_ref()
            .context("transaction response missing transaction")?;
        let data: TransactionData = proto_transaction
            .bcs
            .as_ref()
            .context("transaction response missing transaction bcs")?
            .deserialize()
            .context("failed to decode transaction bcs")?;
        let proto_effects = executed
            .effects
            .as_ref()
            .context("transaction response missing effects")?;
        let effects: TransactionEffects = proto_effects
            .bcs
            .as_ref()
            .context("transaction response missing effects bcs")?
            .deserialize()
            .context("failed to decode effects bcs")?;
        let checkpoint = executed
            .checkpoint
            .with_context(|| format!("transaction {} response missing checkpoint", digest))?;

        self.record_version_hints(proto_effects);

        Ok(Some(TransactionInfo {
            data,
            effects,
            checkpoint,
        }))
    }

    /// Record, from a transaction's effects, the exact version at which each
    /// object was read: the pre-execution (`input_version`) of every changed
    /// object plus every read-only runtime-loaded object.
    fn record_version_hints(&self, effects: &proto::TransactionEffects) {
        let mut hints = self.root_version_hints.write().unwrap();
        for changed in &effects.changed_objects {
            if let (Some(object_id), Some(version)) = (&changed.object_id, changed.input_version)
                && let Ok(object_id) = ObjectID::from_str(object_id)
            {
                hints.insert(object_id, version);
            }
        }
        for loaded in &effects.unchanged_loaded_runtime_objects {
            if let (Some(object_id), Some(version)) = (&loaded.object_id, loaded.version)
                && let Ok(object_id) = ObjectID::from_str(object_id)
            {
                hints.insert(object_id, version);
            }
        }
    }

    async fn chain_identifier(&self) -> Result<String, Error> {
        let response = with_retry("get_service_info", || {
            let mut client = self.client.clone();
            async move {
                client
                    .ledger_client()
                    .get_service_info(proto::GetServiceInfoRequest::default())
                    .await
                    .map(|response| response.into_inner())
            }
        })
        .await?;
        response.chain_id.context("service info missing chain id")
    }
}

impl TransactionStore for GrpcDataStore {
    fn transaction_data_and_effects(
        &self,
        tx_digest: &str,
    ) -> Result<Option<TransactionInfo>, Error> {
        let digest = TransactionDigest::from_str(tx_digest)
            .with_context(|| format!("invalid transaction digest {}", tx_digest))?;
        block_on!(self.fetch_transaction(digest))
    }
}

impl EpochStore for GrpcDataStore {
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

impl ObjectStore for GrpcDataStore {
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

impl SetupStore for GrpcDataStore {
    fn setup(&self, _chain_id: Option<String>) -> Result<Option<String>, Error> {
        Ok(Some(block_on!(self.chain_identifier())?))
    }
}

impl StoreSummary for GrpcDataStore {
    fn summary<W: Write>(&self, writer: &mut W) -> Result<()> {
        writeln!(writer, "GrpcDataStore (fullnode gRPC sui.rpc.v2 only)")?;
        writeln!(
            writer,
            "  Epochs cached: {}",
            self.epoch_map.read().unwrap().len()
        )?;
        writeln!(
            writer,
            "  Root-version hints: {}",
            self.root_version_hints.read().unwrap().len()
        )?;
        Ok(())
    }
}

/// Normalize a configured gRPC endpoint into a URI tonic accepts. Accepts
/// `grpc://`/`grpcs://` (and scheme-less `host:port`) and maps them to
/// `http://`/`https://`.
fn normalize_grpc_uri(url: &str) -> String {
    let url = if let Some(rest) = url.strip_prefix("grpc://") {
        format!("http://{rest}")
    } else if let Some(rest) = url.strip_prefix("grpcs://") {
        format!("https://{rest}")
    } else if let Some(rest) = url.strip_prefix("grpc:") {
        format!("http://{rest}")
    } else {
        url.to_string()
    };
    if url.starts_with("http://") || url.starts_with("https://") {
        url
    } else {
        format!("http://{url}")
    }
}

fn decode_object(object: &proto::Object) -> Result<Object, Error> {
    object
        .bcs
        .as_ref()
        .context("object response missing bcs")?
        .deserialize()
        .context("failed to decode object bcs")
}

fn timestamp_to_ms(timestamp: prost_types::Timestamp) -> Result<u64, Error> {
    sui_rpc::proto::proto_to_timestamp_ms(timestamp)
        .map_err(|err| anyhow!("invalid epoch timestamp: {}", err))
}

fn system_package_ids(protocol_version: u64) -> BTreeSet<ObjectID> {
    let mut ids = BuiltInFramework::all_package_ids();
    if protocol_version < 5 {
        ids.retain(|id| *id != DEEPBOOK_PACKAGE_ID);
    }
    ids.into_iter().collect()
}
