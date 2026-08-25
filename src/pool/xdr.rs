//! Typed Soroban XDR construction and decoding.
//!
//! Every value here goes through `stellar-xdr` — the Stellar Development
//! Foundation's official generated XDR bindings — rather than the
//! hand-rolled byte buffers this module replaces. Encoding a contract call
//! through real `ScVal`/`Operation`/`Transaction` types means a malformed
//! argument is a type error or an `Err` here, not a subtly-wrong buffer
//! that only fails once it reaches Soroban RPC or the network.

use stellar_xdr::curr::{
    AccountId, ContractId, Hash, HostFunction, Int128Parts, InvokeContractArgs,
    InvokeHostFunctionOp, LedgerKey, LedgerKeyAccount, LedgerKeyContractData, Limits, Memo,
    MuxedAccount, Operation, OperationBody, Preconditions, PublicKey, ScAddress, ScSymbol, ScVal,
    SequenceNumber, SorobanAuthorizationEntry, SorobanTransactionData, StringM, TimeBounds,
    TimePoint, Transaction, TransactionEnvelope, TransactionExt, TransactionV1Envelope, Uint256,
    VecM, WriteXdr,
};

use crate::pool::abi::PoolFunction;
use crate::utils::EngineError;

fn xdr_err(context: &str, e: impl std::fmt::Display) -> EngineError {
    EngineError::Internal(format!("xdr: {context}: {e}"))
}

// ── Addresses ────────────────────────────────────────────────────────────────

/// Parses a Stellar strkey (`G...` account or `C...` contract) into a typed
/// `ScAddress`. Rejects anything that isn't a valid, correctly-checksummed
/// strkey of one of those two kinds — no silent fallback to raw bytes.
pub fn parse_address(strkey: &str) -> Result<ScAddress, EngineError> {
    if let Ok(contract) = stellar_strkey::Contract::from_string(strkey) {
        return Ok(ScAddress::Contract(ContractId(Hash(contract.0))));
    }
    if let Ok(account) = stellar_strkey::ed25519::PublicKey::from_string(strkey) {
        return Ok(ScAddress::Account(AccountId(
            PublicKey::PublicKeyTypeEd25519(Uint256(account.0)),
        )));
    }
    Err(EngineError::InvalidRequest(format!(
        "'{strkey}' is not a valid Stellar account (G...) or contract (C...) address"
    )))
}

/// Parses a `G...` strkey specifically — used where an operation must be a
/// funded account (the transaction source) rather than any `Address`.
pub fn parse_account(strkey: &str) -> Result<AccountId, EngineError> {
    let account = stellar_strkey::ed25519::PublicKey::from_string(strkey).map_err(|_| {
        EngineError::InvalidRequest(format!("'{strkey}' is not a valid account (G...) address"))
    })?;
    Ok(AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
        account.0,
    ))))
}

pub fn address_to_string(addr: &ScAddress) -> String {
    match addr {
        ScAddress::Contract(ContractId(Hash(bytes))) => {
            stellar_strkey::Contract(*bytes).to_string()
        }
        ScAddress::Account(AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes)))) => {
            stellar_strkey::ed25519::PublicKey(*bytes).to_string()
        }
        // MuxedAccount / ClaimableBalance / LiquidityPool addresses never
        // appear in this contract's ABI; render a diagnostic instead of
        // panicking if one somehow shows up while decoding foreign XDR.
        other => format!("{other:?}"),
    }
}

pub fn account_id_to_string(id: &AccountId) -> String {
    address_to_string(&ScAddress::Account(id.clone()))
}

// ── Scalars ──────────────────────────────────────────────────────────────────

pub fn i128_to_scval(v: i128) -> ScVal {
    let bits = v as u128;
    ScVal::I128(Int128Parts {
        hi: (bits >> 64) as i64,
        lo: bits as u64,
    })
}

pub fn scval_to_i128(v: &ScVal) -> Result<i128, EngineError> {
    match v {
        ScVal::I128(Int128Parts { hi, lo }) => {
            let bits = ((*hi as u64 as u128) << 64) | (*lo as u128);
            Ok(bits as i128)
        }
        other => Err(EngineError::Internal(format!(
            "expected ScVal::I128, got {other:?}"
        ))),
    }
}

pub fn scval_to_u64(v: &ScVal) -> Result<u64, EngineError> {
    match v {
        ScVal::U64(n) => Ok(*n),
        other => Err(EngineError::Internal(format!(
            "expected ScVal::U64, got {other:?}"
        ))),
    }
}

/// Companion to [`scval_to_i128`] and [`scval_to_u64`] for `ScVal::Address`
/// decoding. Not called anywhere yet — the review summary decodes
/// addresses inline via [`address_to_string`] instead — but kept as part of
/// this module's typed-decode surface (matching the `i128`/`u64` pair)
/// rather than deleted, hence the narrow allow.
#[allow(dead_code)]
pub fn scval_to_address(v: &ScVal) -> Result<ScAddress, EngineError> {
    match v {
        ScVal::Address(addr) => Ok(addr.clone()),
        other => Err(EngineError::Internal(format!(
            "expected ScVal::Address, got {other:?}"
        ))),
    }
}

fn symbol(name: &str) -> Result<ScSymbol, EngineError> {
    Ok(ScSymbol(
        StringM::try_from(name).map_err(|e| xdr_err("symbol", e))?,
    ))
}

/// A u128 amount that must fit in the contract's `i128` parameter without
/// changing sign or magnitude. The AMM never uses negative amounts, but the
/// engine's math types are `u128`; this is the single point where that gets
/// reconciled with what the chain actually accepts.
pub fn u128_to_positive_i128(amount: u128, field: &str) -> Result<i128, EngineError> {
    i128::try_from(amount)
        .map_err(|_| EngineError::InvalidRequest(format!("{field} ({amount}) exceeds i128::MAX")))
}

// ── Ledger keys (for getLedgerEntries) ──────────────────────────────────────

pub fn account_ledger_key(account: &str) -> Result<String, EngineError> {
    let account_id = parse_account(account)?;
    let key = LedgerKey::Account(LedgerKeyAccount { account_id });
    key.to_xdr_base64(Limits::none())
        .map_err(|e| xdr_err("account ledger key", e))
}

pub fn contract_instance_ledger_key(contract: &str) -> Result<String, EngineError> {
    let address = parse_address(contract)?;
    let key = LedgerKey::ContractData(LedgerKeyContractData {
        contract: address,
        key: ScVal::LedgerKeyContractInstance,
        durability: stellar_xdr::curr::ContractDataDurability::Persistent,
    });
    key.to_xdr_base64(Limits::none())
        .map_err(|e| xdr_err("contract instance ledger key", e))
}

// ── Transaction envelope construction ───────────────────────────────────────

/// Builds the unsigned, unsimulated `InvokeHostFunction` envelope for a
/// single pool contract call. `sequence` is the source account's *current*
/// on-chain sequence number (as read from `getLedgerEntries`); this sets
/// `seq_num = sequence + 1`, per Stellar's sequence number semantics.
#[allow(clippy::too_many_arguments)]
pub fn build_invoke_envelope(
    contract: &str,
    function: PoolFunction,
    args: Vec<ScVal>,
    source_account: &str,
    sequence: i64,
    base_fee: u32,
    deadline: u64,
) -> Result<TransactionEnvelope, EngineError> {
    let contract_address = parse_address(contract)?;
    let source = parse_account(source_account)?;
    let AccountId(PublicKey::PublicKeyTypeEd25519(source_key)) = source;

    let invoke_args = InvokeContractArgs {
        contract_address,
        function_name: symbol(function.wire_name())?,
        args: VecM::try_from(args).map_err(|e| xdr_err("args", e))?,
    };

    let op = Operation {
        source_account: None,
        body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            host_function: HostFunction::InvokeContract(invoke_args),
            auth: VecM::default(),
        }),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(source_key),
        fee: base_fee,
        seq_num: SequenceNumber(
            sequence
                .checked_add(1)
                .ok_or_else(|| EngineError::Internal("sequence number overflow".into()))?,
        ),
        cond: Preconditions::Time(TimeBounds {
            min_time: TimePoint(0),
            max_time: TimePoint(deadline),
        }),
        memo: Memo::None,
        operations: VecM::try_from(vec![op]).map_err(|e| xdr_err("operations", e))?,
        ext: TransactionExt::V0,
    };

    Ok(TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    }))
}

/// Applies Soroban RPC `simulateTransaction` results to an unsimulated
/// envelope: attaches the resource footprint/data, the authorization
/// entries Soroban determined are required, and bumps the fee by the
/// simulated resource fee. Rejects if the resulting fee exceeds
/// `fee_ceiling`, or if the envelope isn't the single-operation shape this
/// module builds (defense in depth against a future code path handing this
/// function something unexpected).
pub fn apply_simulation(
    envelope: TransactionEnvelope,
    soroban_data: SorobanTransactionData,
    auth: Vec<SorobanAuthorizationEntry>,
    resource_fee: i64,
    base_fee: u32,
    fee_ceiling: u32,
) -> Result<TransactionEnvelope, EngineError> {
    let TransactionEnvelope::Tx(mut v1) = envelope else {
        return Err(EngineError::Internal(
            "apply_simulation: expected a TransactionEnvelope::Tx".into(),
        ));
    };

    // VecM only implements `Deref` (not `DerefMut`), so the single
    // operation is taken out, mutated by value, and put back — rather than
    // indexed in place.
    let mut ops: Vec<Operation> = v1.tx.operations.into();
    if ops.len() != 1 {
        return Err(EngineError::Internal(
            "apply_simulation: expected exactly one operation".into(),
        ));
    }
    let mut op = ops.remove(0);
    let OperationBody::InvokeHostFunction(ref mut invoke) = op.body else {
        return Err(EngineError::Internal(
            "apply_simulation: operation is not InvokeHostFunction".into(),
        ));
    };
    invoke.auth = VecM::try_from(auth).map_err(|e| xdr_err("auth entries", e))?;

    let total_fee = i64::from(base_fee)
        .checked_add(resource_fee)
        .ok_or_else(|| EngineError::Internal("fee overflow".into()))?;
    if total_fee < 0 || total_fee > i64::from(fee_ceiling) {
        return Err(EngineError::PreconditionFailed(format!(
            "prepared fee {total_fee} stroops exceeds configured ceiling of {fee_ceiling} stroops"
        )));
    }
    v1.tx.fee = total_fee as u32;
    v1.tx.ext = TransactionExt::V1(soroban_data);
    v1.tx.operations = VecM::try_from(vec![op]).map_err(|e| xdr_err("operations", e))?;

    Ok(TransactionEnvelope::Tx(v1))
}

pub fn envelope_to_xdr(envelope: &TransactionEnvelope) -> Result<String, EngineError> {
    envelope
        .to_xdr_base64(Limits::none())
        .map_err(|e| xdr_err("encode envelope", e))
}

pub fn envelope_from_xdr(xdr: &str) -> Result<TransactionEnvelope, EngineError> {
    use stellar_xdr::curr::ReadXdr;
    TransactionEnvelope::from_xdr_base64(xdr, Limits::none())
        .map_err(|e| EngineError::InvalidRequest(format!("malformed transaction XDR: {e}")))
}

pub fn decode_soroban_transaction_data(xdr: &str) -> Result<SorobanTransactionData, EngineError> {
    use stellar_xdr::curr::ReadXdr;
    SorobanTransactionData::from_xdr_base64(xdr, Limits::none())
        .map_err(|e| EngineError::AdapterError(format!("malformed sorobanData from RPC: {e}")))
}

pub fn decode_auth_entry(xdr: &str) -> Result<SorobanAuthorizationEntry, EngineError> {
    use stellar_xdr::curr::ReadXdr;
    SorobanAuthorizationEntry::from_xdr_base64(xdr, Limits::none())
        .map_err(|e| EngineError::AdapterError(format!("malformed auth entry from RPC: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i128_round_trips_positive_and_negative() {
        for v in [0i128, 1, -1, i128::MAX, i128::MIN, 1_000_000_000_000] {
            let scval = i128_to_scval(v);
            assert_eq!(scval_to_i128(&scval).unwrap(), v);
        }
    }

    #[test]
    fn address_round_trips_contract_and_account() {
        // Constructed via the strkey crate's own encoder (not hand-typed
        // literals) so the checksum is guaranteed valid; this test then
        // proves parse_address/address_to_string round-trip correctly.
        let contract_strkey = stellar_strkey::Contract([7u8; 32]).to_string();
        let account_strkey = stellar_strkey::ed25519::PublicKey([9u8; 32]).to_string();

        let contract_addr = parse_address(&contract_strkey).unwrap();
        assert!(matches!(contract_addr, ScAddress::Contract(_)));
        assert_eq!(address_to_string(&contract_addr), contract_strkey);

        let account_addr = parse_address(&account_strkey).unwrap();
        assert!(matches!(account_addr, ScAddress::Account(_)));
        assert_eq!(address_to_string(&account_addr), account_strkey);
    }

    #[test]
    fn rejects_garbage_address() {
        assert!(parse_address("not-an-address").is_err());
        assert!(parse_address("").is_err());
    }

    #[test]
    fn u128_overflow_is_rejected() {
        let over = (i128::MAX as u128) + 1;
        assert!(u128_to_positive_i128(over, "amount").is_err());
        assert!(u128_to_positive_i128(1_000, "amount").is_ok());
    }
}
