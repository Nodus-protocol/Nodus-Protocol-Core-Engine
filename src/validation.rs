use crate::utils::{AssetId, AssetType, EngineError};

/// Validates a Stellar classic account address (G… strkey, 56 chars).
pub fn stellar_address(addr: &str) -> Result<(), EngineError> {
    if addr.len() == 56 && addr.starts_with('G') && addr.chars().all(|c| c.is_ascii_alphanumeric())
    {
        Ok(())
    } else {
        Err(EngineError::InvalidRequest(format!(
            "invalid Stellar address '{addr}': must be 56 alphanumeric chars starting with G"
        )))
    }
}

/// Validates a Soroban contract address (C… strkey, 56 chars).
pub fn contract_address(addr: &str) -> Result<(), EngineError> {
    if addr.len() == 56 && addr.starts_with('C') && addr.chars().all(|c| c.is_ascii_alphanumeric())
    {
        Ok(())
    } else {
        Err(EngineError::InvalidRequest(format!(
            "invalid contract address '{addr}': must be 56 alphanumeric chars starting with C"
        )))
    }
}

/// Validates that `amount` is strictly positive.
pub fn amount(amount: u64) -> Result<(), EngineError> {
    if amount == 0 {
        Err(EngineError::InvalidRequest(
            "amount must be greater than 0".into(),
        ))
    } else {
        Ok(())
    }
}

/// Validates a network string ("mainnet" or "testnet").
pub fn network(net: &str) -> Result<(), EngineError> {
    match net {
        "mainnet" | "testnet" => Ok(()),
        other => Err(EngineError::InvalidRequest(format!(
            "network must be 'mainnet' or 'testnet', got '{other}'"
        ))),
    }
}

/// Validates a symbol string: non-empty, ≤12 ASCII alphanumeric chars.
/// Symbol validation is kept intentionally lightweight — the symbol is
/// display metadata only and is never used for routing or pool selection.
pub fn symbol(sym: &str) -> Result<(), EngineError> {
    if sym.is_empty() || sym.len() > 12 || !sym.chars().all(|c| c.is_ascii_alphanumeric()) {
        Err(EngineError::InvalidRequest(format!(
            "invalid token symbol '{sym}': must be 1-12 ASCII alphanumeric chars"
        )))
    } else {
        Ok(())
    }
}

/// Full canonical AssetId validation.
///
/// Rules enforced:
/// 1. `network` must be "mainnet" or "testnet".
/// 2. `symbol` must pass the display-metadata check.
/// 3. `decimals` must be ≤ 19 (u64 base-unit cap for Stellar amounts).
/// 4. Type-specific field coherence:
///    - `Native`:        no issuer, no contract required.
///    - `IssuedAsset`:   `issuer` required and must be a valid G… address;
///                       `contract`, if present, must be a valid C… address.
///    - `ContractToken`: `contract` required and must be a valid C… address;
///                       `issuer` must be absent (prevents attaching a
///                       classic issuer to a Soroban-only token, which is
///                       how same-symbol impostors sneak in).
/// 5. Network coherence: testnet assets must not carry mainnet contract
///    addresses (we cannot verify the chain from the string alone, but we
///    reject any asset whose `network` field disagrees with the engine's
///    configured network at the call sites that matter).
///
/// This function does NOT check whether the contract/issuer is actually
/// deployed or registered — that is the job of the asset registry and the
/// pool's verified address list.
pub fn asset_id(asset: &AssetId) -> Result<(), EngineError> {
    network(&asset.network)?;
    symbol(&asset.symbol)?;

    if asset.decimals > 19 {
        return Err(EngineError::InvalidRequest(format!(
            "asset '{}': decimals {} exceeds maximum of 19",
            asset.symbol, asset.decimals
        )));
    }

    match asset.asset_type {
        AssetType::Native => {
            if asset.issuer.is_some() {
                return Err(EngineError::InvalidRequest(format!(
                    "native asset '{}' must not carry an issuer",
                    asset.symbol
                )));
            }
            // contract is allowed (SAC address for wrapped XLM) but not required.
            if let Some(ref c) = asset.contract {
                contract_address(c).map_err(|_| {
                    EngineError::InvalidRequest(format!(
                        "native asset '{}': invalid contract address '{c}'",
                        asset.symbol
                    ))
                })?;
            }
        }

        AssetType::IssuedAsset => {
            match &asset.issuer {
                None => {
                    return Err(EngineError::InvalidRequest(format!(
                        "issued asset '{}' requires an issuer address",
                        asset.symbol
                    )));
                }
                Some(iss) => {
                    stellar_address(iss).map_err(|_| {
                        EngineError::InvalidRequest(format!(
                            "issued asset '{}': invalid issuer address '{iss}'",
                            asset.symbol
                        ))
                    })?;
                }
            }
            // Optional SAC wrapper contract address.
            if let Some(ref c) = asset.contract {
                contract_address(c).map_err(|_| {
                    EngineError::InvalidRequest(format!(
                        "issued asset '{}': invalid contract address '{c}'",
                        asset.symbol
                    ))
                })?;
            }
        }

        AssetType::ContractToken => {
            match &asset.contract {
                None => {
                    return Err(EngineError::InvalidRequest(format!(
                        "contract token '{}' requires a contract address",
                        asset.symbol
                    )));
                }
                Some(c) => {
                    contract_address(c).map_err(|_| {
                        EngineError::InvalidRequest(format!(
                            "contract token '{}': invalid contract address '{c}'",
                            asset.symbol
                        ))
                    })?;
                }
            }
            // A Soroban-native token must NOT have a classic issuer — this is
            // the primary guard against same-symbol impostor tokens where an
            // attacker attaches a well-known ticker (e.g. "USDC") to an
            // unrelated contract by also setting a plausible-looking issuer.
            if asset.issuer.is_some() {
                return Err(EngineError::InvalidRequest(format!(
                    "contract token '{}' must not carry an issuer (use asset_type \
                     'issued_asset' with a contract field for SAC-wrapped tokens)",
                    asset.symbol
                )));
            }
        }
    }

    Ok(())
}

/// Checks that `asset.network` matches the engine's configured network.
/// Rejects cross-network assets before they reach simulation or quote logic.
pub fn asset_network(asset: &AssetId, engine_network: &str) -> Result<(), EngineError> {
    if asset.network != engine_network {
        return Err(EngineError::InvalidRequest(format!(
            "asset '{}' belongs to network '{}' but this engine is configured \
             for '{engine_network}'",
            asset.symbol, asset.network
        )));
    }
    Ok(())
}

/// Checks that a pool's verified token addresses contain the requested token.
/// `pool_tokens` should be the slice of canonical keys from the pool config
/// (contract addresses or "native"). Returns the matching canonical key or
/// an error if the token is not in the pool.
pub fn token_in_pool<'a>(asset: &AssetId, pool_tokens: &[&'a str]) -> Result<&'a str, EngineError> {
    let key = asset.canonical_key();
    pool_tokens
        .iter()
        .copied()
        .find(|&t| t == key)
        .ok_or_else(|| {
            EngineError::InvalidRequest(format!(
                "asset '{}' (key: '{key}') is not in this pool",
                asset.symbol
            ))
        })
}

// ─── Legacy symbol-only validator (kept for backward-compat call sites that
//     haven't been migrated yet — marked deprecated so clippy warns on new
//     uses). ──────────────────────────────────────────────────────────────────

/// Validates a bare token symbol string.
/// **Deprecated** — use [`asset_id`] with a full [`AssetId`] instead.
/// Symbols alone cannot identify an asset and will be removed in a future
/// release.
#[deprecated(
    since = "0.2.0",
    note = "use asset_id() with a full AssetId; bare symbols cannot prevent impostor tokens"
)]
#[allow(dead_code)]
pub fn token(token: &str) -> Result<(), EngineError> {
    symbol(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::{AssetId, AssetType};

    fn usdc_mainnet() -> AssetId {
        AssetId {
            network: "mainnet".into(),
            asset_type: AssetType::IssuedAsset,
            symbol: "USDC".into(),
            decimals: 7,
            issuer: Some("GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN".into()),
            contract: Some("CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75".into()),
        }
    }

    fn xlm_native() -> AssetId {
        AssetId {
            network: "mainnet".into(),
            asset_type: AssetType::Native,
            symbol: "XLM".into(),
            decimals: 7,
            issuer: None,
            contract: None,
        }
    }

    fn contract_token() -> AssetId {
        AssetId {
            network: "testnet".into(),
            asset_type: AssetType::ContractToken,
            symbol: "MYTKN".into(),
            decimals: 6,
            issuer: None,
            contract: Some("CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC".into()),
        }
    }

    #[test]
    fn valid_stellar_address() {
        let addr = "GAHJJJKMOKYE4RVPZEWZTKH5FVI4PA3VL7GK2LFNUBSGBV7REEX6XCLD";
        assert!(stellar_address(addr).is_ok());
    }

    #[test]
    fn rejects_short_address() {
        assert!(stellar_address("GABC123").is_err());
    }

    #[test]
    fn rejects_wrong_prefix() {
        let addr = "XAHJJJKMOKYE4RVPZEWZTKH5FVI4PA3VL7GK2LFNUBSGBV7REEX6XCLD";
        assert!(stellar_address(addr).is_err());
    }

    #[test]
    fn rejects_zero_amount() {
        assert!(amount(0).is_err());
    }

    #[test]
    fn accepts_nonzero_amount() {
        assert!(amount(1).is_ok());
        assert!(amount(u64::MAX).is_ok());
    }

    #[test]
    fn valid_native_xlm() {
        assert!(asset_id(&xlm_native()).is_ok());
    }

    #[test]
    fn valid_issued_usdc() {
        assert!(asset_id(&usdc_mainnet()).is_ok());
    }

    #[test]
    fn valid_contract_token() {
        assert!(asset_id(&contract_token()).is_ok());
    }

    #[test]
    fn native_with_issuer_rejected() {
        let mut a = xlm_native();
        a.issuer = Some("GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN".into());
        assert!(asset_id(&a).is_err());
    }

    #[test]
    fn issued_asset_missing_issuer_rejected() {
        let mut a = usdc_mainnet();
        a.issuer = None;
        assert!(asset_id(&a).is_err());
    }

    #[test]
    fn issued_asset_bad_issuer_rejected() {
        let mut a = usdc_mainnet();
        a.issuer = Some("not-an-address".into());
        assert!(asset_id(&a).is_err());
    }

    #[test]
    fn contract_token_missing_contract_rejected() {
        let mut a = contract_token();
        a.contract = None;
        assert!(asset_id(&a).is_err());
    }

    #[test]
    fn contract_token_with_issuer_rejected_impostor_guard() {
        // This is the key impostor-token guard: a "USDC" contract token that
        // also claims a classic issuer should be rejected.
        let mut a = contract_token();
        a.issuer = Some("GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN".into());
        assert!(asset_id(&a).is_err());
    }

    #[test]
    fn wrong_network_rejected() {
        let a = usdc_mainnet();
        assert!(asset_network(&a, "testnet").is_err());
        assert!(asset_network(&a, "mainnet").is_ok());
    }

    #[test]
    fn invalid_decimals_rejected() {
        let mut a = xlm_native();
        a.decimals = 20;
        assert!(asset_id(&a).is_err());
    }

    #[test]
    fn token_in_pool_matches_canonical_key() {
        let usdc = usdc_mainnet();
        let key = usdc.canonical_key();
        let pool = [key.as_str(), "native"];
        assert!(token_in_pool(&usdc, &pool).is_ok());
    }

    #[test]
    fn token_not_in_pool_rejected() {
        let xlm = xlm_native();
        let pool = ["CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75"];
        assert!(token_in_pool(&xlm, &pool).is_err());
    }
}
