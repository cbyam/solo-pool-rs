/// settings.rs
///
/// Runtime-mutable pool settings, editable from the dashboard's Settings page.
///
/// The payout address and expected network start from config.toml, may be
/// overridden by values persisted in the stats SQLite (set via the dashboard),
/// and can be changed at runtime. The template engine reads the address on
/// every GBT refresh, so a change takes effect on the next job — combined with
/// the forced clean-job refresh after a save, miners switch payout immediately.
///
/// The network selector controls which network addresses are validated
/// against (and the dashboard badge). It does NOT switch the Bitcoin node —
/// the node the pool connects to decides the actual chain.
use bitcoin::{address::NetworkUnchecked, Address, Network};
use parking_lot::RwLock;
use std::sync::Arc;
use tracing::warn;

pub const NETWORKS: [&str; 4] = ["mainnet", "testnet", "signet", "regtest"];

pub struct RuntimeSettings {
    coinbase_address: RwLock<String>,
    network: RwLock<String>,
}

impl RuntimeSettings {
    /// Seed from config. The config address is validated *leniently* (warn
    /// only) so existing deployments that predate the network field keep
    /// booting; dashboard-driven changes go through [`Self::update`] and are
    /// validated strictly.
    pub fn from_config(pool_cfg: &crate::config::PoolConfig) -> Arc<Self> {
        if let Err(e) = validate(&pool_cfg.coinbase_address, &pool_cfg.network) {
            warn!(
                "coinbase_address does not validate against [pool] network=\"{}\": {e} — \
                 continuing with the configured address; fix the config or use the \
                 dashboard Settings page",
                pool_cfg.network
            );
        }
        Arc::new(Self {
            coinbase_address: RwLock::new(pool_cfg.coinbase_address.clone()),
            network: RwLock::new(pool_cfg.network.clone()),
        })
    }

    /// Apply values persisted from a previous dashboard save. Lenient: a bad
    /// stored pair is logged and skipped rather than killing boot.
    pub fn apply_persisted(&self, address: Option<String>, network: Option<String>) {
        let net = network.unwrap_or_else(|| self.network());
        if !NETWORKS.contains(&net.as_str()) {
            warn!("Ignoring persisted network \"{net}\": unknown");
            return;
        }
        if let Some(addr) = address {
            match validate(&addr, &net) {
                Ok(()) => {
                    *self.coinbase_address.write() = addr;
                    *self.network.write() = net;
                }
                Err(e) => warn!("Ignoring persisted settings: {e}"),
            }
        } else {
            *self.network.write() = net;
        }
    }

    /// Strictly validate and apply a dashboard-driven change.
    pub fn update(&self, address: &str, network: &str) -> Result<(), String> {
        if !NETWORKS.contains(&network) {
            return Err(format!(
                "unknown network \"{network}\" (expected one of {NETWORKS:?})"
            ));
        }
        validate(address, network)?;
        *self.coinbase_address.write() = address.to_string();
        *self.network.write() = network.to_string();
        Ok(())
    }

    pub fn coinbase_address(&self) -> String {
        self.coinbase_address.read().clone()
    }

    pub fn network(&self) -> String {
        self.network.read().clone()
    }
}

/// Parse an address and require it to belong to `network`.
fn validate(address: &str, network: &str) -> Result<(), String> {
    let net = match network {
        "mainnet" => Network::Bitcoin,
        "testnet" => Network::Testnet,
        "signet" => Network::Signet,
        "regtest" => Network::Regtest,
        other => return Err(format!("unknown network \"{other}\"")),
    };
    let parsed: Address<NetworkUnchecked> = address
        .parse()
        .map_err(|e| format!("invalid address: {e}"))?;
    parsed
        .require_network(net)
        .map(|_| ())
        .map_err(|_| format!("address is not valid for {network}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(addr: &str, net: &str) -> RuntimeSettings {
        RuntimeSettings {
            coinbase_address: RwLock::new(addr.to_string()),
            network: RwLock::new(net.to_string()),
        }
    }

    /// Derive a guaranteed-valid P2WPKH address (secp generator point as the
    /// pubkey) so the tests don't depend on hand-typed bech32 checksums.
    fn addr_for(net: Network) -> String {
        use std::str::FromStr;
        let pk = bitcoin::CompressedPublicKey::from_str(
            "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
        )
        .unwrap();
        bitcoin::Address::p2wpkh(&pk, net).to_string()
    }

    #[test]
    fn update_accepts_matching_network() {
        let mainnet = addr_for(Network::Bitcoin);
        let s = settings("x", "mainnet");
        assert!(s.update(&mainnet, "mainnet").is_ok());
        assert_eq!(s.coinbase_address(), mainnet);
    }

    #[test]
    fn update_rejects_wrong_network_and_keeps_previous() {
        let mainnet = addr_for(Network::Bitcoin);
        let testnet = addr_for(Network::Testnet);
        let s = settings(&mainnet, "mainnet");
        assert!(s.update(&testnet, "mainnet").is_err());
        assert!(s.update(&mainnet, "testnet").is_err());
        assert!(s.update("not-an-address", "mainnet").is_err());
        assert!(s.update(&mainnet, "lightning").is_err());
        assert_eq!(s.coinbase_address(), mainnet);
        assert_eq!(s.network(), "mainnet");
    }

    #[test]
    fn testnet_address_validates_on_testnet_and_signet() {
        let testnet = addr_for(Network::Testnet);
        let s = settings("x", "testnet");
        assert!(s.update(&testnet, "testnet").is_ok());
        // Signet shares the tb1 HRP with testnet.
        assert!(s.update(&testnet, "signet").is_ok());
    }

    #[test]
    fn persisted_values_are_lenient() {
        let mainnet = addr_for(Network::Bitcoin);
        let testnet = addr_for(Network::Testnet);
        let s = settings(&mainnet, "mainnet");
        // Bad persisted pair is ignored, boot state survives.
        s.apply_persisted(Some(testnet.clone()), Some("mainnet".into()));
        assert_eq!(s.coinbase_address(), mainnet);
        // Good persisted pair applies.
        s.apply_persisted(Some(testnet.clone()), Some("testnet".into()));
        assert_eq!(s.network(), "testnet");
    }
}
