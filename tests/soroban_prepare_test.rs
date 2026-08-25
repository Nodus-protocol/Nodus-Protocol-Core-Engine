//! Golden tests for the Soroban transaction-preparation pipeline: swap,
//! add-liquidity, and remove-liquidity happy paths, plus adversarial field
//! mutations (unknown ABI, extra operations, wrong contract, expired
//! deadline, fee above ceiling, stale sequence).
//!
//! Soroban RPC is faked via [`RpcTransport`] so these run deterministically
//! with no network access, while still exercising the real XDR
//! construction/decoding in `src/pool/xdr.rs` and `src/pool/prepare.rs`.

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::{json, Value};

use nodus_core_engine::config::{Network, PoolConfig};
use nodus_core_engine::pool::abi::PoolFunction;
use nodus_core_engine::pool::contract::ContractClient;
use nodus_core_engine::pool::prepare::{self, PrepareParams};
use nodus_core_engine::pool::soroban::{RpcTransport, SorobanRpc};
use nodus_core_engine::pool::xdr;
use nodus_core_engine::utils::EngineError;

use stellar_xdr::curr::{
    AccountEntry, AccountEntryExt, AccountId, HostFunction, InvokeContractArgs,
    InvokeHostFunctionOp, LedgerEntryData, LedgerFootprint, Limits, Memo, MuxedAccount, Operation,
    OperationBody, Preconditions, PublicKey, ScSymbol, ScVal, SequenceNumber, SorobanResources,
    SorobanTransactionData, SorobanTransactionDataExt, String32, StringM, Thresholds, TimeBounds,
    TimePoint, Transaction, TransactionEnvelope, TransactionExt, TransactionV1Envelope, Uint256,
    VecM, WriteXdr,
};

// ── Fixtures ─────────────────────────────────────────────────────────────────

fn contract_strkey() -> String {
    stellar_strkey::Contract([1u8; 32]).to_string()
}

fn account_strkey(byte: u8) -> String {
    stellar_strkey::ed25519::PublicKey([byte; 32]).to_string()
}

fn far_future_deadline() -> u64 {
    unix_now() + 3600
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn fake_account_entry_xdr(pubkey_bytes: [u8; 32], seq: i64) -> String {
    let entry = AccountEntry {
        account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(pubkey_bytes))),
        balance: 1_000_000_000,
        seq_num: SequenceNumber(seq),
        num_sub_entries: 0,
        inflation_dest: None,
        flags: 0,
        home_domain: String32(StringM::default()),
        thresholds: Thresholds([1, 0, 0, 0]),
        signers: VecM::default(),
        ext: AccountEntryExt::V0,
    };
    LedgerEntryData::Account(entry)
        .to_xdr_base64(Limits::none())
        .unwrap()
}

fn fake_soroban_data_xdr(resource_fee: i64) -> (String, String) {
    let data = SorobanTransactionData {
        ext: SorobanTransactionDataExt::V0,
        resources: SorobanResources {
            footprint: LedgerFootprint {
                read_only: VecM::default(),
                read_write: VecM::default(),
            },
            instructions: 0,
            disk_read_bytes: 0,
            write_bytes: 0,
        },
        resource_fee,
    };
    (
        data.to_xdr_base64(Limits::none()).unwrap(),
        resource_fee.to_string(),
    )
}

/// A [`RpcTransport`] that answers `getLedgerEntries` with a canned account
/// (or none, to simulate an unfunded account) and `simulateTransaction` with
/// a canned success/error response — no real network involved.
struct FakeTransport {
    account_bytes: [u8; 32],
    sequence: i64,
    account_missing: bool,
    simulate_error: Option<String>,
    resource_fee: i64,
}

impl FakeTransport {
    fn happy(account_bytes: [u8; 32], sequence: i64) -> Self {
        Self {
            account_bytes,
            sequence,
            account_missing: false,
            simulate_error: None,
            resource_fee: 1_000,
        }
    }
}

#[async_trait]
impl RpcTransport for FakeTransport {
    async fn call(&self, method: &str, _params: Value) -> Result<Value, EngineError> {
        match method {
            "getLedgerEntries" => {
                if self.account_missing {
                    Ok(json!({ "entries": [] }))
                } else {
                    Ok(json!({
                        "entries": [
                            { "key": "", "xdr": fake_account_entry_xdr(self.account_bytes, self.sequence) }
                        ]
                    }))
                }
            }
            "simulateTransaction" => {
                if let Some(err) = &self.simulate_error {
                    return Ok(json!({ "error": err, "latestLedger": 100 }));
                }
                let (transaction_data, min_resource_fee) = fake_soroban_data_xdr(self.resource_fee);
                Ok(json!({
                    "latestLedger": 100,
                    "transactionData": transaction_data,
                    "minResourceFee": min_resource_fee,
                    "results": [ { "xdr": "", "auth": [] } ],
                }))
            }
            other => Err(EngineError::Internal(format!(
                "unexpected rpc method in test: {other}"
            ))),
        }
    }
}

fn client(transport: FakeTransport, fee_ceiling_stroops: u32) -> ContractClient {
    let pool = PoolConfig {
        soroban_rpc_url: "http://unused.invalid".into(),
        contract_id: contract_strkey(),
        token_0: "XLM".into(),
        token_1: "USDC".into(),
        base_fee_stroops: 100,
        fee_ceiling_stroops,
    };
    ContractClient::new(
        SorobanRpc::with_transport(Box::new(transport)),
        &pool,
        Network::Testnet,
    )
}

// ── Golden happy paths ───────────────────────────────────────────────────────

#[tokio::test]
async fn swap_golden_happy_path() {
    let source_bytes = [2u8; 32];
    let pool = client(FakeTransport::happy(source_bytes, 41), 1_000_000);

    let prepared = pool
        .prepare_swap(
            &account_strkey(3),
            1_000,
            0,
            PrepareParams {
                network: Network::Testnet,
                source_account: account_strkey(2),
                deadline: far_future_deadline(),
            },
        )
        .await
        .expect("swap should prepare successfully");

    assert_eq!(prepared.review.function, "swap");
    assert_eq!(prepared.review.contract, contract_strkey());
    assert_eq!(prepared.review.sequence, 42); // 41 + 1
    assert_eq!(prepared.review.fee_stroops, 1_100); // base 100 + resource 1000
    assert_eq!(prepared.review.resource_fee_stroops, Some(1_000));
    assert_eq!(prepared.review.operation_count, 1);
    assert_eq!(
        prepared.review.args["amount_0_out"],
        serde_json::json!("1000")
    );
    assert_eq!(
        prepared.review.spec_hash,
        nodus_core_engine::pool::abi::spec_hash()
    );
    assert!(!prepared.xdr.is_empty());
}

#[tokio::test]
async fn add_liquidity_golden_happy_path() {
    let pool = client(FakeTransport::happy([4u8; 32], 9), 1_000_000);

    let prepared = pool
        .prepare_add_liquidity(
            &account_strkey(4),
            &account_strkey(4),
            5_000,
            5_000,
            4_900,
            4_900,
            PrepareParams {
                network: Network::Testnet,
                source_account: account_strkey(4),
                deadline: far_future_deadline(),
            },
        )
        .await
        .expect("add_liquidity should prepare successfully");

    assert_eq!(prepared.review.function, "add_liquidity");
    assert_eq!(prepared.review.sequence, 10);
    assert_eq!(
        prepared.review.args["amount_0_desired"],
        serde_json::json!("5000")
    );
}

#[tokio::test]
async fn remove_liquidity_golden_happy_path() {
    let pool = client(FakeTransport::happy([5u8; 32], 100), 1_000_000);

    let prepared = pool
        .prepare_remove_liquidity(
            &account_strkey(5),
            &account_strkey(5),
            2_500,
            1_000,
            1_000,
            PrepareParams {
                network: Network::Testnet,
                source_account: account_strkey(5),
                deadline: far_future_deadline(),
            },
        )
        .await
        .expect("remove_liquidity should prepare successfully");

    assert_eq!(prepared.review.function, "remove_liquidity");
    assert_eq!(prepared.review.args["liquidity"], serde_json::json!("2500"));
}

// ── Adversarial: prepare-time policy ────────────────────────────────────────

#[tokio::test]
async fn rejects_fee_above_ceiling() {
    // base_fee 100 + resource_fee 1000 = 1100, ceiling is set below that.
    let pool = client(FakeTransport::happy([6u8; 32], 1), 500);

    let err = pool
        .prepare_swap(
            &account_strkey(7),
            1,
            0,
            PrepareParams {
                network: Network::Testnet,
                source_account: account_strkey(6),
                deadline: far_future_deadline(),
            },
        )
        .await
        .unwrap_err();

    assert_eq!(err.http_status(), 412);
}

#[tokio::test]
async fn rejects_expired_deadline() {
    let pool = client(FakeTransport::happy([8u8; 32], 1), 1_000_000);

    let err = pool
        .prepare_swap(
            &account_strkey(9),
            1,
            0,
            PrepareParams {
                network: Network::Testnet,
                source_account: account_strkey(8),
                deadline: unix_now().saturating_sub(60),
            },
        )
        .await
        .unwrap_err();

    assert_eq!(err.http_status(), 412);
}

#[tokio::test]
async fn rejects_wrong_network() {
    let pool = client(FakeTransport::happy([10u8; 32], 1), 1_000_000);

    let err = pool
        .prepare_swap(
            &account_strkey(11),
            1,
            0,
            PrepareParams {
                network: Network::Mainnet, // client is configured for Testnet
                source_account: account_strkey(10),
                deadline: far_future_deadline(),
            },
        )
        .await
        .unwrap_err();

    assert_eq!(err.http_status(), 400);
}

#[tokio::test]
async fn rejects_when_source_account_is_unfunded() {
    let mut transport = FakeTransport::happy([12u8; 32], 1);
    transport.account_missing = true;
    let pool = client(transport, 1_000_000);

    let err = pool
        .prepare_swap(
            &account_strkey(13),
            1,
            0,
            PrepareParams {
                network: Network::Testnet,
                source_account: account_strkey(12),
                deadline: far_future_deadline(),
            },
        )
        .await
        .unwrap_err();

    assert_eq!(err.http_status(), 412);
}

#[tokio::test]
async fn rejects_when_simulation_errors() {
    let mut transport = FakeTransport::happy([14u8; 32], 1);
    transport.simulate_error = Some("host invocation failed: contract paused".into());
    let pool = client(transport, 1_000_000);

    let err = pool
        .prepare_swap(
            &account_strkey(15),
            1,
            0,
            PrepareParams {
                network: Network::Testnet,
                source_account: account_strkey(14),
                deadline: far_future_deadline(),
            },
        )
        .await
        .unwrap_err();

    assert_eq!(err.http_status(), 412);
}

// ── Adversarial: validate-time policy over hand-mutated XDR ─────────────────

/// Builds a raw, unsimulated `TransactionEnvelope::Tx` without going through
/// [`xdr::build_invoke_envelope`] (which only ever accepts a known
/// [`PoolFunction`]) — this is how the adversarial tests below construct
/// "unknown ABI" and "extra operations" cases that the real builder cannot
/// produce.
#[allow(clippy::too_many_arguments)]
fn manual_envelope(
    contract_strkey: &str,
    function_name: &str,
    args: Vec<ScVal>,
    source_strkey: &str,
    sequence: i64,
    fee: u32,
    deadline: u64,
    extra_op: bool,
) -> TransactionEnvelope {
    let contract_address = xdr::parse_address(contract_strkey).unwrap();
    let source = xdr::parse_account(source_strkey).unwrap();
    let AccountId(PublicKey::PublicKeyTypeEd25519(source_key)) = source;

    let make_op = || Operation {
        source_account: None,
        body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            host_function: HostFunction::InvokeContract(InvokeContractArgs {
                contract_address: contract_address.clone(),
                function_name: ScSymbol(StringM::try_from(function_name).unwrap()),
                args: VecM::try_from(args.clone()).unwrap(),
            }),
            auth: VecM::default(),
        }),
    };

    let mut ops = vec![make_op()];
    if extra_op {
        ops.push(make_op());
    }

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(source_key),
        fee,
        seq_num: SequenceNumber(sequence),
        cond: Preconditions::Time(TimeBounds {
            min_time: TimePoint(0),
            max_time: TimePoint(deadline),
        }),
        memo: Memo::None,
        operations: VecM::try_from(ops).unwrap(),
        ext: TransactionExt::V0,
    };

    TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    })
}

#[tokio::test]
async fn validate_rejects_unknown_function() {
    let pool = client(FakeTransport::happy([16u8; 32], 5), 1_000_000);
    let source = account_strkey(16);

    let envelope = manual_envelope(
        &contract_strkey(),
        "drain_pool",
        vec![ScVal::U64(1)],
        &source,
        6,
        100,
        far_future_deadline(),
        false,
    );
    let raw_xdr = xdr::envelope_to_xdr(&envelope).unwrap();

    let err = pool
        .validate_transaction(&raw_xdr, Network::Testnet, &source, unix_now())
        .await
        .unwrap_err();
    assert_eq!(err.http_status(), 400);
}

#[tokio::test]
async fn validate_rejects_extra_operations() {
    let pool = client(FakeTransport::happy([17u8; 32], 5), 1_000_000);
    let source = account_strkey(17);
    let args = prepare::encode_swap_args(&account_strkey(18), 1, 0, far_future_deadline()).unwrap();

    let envelope = manual_envelope(
        &contract_strkey(),
        PoolFunction::Swap.wire_name(),
        args,
        &source,
        6,
        100,
        far_future_deadline(),
        true, // extra operation
    );
    let raw_xdr = xdr::envelope_to_xdr(&envelope).unwrap();

    let err = pool
        .validate_transaction(&raw_xdr, Network::Testnet, &source, unix_now())
        .await
        .unwrap_err();
    assert_eq!(err.http_status(), 400);
}

#[tokio::test]
async fn validate_rejects_wrong_contract() {
    let pool = client(FakeTransport::happy([19u8; 32], 5), 1_000_000);
    let source = account_strkey(19);
    let args = prepare::encode_swap_args(&account_strkey(20), 1, 0, far_future_deadline()).unwrap();

    let wrong_contract = stellar_strkey::Contract([99u8; 32]).to_string();
    let envelope = manual_envelope(
        &wrong_contract,
        PoolFunction::Swap.wire_name(),
        args,
        &source,
        6,
        100,
        far_future_deadline(),
        false,
    );
    let raw_xdr = xdr::envelope_to_xdr(&envelope).unwrap();

    let err = pool
        .validate_transaction(&raw_xdr, Network::Testnet, &source, unix_now())
        .await
        .unwrap_err();
    assert_eq!(err.http_status(), 400);
}

#[tokio::test]
async fn validate_rejects_stale_sequence() {
    // Live account sequence is 5, but the transaction claims seq_num 999.
    let pool = client(FakeTransport::happy([21u8; 32], 5), 1_000_000);
    let source = account_strkey(21);
    let args = prepare::encode_swap_args(&account_strkey(22), 1, 0, far_future_deadline()).unwrap();

    let envelope = manual_envelope(
        &contract_strkey(),
        PoolFunction::Swap.wire_name(),
        args,
        &source,
        999,
        100,
        far_future_deadline(),
        false,
    );
    let raw_xdr = xdr::envelope_to_xdr(&envelope).unwrap();

    let err = pool
        .validate_transaction(&raw_xdr, Network::Testnet, &source, unix_now())
        .await
        .unwrap_err();
    assert_eq!(err.http_status(), 412);
}

#[tokio::test]
async fn validate_rejects_expired_deadline() {
    let pool = client(FakeTransport::happy([23u8; 32], 5), 1_000_000);
    let source = account_strkey(23);
    let expired = unix_now().saturating_sub(120);
    let args = prepare::encode_swap_args(&account_strkey(24), 1, 0, expired).unwrap();

    let envelope = manual_envelope(
        &contract_strkey(),
        PoolFunction::Swap.wire_name(),
        args,
        &source,
        6,
        100,
        expired,
        false,
    );
    let raw_xdr = xdr::envelope_to_xdr(&envelope).unwrap();

    let err = pool
        .validate_transaction(&raw_xdr, Network::Testnet, &source, unix_now())
        .await
        .unwrap_err();
    assert_eq!(err.http_status(), 412);
}

#[tokio::test]
async fn validate_accepts_a_well_formed_transaction() {
    let pool = client(FakeTransport::happy([25u8; 32], 5), 1_000_000);
    let source = account_strkey(25);
    let args = prepare::encode_swap_args(&account_strkey(26), 1, 0, far_future_deadline()).unwrap();

    let envelope = manual_envelope(
        &contract_strkey(),
        PoolFunction::Swap.wire_name(),
        args,
        &source,
        6,
        100,
        far_future_deadline(),
        false,
    );
    let raw_xdr = xdr::envelope_to_xdr(&envelope).unwrap();

    let review = pool
        .validate_transaction(&raw_xdr, Network::Testnet, &source, unix_now())
        .await
        .expect("well-formed transaction should validate");
    assert_eq!(review.function, "swap");
    assert_eq!(review.sequence, 6);
}
