/// bitcoin/rpc.rs
///
/// Thin wrapper around `bitcoincore-rpc` providing:
///  - Cookie-file authentication (Bitcoin Knots compatible)
///  - `getblocktemplate`
///  - `submitblock`
///  - Best-block-hash polling (ZMQ fallback)
use crate::{config::RpcConfig, error::PoolError};
use anyhow::{anyhow, Result};
use bitcoincore_rpc::{Client, RpcApi};
use serde_json::{json, Value};
use tracing::{info, warn};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct GbtResult {
    pub version: u32,
    pub prev_hash: String,
    pub bits: String,
    pub cur_time: u32,
    pub height: u64,
    pub coinbase_value: u64,
    pub transactions: Vec<GbtTransaction>,
    pub longpoll_id: Option<String>,
    pub default_witness_commitment: Option<String>,
    pub rules: Vec<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct GbtTransaction {
    pub data: Vec<u8>,
    pub txid: String,
    pub hash: String,
    pub fee: u64,
    pub weight: u64,
}

pub struct RpcClient {
    inner: Client,
}

impl RpcClient {
    pub fn new(cfg: &RpcConfig) -> Result<Self> {
        let auth = cfg.rpc_auth()?;
        let inner = Client::new(&cfg.url, auth)?;
        info!("Bitcoin RPC connected to {}", cfg.url);
        Ok(Self { inner })
    }

    pub fn get_block_template(&self) -> Result<GbtResult, PoolError> {
        let request = json!({
            "rules": ["segwit"],
            "capabilities": ["coinbasetxn", "workid"]
        });

        let result: Value = self
            .inner
            .call("getblocktemplate", &[request])
            .map_err(PoolError::Rpc)?;

        let transactions = result
            .get("transactions")
            .and_then(Value::as_array)
            .map(|txs| txs.iter().map(parse_gbt_transaction).collect())
            .transpose()?
            .unwrap_or_default();

        Ok(GbtResult {
            version: value_as_u32(&result, "version")?,
            prev_hash: value_as_string(&result, "previousblockhash")?,
            bits: value_as_string(&result, "bits")?,
            cur_time: value_as_u32(&result, "curtime")?,
            height: value_as_u64(&result, "height")?,
            coinbase_value: value_as_u64(&result, "coinbasevalue")?,
            transactions,
            longpoll_id: result
                .get("longpollid")
                .and_then(Value::as_str)
                .map(str::to_owned),
            default_witness_commitment: result
                .get("default_witness_commitment")
                .and_then(Value::as_str)
                .map(str::to_owned),
            rules: result
                .get("rules")
                .and_then(Value::as_array)
                .map(|rules| {
                    rules
                        .iter()
                        .filter_map(|r| r.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    pub fn submit_block(&self, block_hex: &str) -> Result<(), PoolError> {
        let result: Value = self
            .inner
            .call("submitblock", &[json!(block_hex)])
            .map_err(PoolError::Rpc)?;

        if result.is_null() {
            info!("🎉 Block accepted by network!");
            return Ok(());
        }

        if let Some(reason) = result.as_str() {
            warn!("submitblock rejected: {reason}");
            return Err(PoolError::SubmitBlockRejected(reason.to_owned()));
        }

        Err(PoolError::Other(anyhow!(
            "unexpected submitblock response: {result}"
        )))
    }

    pub fn best_block_hash(&self) -> Result<String, PoolError> {
        Ok(self
            .inner
            .get_best_block_hash()
            .map_err(PoolError::Rpc)?
            .to_string())
    }
}

fn parse_gbt_transaction(tx: &Value) -> Result<GbtTransaction, PoolError> {
    let data_hex = tx
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| PoolError::Other(anyhow!("GBT transaction missing data field")))?;

    let data = hex::decode(data_hex)
        .map_err(|e| PoolError::Other(anyhow!("GBT transaction data hex decode: {e}")))?;

    Ok(GbtTransaction {
        data,
        txid: value_as_string(tx, "txid")?,
        hash: tx
            .get("hash")
            .and_then(Value::as_str)
            .or_else(|| tx.get("wtxid").and_then(Value::as_str))
            .unwrap_or_default()
            .to_owned(),
        fee: value_as_u64(tx, "fee")?,
        weight: value_as_u64(tx, "weight")?,
    })
}

fn value_as_string(v: &Value, key: &str) -> Result<String, PoolError> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            PoolError::Other(anyhow!("missing or invalid getblocktemplate field: {key}"))
        })
}

fn value_as_u64(v: &Value, key: &str) -> Result<u64, PoolError> {
    v.get(key).and_then(Value::as_u64).ok_or_else(|| {
        PoolError::Other(anyhow!("missing or invalid getblocktemplate field: {key}"))
    })
}

fn value_as_u32(v: &Value, key: &str) -> Result<u32, PoolError> {
    let n = value_as_u64(v, key)?;
    u32::try_from(n).map_err(|_| {
        PoolError::Other(anyhow!(
            "getblocktemplate field out of range for u32: {key}"
        ))
    })
}
