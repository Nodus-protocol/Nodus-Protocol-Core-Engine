use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("payment not found: {0}")]
    NotFound(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("chain adapter error: {0}")]
    AdapterError(String),
    #[error("network error: {0}")]
    NetworkError(String),
    #[error("internal error: {0}")]
    Internal(String),
    /// A request that is well-formed but fails a policy precondition tied to
    /// current chain/ledger state: stale sequence number, expired deadline,
    /// a fee above the configured ceiling, or state that requires ledger
    /// entry restoration before it can be simulated.
    #[error("precondition failed: {0}")]
    PreconditionFailed(String),
}

impl EngineError {
    pub fn http_status(&self) -> u16 {
        match self {
            EngineError::NotFound(_) => 404,
            EngineError::InvalidRequest(_) => 400,
            EngineError::Conflict(_) => 409,
            EngineError::PreconditionFailed(_) => 412,
            _ => 500,
        }
    }
}

/// Canonical asset type discriminant. Determines which fields are required
/// and how the asset is addressed on-chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetType {
    /// XLM native asset — no issuer, no contract address.
    Native,
    /// Classic Stellar credit asset (pre-CAP-0046 SEP-41 token issued via
    /// the classic `ChangeTrust` / `Payment` operations). Requires
    /// `issuer` to be set; `contract` is optional (the SAC address when
    /// wrapped).
    IssuedAsset,
    /// Soroban-native token deployed as a smart contract. `contract` holds
    /// the `C…` contract address; `issuer` is absent.
    ContractToken,
}

/// A fully-qualified, canonical asset identity. Symbol / display name is
/// purely cosmetic and is never used for routing, validation, or pool
/// selection — the canonical identifier is the `(network, asset_type,
/// contract/issuer)` triple.
///
/// Serialises to / deserialises from a flat JSON object so callers can
/// include it inline in request bodies or query parameters without nesting.
///
/// ```json
/// // Native XLM
/// { "network": "mainnet", "asset_type": "native", "symbol": "XLM", "decimals": 7 }
///
/// // USDC on mainnet (classic issued asset wrapped as SAC)
/// {
///   "network": "mainnet",
///   "asset_type": "issued_asset",
///   "symbol": "USDC",
///   "decimals": 7,
///   "issuer": "GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN",
///   "contract": "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75"
/// }
///
/// // A Soroban-native token
/// {
///   "network": "testnet",
///   "asset_type": "contract_token",
///   "symbol": "MYTKN",
///   "decimals": 6,
///   "contract": "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC"
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetId {
    /// Which Stellar network this asset lives on.
    pub network: String,
    /// Asset type — governs which other fields are meaningful.
    pub asset_type: AssetType,
    /// Human-readable ticker. Display-only; not used for equality or routing.
    pub symbol: String,
    /// Decimal precision of the asset's base unit. XLM and most SAC tokens
    /// use 7; some Soroban tokens use different values.
    pub decimals: u8,
    /// For `IssuedAsset` and SAC-wrapped tokens: the Stellar classic issuer
    /// `G…` account. `None` for native and bare contract tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    /// For `ContractToken` and SAC-wrapped `IssuedAsset`: the `C…` Soroban
    /// contract address used to address the token in contract calls.
    /// `None` for the native asset when not wrapped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract: Option<String>,
}

impl AssetId {
    /// Returns the canonical routing key: the contract address when present,
    /// otherwise the `issuer:symbol` pair for classic assets, or `"native"`
    /// for XLM. This value — not the symbol — is used for pool-side token
    /// direction matching and cache keying.
    pub fn canonical_key(&self) -> String {
        if let Some(ref c) = self.contract {
            return c.clone();
        }
        match self.asset_type {
            AssetType::Native => "native".to_string(),
            AssetType::IssuedAsset => {
                let issuer = self.issuer.as_deref().unwrap_or("");
                format!("{}:{}", issuer, self.symbol)
            }
            AssetType::ContractToken => {
                // contract should always be set for ContractToken but guard
                // against malformed instances at runtime.
                format!("contract:{}", self.symbol)
            }
        }
    }

    /// Build a native XLM AssetId for a given network.
    pub fn native(network: &str) -> Self {
        AssetId {
            network: network.to_string(),
            asset_type: AssetType::Native,
            symbol: "XLM".to_string(),
            decimals: 7,
            issuer: None,
            contract: None,
        }
    }
}

/// An exact price or exchange rate represented as an integer rational
/// `numerator / denominator`. Both values are serialised as decimal strings
/// to avoid any JSON floating-point encoding loss.
///
/// Convention: `effective_price` in a quote is `amount_in / amount_out`,
/// i.e. "how many input base-units per one output base-unit".  This matches
/// audited contract behaviour and is independent of token decimal scale —
/// callers that want a human-readable price must scale by `decimals`
/// themselves.
///
/// `denominator` is always ≥ 1.  When `denominator == 1` the price is an
/// exact integer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RationalPrice {
    /// Numerator as a decimal string (avoids i64 overflow for u128 values).
    pub numerator: String,
    /// Denominator as a decimal string.  Never zero.
    pub denominator: String,
}

impl RationalPrice {
    pub fn new(numerator: u128, denominator: u128) -> Self {
        assert!(denominator > 0, "RationalPrice denominator must be non-zero");
        // Reduce by GCD for canonical representation.
        let g = gcd(numerator, denominator);
        RationalPrice {
            numerator: (numerator / g).to_string(),
            denominator: (denominator / g).to_string(),
        }
    }

    /// Convenience: price is zero (e.g. zero output amount).
    pub fn zero() -> Self {
        RationalPrice {
            numerator: "0".to_string(),
            denominator: "1".to_string(),
        }
    }
}

fn gcd(a: u128, b: u128) -> u128 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentStatus {
    Pending,
    Processing,
    Confirmed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Urgency {
    #[default]
    Standard,
    Fast,
    Urgent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Payment {
    pub id: String,
    pub sender: String,
    pub recipient: String,
    /// Amount in integer base units of `asset`.
    pub amount: u64,
    /// Fully-qualified canonical asset identity. Replaces the old bare
    /// `token: String` field.
    pub asset: AssetId,
    pub status: PaymentStatus,
    pub tx_hash: Option<String>,
    pub fee_stroops: u64,
    pub urgency: Urgency,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeEstimate {
    pub standard_stroops: u64,
    pub fast_stroops: u64,
    pub urgent_stroops: u64,
    pub standard_seconds: u32,
    pub fast_seconds: u32,
    pub urgent_seconds: u32,
}

impl Default for FeeEstimate {
    fn default() -> Self {
        Self {
            standard_stroops: 100,
            fast_stroops: 250,
            urgent_stroops: 500,
            standard_seconds: 5,
            fast_seconds: 3,
            urgent_seconds: 1,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub code: &'static str,
    pub message: String,
}

pub fn now_utc() -> String {
    chrono::Utc::now().to_rfc3339()
}
