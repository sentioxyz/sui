// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result, bail};
use fastcrypto::encoding::Base64;
use move_core_types::language_storage::StructTag;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize, de::DeserializeOwned, de::IgnoredAny};
use serde_json::{Value, json};
use serde_with::serde_as;
use std::{collections::BTreeMap, time::Duration};
use sui_json_rpc_types::{
    EventFilter, EventPage, SuiTransactionBlockResponse, SuiTransactionBlockResponseOptions,
};
use sui_protocol_config::ProtocolConfig;
use sui_types::{
    base_types::{ObjectID, SequenceNumber},
    digests::TransactionDigest,
    event::EventID,
    move_package::{MovePackage, TypeOrigin, UpgradeInfo},
    object::{Data, MoveObject, Object, ObjectInner, Owner},
    sui_serde::{BigInt, SequenceNumber as AsSequenceNumber, SuiStructTag},
};
use tokio::sync::Semaphore;

/// The legacy JSON-RPC methods still used by archive replay.
pub(super) struct LegacyRpcClient {
    client: Client,
    endpoint: Url,
    requests: Semaphore,
}

impl LegacyRpcClient {
    pub(super) fn new(endpoint: &str, timeout: Duration, max_requests: usize) -> Result<Self> {
        let endpoint = Url::parse(endpoint).context("invalid archive JSON-RPC URL")?;
        let client = Client::builder().timeout(timeout).build()?;
        Ok(Self {
            client,
            endpoint,
            requests: Semaphore::new(max_requests.min(Semaphore::MAX_PERMITS)),
        })
    }

    async fn request<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        let _permit = self.requests.acquire().await?;
        let response = self
            .client
            .post(self.endpoint.clone())
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(reqwest::Error::without_url)?;

        match response.json::<RpcResponse<T>>().await? {
            RpcResponse::Failure { error } => {
                bail!("{} failed ({}): {}", method, error.code, error.message)
            }
            RpcResponse::Success { result } => Ok(result),
        }
    }

    pub(super) async fn get_transaction_with_options(
        &self,
        digest: TransactionDigest,
        options: SuiTransactionBlockResponseOptions,
    ) -> Result<SuiTransactionBlockResponse> {
        self.request("sui_getTransactionBlock", json!([digest, options]))
            .await
    }

    pub(super) async fn get_checkpoint(&self, checkpoint: u64) -> Result<Checkpoint> {
        self.request("sui_getCheckpoint", json!([BigInt::from(checkpoint)]))
            .await
    }

    pub(super) async fn try_multi_get_parsed_past_object(
        &self,
        requests: Vec<SuiGetPastObjectRequest>,
        options: SuiObjectDataOptions,
    ) -> Result<Vec<SuiPastObjectResponse>> {
        let count = requests.len();
        let objects: Vec<_> = self
            .request("sui_tryMultiGetPastObjects", json!([requests, options]))
            .await?;
        anyhow::ensure!(
            objects.len() == count,
            "archive returned an incorrect number of past objects"
        );
        Ok(objects)
    }

    pub(super) async fn try_get_object_before_version(
        &self,
        object: ObjectID,
        version: SequenceNumber,
    ) -> Result<SuiPastObjectResponse> {
        self.request("sui_tryGetObjectBeforeVersion", json!([object, version]))
            .await
    }

    pub(super) async fn multi_get_object_with_options(
        &self,
        objects: Vec<ObjectID>,
        options: SuiObjectDataOptions,
    ) -> Result<Vec<SuiObjectResponse>> {
        let count = objects.len();
        let responses: Vec<_> = self
            .request("sui_multiGetObjects", json!([objects, options]))
            .await?;
        anyhow::ensure!(
            responses.len() == count,
            "archive returned an incorrect number of objects"
        );
        Ok(responses)
    }

    pub(super) async fn query_events(
        &self,
        filter: EventFilter,
        cursor: Option<EventID>,
        limit: Option<usize>,
        descending: bool,
    ) -> Result<EventPage> {
        self.request(
            "suix_queryEvents",
            json!([filter, cursor, limit, descending]),
        )
        .await
    }

    pub(super) async fn get_chain_identifier(&self) -> Result<String> {
        self.request("sui_getChainIdentifier", json!([])).await
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RpcResponse<T> {
    Failure { error: RpcError },
    Success { result: T },
}

#[derive(Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

#[serde_as]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Checkpoint {
    #[serde_as(as = "BigInt<u64>")]
    pub(super) epoch: u64,
    #[serde_as(as = "BigInt<u64>")]
    pub(super) timestamp_ms: u64,
    pub(super) transactions: Vec<TransactionDigest>,
}

#[derive(Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SuiObjectDataOptions {
    show_type: bool,
    show_owner: bool,
    show_previous_transaction: bool,
    show_display: bool,
    show_content: bool,
    show_bcs: bool,
    show_storage_rebate: bool,
}

impl SuiObjectDataOptions {
    pub(super) fn bcs_lossless() -> Self {
        Self {
            show_type: true,
            show_owner: true,
            show_previous_transaction: true,
            show_bcs: true,
            show_storage_rebate: true,
            ..Self::default()
        }
    }

    pub(super) fn with_previous_transaction() -> Self {
        Self {
            show_previous_transaction: true,
            ..Self::default()
        }
    }
}

#[serde_as]
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SuiGetPastObjectRequest {
    pub(super) object_id: ObjectID,
    #[serde_as(as = "AsSequenceNumber")]
    pub(super) version: SequenceNumber,
}

#[derive(Deserialize)]
pub(super) struct SuiObjectResponse {
    pub(super) data: Option<SuiObjectData>,
}

#[derive(Deserialize)]
#[serde(tag = "status", content = "details")]
pub(super) enum SuiPastObjectResponse {
    VersionFound(Box<SuiObjectData>),
    ObjectNotExists(IgnoredAny),
    ObjectDeleted(IgnoredAny),
    VersionNotFound(IgnoredAny),
    VersionTooHigh(IgnoredAny),
}

impl SuiPastObjectResponse {
    pub(super) fn into_data(self) -> Option<SuiObjectData> {
        match self {
            Self::VersionFound(data) => Some(*data),
            Self::ObjectNotExists(_)
            | Self::ObjectDeleted(_)
            | Self::VersionNotFound(_)
            | Self::VersionTooHigh(_) => None,
        }
    }
}

#[serde_as]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SuiObjectData {
    pub(super) object_id: ObjectID,
    #[serde_as(as = "AsSequenceNumber")]
    pub(super) version: SequenceNumber,
    owner: Option<Owner>,
    pub(super) previous_transaction: Option<TransactionDigest>,
    #[serde_as(as = "Option<BigInt<u64>>")]
    storage_rebate: Option<u64>,
    bcs: Option<RawData>,
}

#[derive(Deserialize)]
#[serde(tag = "dataType", rename_all = "camelCase")]
enum RawData {
    MoveObject(RawMoveObject),
    Package(RawMovePackage),
}

#[serde_as]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMoveObject {
    #[serde(rename = "type")]
    #[serde_as(as = "SuiStructTag")]
    type_: StructTag,
    has_public_transfer: bool,
    version: SequenceNumber,
    #[serde_as(as = "Base64")]
    bcs_bytes: Vec<u8>,
}

#[serde_as]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMovePackage {
    id: ObjectID,
    #[serde_as(as = "BTreeMap<_, Base64>")]
    module_map: BTreeMap<String, Vec<u8>>,
    type_origin_table: Vec<TypeOrigin>,
    linkage_table: BTreeMap<ObjectID, UpgradeInfo>,
}

impl SuiObjectData {
    pub(super) fn into_object(self) -> Result<(Object, u64)> {
        let data = match self.bcs.context("archive object missing BCS data")? {
            RawData::MoveObject(object) => Data::Move(unsafe {
                // The archive supplies the BCS contents and type together, as in the legacy SDK.
                MoveObject::new_from_execution(
                    object.type_.into(),
                    object.has_public_transfer,
                    object.version,
                    object.bcs_bytes,
                    &ProtocolConfig::get_for_min_version(),
                    false,
                )?
            }),
            RawData::Package(package) => Data::Package(MovePackage::new(
                package.id,
                self.version,
                package.module_map,
                u64::MAX,
                package.type_origin_table,
                package.linkage_table,
            )?),
        };
        let object: Object = ObjectInner {
            data,
            owner: self.owner.context("archive object missing owner")?,
            previous_transaction: self
                .previous_transaction
                .context("archive object missing previous transaction")?,
            storage_rebate: self
                .storage_rebate
                .context("archive object missing storage rebate")?,
        }
        .into();
        let version = object.version().value();
        Ok((object, version))
    }
}
