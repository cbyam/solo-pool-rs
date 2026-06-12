/// settings.rs
///
/// Runtime-mutable pool settings, editable from the dashboard's Settings page.
///
/// Safety model — the node is the source of truth:
///  - The network is detected from the connected node (`getblockchaininfo`)
///    at boot and is fixed for the process lifetime. The dashboard shows it
///    read-only; there is no user-selectable network.
///  - The payout address must validate against that network before the
///    template engine will build a single job. A wrong-network address never
///    silently mines: the pool boots into a "mining paused" state (so the
///    dashboard stays reachable to fix it) and starts the moment a valid
///    address is saved.
///  - `[pool] network` in config is an optional assertion: when set, boot
///    fails fast if the node reports a different chain — catching a config
///    pointed at the wrong node before any work is built.
///
/// The address starts from config.toml, may be overridden by a value
/// persisted in the stats SQLite (set via the dashboard), and is read by the
/// template engine on every GBT refresh.
use bitcoin::{address::NetworkUnchecked, Address, Network};
use parking_lot::RwLock;
use std::sync::Arc;
use tracing::warn;

struct AddressState {
    address: String,
    /// Whether `address` validates against `RuntimeSettings::network`.
    valid: bool,
}

pub struct RuntimeSettings {
    /// "mainnet" | "testnet" | "signet" | "regtest" — node-derived, immutable.
    network: String,
    state: RwLock<AddressState>,
}

impl RuntimeSettings {
    /// Build from config plus the node's reported chain
    /// (`getblockchaininfo` → "main" | "test" | "signet" | "regtest").
    ///
    /// Fails when the chain is unrecognized or contradicts a configured
    /// `[pool] network` assertion. An address that doesn't validate is NOT
    /// fatal — the pool boots with mining paused so the address can be fixed
    /// from the dashboard.
    pub fn new(
        pool_cfg: &crate::config::PoolConfig,
        node_chain: &str,
    ) -> anyhow::Result<Arc<Self>> {
        let network = match node_chain {
            "main" => "mainnet",
            "test" => "testnet",
            "signet" => "signet",
            "regtest" => "regtest",
            other => anyhow::bail!("node reports unrecognized chain \"{other}\""),
        };

        if let Some(asserted) = &pool_cfg.network {
            if asserted != network {
                anyhow::bail!(
                    "[pool] network = \"{asserted}\" but the connected node is on {network} — \
                     refusing to start. Point the pool at a {asserted} node, or fix/remove the \
                     network assertion in config.toml"
                );
            }
        }

        let valid = match validate(&pool_cfg.coinbase_address, network) {
            Ok(()) => true,
            Err(e) => {
                warn!(
                    "coinbase_address is not usable on {network}: {e} — MINING IS PAUSED \
                     until a valid address is set (dashboard → Settings)"
                );
                false
            }
        };

        Ok(Arc::new(Self {
            network: network.to_string(),
            state: RwLock::new(AddressState {
                address: pool_cfg.coinbase_address.clone(),
                valid,
            }),
        }))
    }

    /// Apply an address persisted from a previous dashboard save. Lenient: a
    /// stored address that no longer validates (e.g. the node was switched to
    /// a different chain since) is logged and skipped.
    pub fn apply_persisted(&self, address: Option<String>) {
        let Some(addr) = address else { return };
        match validate(&addr, &self.network) {
            Ok(()) => {
                *self.state.write() = AddressState {
                    address: addr,
                    valid: true,
                };
            }
            Err(e) => warn!(
                "Ignoring persisted payout address (not valid on {}): {e}",
                self.network
            ),
        }
    }

    /// Strictly validate and apply a dashboard-driven address change.
    pub fn update(&self, address: &str) -> Result<(), String> {
        validate(address, &self.network)?;
        *self.state.write() = AddressState {
            address: address.to_string(),
            valid: true,
        };
        Ok(())
    }

    /// The payout address — but only if it validates against the node's
    /// network. The template engine builds no jobs while this is `None`.
    pub fn valid_coinbase_address(&self) -> Option<String> {
        let s = self.state.read();
        s.valid.then(|| s.address.clone())
    }

    /// The configured address regardless of validity (for display).
    pub fn coinbase_address(&self) -> String {
        self.state.read().address.clone()
    }

    pub fn address_valid(&self) -> bool {
        self.state.read().valid
    }

    /// Node-derived network name: "mainnet" | "testnet" | "signet" | "regtest".
    pub fn network(&self) -> &str {
        &self.network
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
    let parsed: Address<NetworkUnchecked> = address.parse().map_err(|_| {
        "invalid address — check for typos (not parseable as a Bitcoin address)".to_string()
    })?;
    parsed
        .require_network(net)
        .map(|_| ())
        .map_err(|_| format!("address is not valid for {network} (the connected node's chain)"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PoolConfig;

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

    fn pool_cfg(address: &str, network_assert: Option<&str>) -> PoolConfig {
        PoolConfig {
            listen_addr: "127.0.0.1:0".into(),
            coinbase_address: address.into(),
            coinbase_tag: "/test/".into(),
            initial_difficulty: 1,
            extranonce1_size: 4,
            extranonce2_size: 4,
            max_connections: 8,
            idle_timeout_secs: 300,
            found_block_dir: "found-blocks".into(),
            network: network_assert.map(str::to_string),
        }
    }

    #[test]
    fn network_is_derived_from_node_chain() {
        let s = RuntimeSettings::new(&pool_cfg(&addr_for(Network::Testnet), None), "test").unwrap();
        assert_eq!(s.network(), "testnet");
        assert!(s.address_valid());
        assert!(RuntimeSettings::new(&pool_cfg("x", None), "weirdchain").is_err());
    }

    #[test]
    fn config_network_assertion_is_enforced() {
        let mainnet = addr_for(Network::Bitcoin);
        // Assertion matches node → boots.
        assert!(RuntimeSettings::new(&pool_cfg(&mainnet, Some("mainnet")), "main").is_ok());
        // Assertion contradicts node → fatal.
        assert!(RuntimeSettings::new(&pool_cfg(&mainnet, Some("testnet")), "main").is_err());
    }

    #[test]
    fn wrong_network_address_boots_paused_not_fatal() {
        let testnet = addr_for(Network::Testnet);
        let s = RuntimeSettings::new(&pool_cfg(&testnet, None), "main").unwrap();
        assert!(!s.address_valid());
        assert_eq!(s.valid_coinbase_address(), None);
        // Address is still shown for display.
        assert_eq!(s.coinbase_address(), testnet);
        // Fixing it via update resumes.
        let mainnet = addr_for(Network::Bitcoin);
        assert!(s.update(&mainnet).is_ok());
        assert_eq!(s.valid_coinbase_address(), Some(mainnet));
    }

    #[test]
    fn update_rejects_wrong_network_and_keeps_previous() {
        let mainnet = addr_for(Network::Bitcoin);
        let testnet = addr_for(Network::Testnet);
        let s = RuntimeSettings::new(&pool_cfg(&mainnet, None), "main").unwrap();
        let err = s.update(&testnet).unwrap_err();
        assert!(err.contains("not valid for mainnet"), "{err}");
        assert!(s.update("not-an-address").unwrap_err().contains("typos"));
        assert_eq!(s.valid_coinbase_address(), Some(mainnet));
    }

    #[test]
    fn testnet_address_validates_on_signet_node_too() {
        // Signet shares the tb1 HRP with testnet.
        let testnet = addr_for(Network::Testnet);
        let s = RuntimeSettings::new(&pool_cfg(&testnet, None), "signet").unwrap();
        assert!(s.address_valid());
    }

    #[test]
    fn persisted_address_is_lenient() {
        let mainnet = addr_for(Network::Bitcoin);
        let testnet = addr_for(Network::Testnet);
        let s = RuntimeSettings::new(&pool_cfg(&mainnet, None), "main").unwrap();
        // Persisted address from a previous run on another chain: ignored.
        s.apply_persisted(Some(testnet));
        assert_eq!(s.valid_coinbase_address(), Some(mainnet.clone()));
        s.apply_persisted(None);
        assert_eq!(s.valid_coinbase_address(), Some(mainnet));
    }
}
