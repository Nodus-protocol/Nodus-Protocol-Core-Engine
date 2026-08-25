//! Construct → simulate → validate: the Soroban transaction-preparation
//! pipeline for the pool contract's write functions.
//!
//! [`prepare`] is the only path that builds a transaction from scratch, and
//! it never hands one back without immediately re-deriving a
//! [`ReviewSummary`] from the exact bytes it is about to return (see
//! [`validate`]) — the response is never "trust me, here's what I built",
//! it's "here is what decoding this XDR actually says". [`validate`] is
//! also exposed standalone (see `api::pool::validate`) so a caller besides
//! this module — including an adversarial one — gets the same policy
//! checks run over any XDR it hands in, not just XDR this engine produced.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use stellar_xdr::curr::{OperationBody, ScVal, SorobanCredentials, TransactionEnvelope};

use crate::config::{Network, PoolConfig};
use crate::pool::abi::{self, AbiType, PoolFunction};
use crate::pool::soroban::SorobanRpc;
use crate::pool::xdr;
use crate::utils::EngineError;

// ── Public request/response shapes ──────────────────────────────────────────

pub struct PrepareParams {
    /// Caller-declared network, checked against the engine's own
    /// configured network before anything is built.
    pub network: Network,
    /// `G...` address that will sign and submit this transaction.
    pub source_account: String,
    /// Unix seconds. Also becomes the transaction's own `TimeBounds.max_time`,
    /// so an expired deadline is rejected by the network even if a caller
    /// somehow bypassed this engine's own check.
    pub deadline: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthSummary {
    /// `"source_account"` or `"address"` (`SorobanCredentialsType`).
    pub kind: &'static str,
    /// The authorizing address, for `"address"`-kind credentials.
    pub address: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewSummary {
    pub spec_hash: String,
    pub contract: String,
    pub function: String,
    pub args: serde_json::Value,
    pub source_account: String,
    pub sequence: i64,
    pub fee_stroops: u32,
    pub resource_fee_stroops: Option<i64>,
    pub deadline: u64,
    pub operation_count: usize,
    pub auth_entry_count: usize,
    pub auth: Vec<AuthSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PreparedTransaction {
    pub xdr: String,
    pub review: ReviewSummary,
}

/// What [`validate`] checks a decoded transaction against. `expected_sequence`
/// is optional because [`validate`]'s caller may not always have a fresh
/// on-chain read available (e.g. a pure decode-and-inspect call); when
/// absent, the sequence-freshness check is skipped rather than faked.
pub struct ValidateContext {
    pub network: Network,
    pub declared_network: Network,
    pub contract_id: String,
    pub fee_ceiling_stroops: u32,
    pub now: u64,
    pub expected_sequence: Option<i64>,
}

// ── Prepare ──────────────────────────────────────────────────────────────────

pub async fn prepare(
    rpc: &SorobanRpc,
    pool: &PoolConfig,
    engine_network: &Network,
    function: PoolFunction,
    args: Vec<ScVal>,
    params: PrepareParams,
) -> Result<PreparedTransaction, EngineError> {
    if params.network != *engine_network {
        return Err(EngineError::InvalidRequest(format!(
            "request declared network {:?} but this engine is configured for {:?}",
            params.network, engine_network
        )));
    }

    let now = unix_now();
    if params.deadline <= now {
        return Err(EngineError::PreconditionFailed(format!(
            "deadline {} is not in the future (engine clock: {now})",
            params.deadline
        )));
    }

    // Freshness: always read the account's current sequence right before
    // building, never trust a caller-supplied one.
    let account_key = xdr::account_ledger_key(&params.source_account)?;
    let entries = rpc.get_ledger_entries(vec![account_key]).await?;
    let entry = entries.first().ok_or_else(|| {
        EngineError::PreconditionFailed(format!(
            "source account {} not found on this network",
            params.source_account
        ))
    })?;
    let sequence = decode_account_sequence(&entry.xdr)?;

    let unsimulated = xdr::build_invoke_envelope(
        &pool.contract_id,
        function,
        args.clone(),
        &params.source_account,
        sequence,
        pool.base_fee_stroops,
        params.deadline,
    )?;
    let unsimulated_xdr = xdr::envelope_to_xdr(&unsimulated)?;

    let sim = rpc.simulate_transaction(&unsimulated_xdr).await?;

    if let Some(err) = sim.error {
        return Err(EngineError::PreconditionFailed(format!(
            "simulation failed: {err}"
        )));
    }
    if sim.restore_preamble.is_some() {
        // Fail closed rather than silently proceeding with stale entries or
        // guessing at a restoration transaction on the caller's behalf.
        return Err(EngineError::PreconditionFailed(
            "one or more ledger entries this call touches have expired and must be restored \
             (RestoreFootprint) before this transaction can be simulated"
                .into(),
        ));
    }
    let transaction_data_xdr = sim.transaction_data.ok_or_else(|| {
        EngineError::AdapterError("simulateTransaction returned no transactionData".into())
    })?;
    let min_resource_fee: i64 = sim
        .min_resource_fee
        .as_deref()
        .ok_or_else(|| {
            EngineError::AdapterError("simulateTransaction returned no minResourceFee".into())
        })?
        .parse()
        .map_err(|_| EngineError::AdapterError("minResourceFee is not a valid integer".into()))?;
    let auth_xdrs = sim
        .results
        .first()
        .map(|r| r.auth.clone())
        .unwrap_or_default();

    let soroban_data = xdr::decode_soroban_transaction_data(&transaction_data_xdr)?;
    let auth_entries = auth_xdrs
        .iter()
        .map(|a| xdr::decode_auth_entry(a))
        .collect::<Result<Vec<_>, _>>()?;

    let prepared = xdr::apply_simulation(
        unsimulated,
        soroban_data,
        auth_entries,
        min_resource_fee,
        pool.base_fee_stroops,
        pool.fee_ceiling_stroops,
    )?;
    let prepared_xdr = xdr::envelope_to_xdr(&prepared)?;

    // Self-check: re-derive the review summary from the exact bytes about
    // to be returned, through the same policy path `/validate` uses. This
    // is also where a future code path that let simulation mutate the
    // invoke args (rather than only add footprint/fee/auth) would be
    // caught — see the args-drift check inside `validate`.
    let review = validate(
        &prepared_xdr,
        &ValidateContext {
            network: *engine_network,
            declared_network: params.network,
            contract_id: pool.contract_id.clone(),
            fee_ceiling_stroops: pool.fee_ceiling_stroops,
            now,
            expected_sequence: Some(sequence),
        },
    )?;

    if review.args != args_to_json(function, &args) {
        return Err(EngineError::PreconditionFailed(
            "simulation altered the transaction's invocation arguments — refusing to return a \
             transaction whose effect differs from what was requested"
                .into(),
        ));
    }

    Ok(PreparedTransaction {
        xdr: prepared_xdr,
        review,
    })
}

// ── Validate ─────────────────────────────────────────────────────────────────

/// Decodes `xdr` and checks it against policy: known ABI function, exactly
/// one operation, correct contract, matching declared/engine network,
/// non-expired deadline, fee within ceiling, and (when `expected_sequence`
/// is known) a non-stale sequence number. Returns the [`ReviewSummary`]
/// derived from the decode on success.
pub fn validate(xdr: &str, ctx: &ValidateContext) -> Result<ReviewSummary, EngineError> {
    if ctx.declared_network != ctx.network {
        return Err(EngineError::InvalidRequest(format!(
            "declared network {:?} does not match this engine's configured network {:?}",
            ctx.declared_network, ctx.network
        )));
    }

    let envelope = xdr::envelope_from_xdr(xdr)?;
    let TransactionEnvelope::Tx(v1) = &envelope else {
        return Err(EngineError::InvalidRequest(
            "only TransactionV1Envelope (Tx) is accepted".into(),
        ));
    };
    let tx = &v1.tx;

    if tx.operations.len() != 1 {
        return Err(EngineError::InvalidRequest(format!(
            "expected exactly 1 operation, found {} — extra operations are rejected",
            tx.operations.len()
        )));
    }
    let OperationBody::InvokeHostFunction(invoke) = &tx.operations[0].body else {
        return Err(EngineError::InvalidRequest(
            "the operation is not an InvokeHostFunction call".into(),
        ));
    };
    let stellar_xdr::curr::HostFunction::InvokeContract(call) = &invoke.host_function else {
        return Err(EngineError::InvalidRequest(
            "only direct contract invocation is accepted (no contract/wasm creation)".into(),
        ));
    };

    let contract_str = xdr::address_to_string(&call.contract_address);
    if contract_str != ctx.contract_id {
        return Err(EngineError::InvalidRequest(format!(
            "transaction targets contract {contract_str}, expected {}",
            ctx.contract_id
        )));
    }

    let function_name = String::from_utf8_lossy(&call.function_name.0).to_string();
    let function = abi::require_known_function(&function_name, call.args.len())?;

    let args: Vec<ScVal> = call.args.to_vec();
    let args_json = args_to_json(function, &args);

    // Every function in the manifest declares its deadline param by name;
    // look it up rather than assuming a position, so a future ABI change
    // that reorders parameters can't silently read the wrong argument.
    let deadline_index = function
        .params()
        .iter()
        .position(|p| p.name == "deadline")
        .ok_or_else(|| {
            EngineError::Internal(format!(
                "{} has no 'deadline' parameter in the manifest",
                function.wire_name()
            ))
        })?;
    let deadline = xdr::scval_to_u64(args.get(deadline_index).ok_or_else(|| {
        EngineError::Internal("argument count did not match the manifest".into())
    })?)?;
    if deadline <= ctx.now {
        return Err(EngineError::PreconditionFailed(format!(
            "deadline {deadline} has expired (as of {})",
            ctx.now
        )));
    }

    if tx.fee > ctx.fee_ceiling_stroops {
        return Err(EngineError::PreconditionFailed(format!(
            "fee {} stroops exceeds configured ceiling of {} stroops",
            tx.fee, ctx.fee_ceiling_stroops
        )));
    }

    if let Some(expected_seq) = ctx.expected_sequence {
        if tx.seq_num.0 != expected_seq + 1 {
            return Err(EngineError::PreconditionFailed(format!(
                "transaction sequence {} does not follow the current account sequence {} — \
                 stale or replayed transaction",
                tx.seq_num.0, expected_seq
            )));
        }
    }

    let source_account = xdr::account_id_to_string(&match &tx.source_account {
        stellar_xdr::curr::MuxedAccount::Ed25519(key) => stellar_xdr::curr::AccountId(
            stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(key.clone()),
        ),
        stellar_xdr::curr::MuxedAccount::MuxedEd25519(m) => stellar_xdr::curr::AccountId(
            stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(m.ed25519.clone()),
        ),
    });

    let resource_fee_stroops = match &tx.ext {
        stellar_xdr::curr::TransactionExt::V1(data) => Some(data.resource_fee),
        stellar_xdr::curr::TransactionExt::V0 => None,
    };

    let auth: Vec<AuthSummary> = invoke
        .auth
        .iter()
        .map(|entry| match &entry.credentials {
            SorobanCredentials::SourceAccount => AuthSummary {
                kind: "source_account",
                address: None,
            },
            SorobanCredentials::Address(addr_creds) => AuthSummary {
                kind: "address",
                address: Some(xdr::address_to_string(&addr_creds.address)),
            },
        })
        .collect();

    Ok(ReviewSummary {
        spec_hash: abi::spec_hash().to_string(),
        contract: contract_str,
        function: function.wire_name().to_string(),
        args: args_json,
        source_account,
        sequence: tx.seq_num.0,
        fee_stroops: tx.fee,
        resource_fee_stroops,
        deadline,
        operation_count: tx.operations.len(),
        auth_entry_count: invoke.auth.len(),
        auth,
    })
}

// ── Decoding helpers ─────────────────────────────────────────────────────────

pub(crate) fn decode_account_sequence(entry_data_xdr: &str) -> Result<i64, EngineError> {
    use stellar_xdr::curr::{LedgerEntryData, Limits, ReadXdr};
    let data = LedgerEntryData::from_xdr_base64(entry_data_xdr, Limits::none())
        .map_err(|e| EngineError::AdapterError(format!("malformed account ledger entry: {e}")))?;
    match data {
        LedgerEntryData::Account(acc) => Ok(acc.seq_num.0),
        other => Err(EngineError::Internal(format!(
            "expected Account ledger entry, got {other:?}"
        ))),
    }
}

fn args_to_json(function: PoolFunction, args: &[ScVal]) -> serde_json::Value {
    let params = function.params();
    let mut map = serde_json::Map::new();
    for (i, val) in args.iter().enumerate() {
        let key = params
            .get(i)
            .map(|p| p.name.to_string())
            .unwrap_or_else(|| format!("arg_{i}"));
        let ty = params.get(i).map(|p| p.ty);
        map.insert(key, scval_to_json(val, ty));
    }
    serde_json::Value::Object(map)
}

fn scval_to_json(val: &ScVal, expected: Option<AbiType>) -> serde_json::Value {
    match expected {
        Some(AbiType::Address) => match val {
            ScVal::Address(addr) => serde_json::Value::String(xdr::address_to_string(addr)),
            other => serde_json::Value::String(format!("{other:?}")),
        },
        Some(AbiType::I128) => xdr::scval_to_i128(val)
            .map(|v| serde_json::Value::String(v.to_string()))
            .unwrap_or_else(|_| serde_json::Value::String(format!("{val:?}"))),
        Some(AbiType::U64) => xdr::scval_to_u64(val)
            .map(|v| serde_json::Value::Number(v.into()))
            .unwrap_or_else(|_| serde_json::Value::String(format!("{val:?}"))),
        None => serde_json::Value::String(format!("{val:?}")),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Encodes a pool function call's arguments in the manifest's declared
/// order, converting the engine's `u128` amounts into the contract's `i128`.
pub fn encode_swap_args(
    to: &str,
    amount_0_out: u128,
    amount_1_out: u128,
    deadline: u64,
) -> Result<Vec<ScVal>, EngineError> {
    Ok(vec![
        ScVal::Address(xdr::parse_address(to)?),
        xdr::i128_to_scval(xdr::u128_to_positive_i128(amount_0_out, "amount_0_out")?),
        xdr::i128_to_scval(xdr::u128_to_positive_i128(amount_1_out, "amount_1_out")?),
        ScVal::U64(deadline),
    ])
}

#[allow(clippy::too_many_arguments)]
pub fn encode_add_liquidity_args(
    from: &str,
    to: &str,
    amount_0_desired: u128,
    amount_1_desired: u128,
    amount_0_min: u128,
    amount_1_min: u128,
    deadline: u64,
) -> Result<Vec<ScVal>, EngineError> {
    Ok(vec![
        ScVal::Address(xdr::parse_address(from)?),
        ScVal::Address(xdr::parse_address(to)?),
        xdr::i128_to_scval(xdr::u128_to_positive_i128(
            amount_0_desired,
            "amount_0_desired",
        )?),
        xdr::i128_to_scval(xdr::u128_to_positive_i128(
            amount_1_desired,
            "amount_1_desired",
        )?),
        xdr::i128_to_scval(xdr::u128_to_positive_i128(amount_0_min, "amount_0_min")?),
        xdr::i128_to_scval(xdr::u128_to_positive_i128(amount_1_min, "amount_1_min")?),
        ScVal::U64(deadline),
    ])
}

pub fn encode_remove_liquidity_args(
    from: &str,
    to: &str,
    liquidity: u128,
    amount_0_min: u128,
    amount_1_min: u128,
    deadline: u64,
) -> Result<Vec<ScVal>, EngineError> {
    Ok(vec![
        ScVal::Address(xdr::parse_address(from)?),
        ScVal::Address(xdr::parse_address(to)?),
        xdr::i128_to_scval(xdr::u128_to_positive_i128(liquidity, "liquidity")?),
        xdr::i128_to_scval(xdr::u128_to_positive_i128(amount_0_min, "amount_0_min")?),
        xdr::i128_to_scval(xdr::u128_to_positive_i128(amount_1_min, "amount_1_min")?),
        ScVal::U64(deadline),
    ])
}
