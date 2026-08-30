//! Golden + mutation tests for the typed pool-state decoder.
//!
//! Real pool/pool-LP-token ledger entries are built *through the official
//! `stellar-xdr` types* (the same way the deployed Soroban contract would
//! serialize them), and the engine's `ContractClient` must decode them with
//! correct, typed values — never by scanning raw bytes for field-name
//! fragments. The mutation cases prove that malformed, truncated, or
//! wrong-type XDR fails safely instead of silently returning a guessed 0.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;

use nodus_core_engine::config::{Network, PoolConfig};
use nodus_core_engine::pool::contract::ContractClient;
use nodus_core_engine::pool::soroban::{RpcTransport, SorobanRpc};

use stellar_xdr::curr::{
    ContractDataDurability, ContractExecutable, ContractId, Hash, Int128Parts, LedgerEntryData,
    LedgerKey, Limits, PublicKey, ReadXdr, ScAddress, ScMap, ScMapEntry, ScSymbol, ScVal, StringM,
    Uint256, WriteXdr,
};

fn contract_strkey(byte: u8) -> String {
    stellar_strkey::Contract([byte; 32]).to_string()
}

fn account_strkey(byte: u8) -> String {
    stellar_strkey::ed25519::PublicKey([byte; 32]).to_string()
}

/// A `RpcTransport` that answers `getLedgerEntries` by looking up each
/// requested key in a caller-supplied map of key-XDR -> entry-XDR. Any key
/// not present is treated as "no such entry" (an empty `entries` array),
/// mirroring how Soroban RPC omits absent ledger entries.
struct MapTransport {
    entries: HashMap<String, String>,
}

impl MapTransport {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    fn with(mut self, key_xdr: String, entry_xdr: String) -> Self {
        self.entries.insert(key_xdr, entry_xdr);
        self
    }
}

#[async_trait]
impl RpcTransport for MapTransport {
    async fn call(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, nodus_core_engine::utils::EngineError> {
        match method {
            "getLedgerEntries" => {
                let keys = params["keys"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|v| v.as_str().map(String::from));
                let entries: Vec<Value> = keys
                    .filter_map(|k| self.entries.get(&k))
                    .map(|entry_xdr| json!({ "key": "", "xdr": entry_xdr }))
                    .collect();
                Ok(json!({ "entries": entries }))
            }
            other => Err(nodus_core_engine::utils::EngineError::Internal(format!(
                "unexpected rpc method in test: {other}"
            ))),
        }
    }
}

fn client(transport: MapTransport) -> ContractClient {
    let pool = PoolConfig {
        soroban_rpc_url: "http://unused.invalid".into(),
        contract_id: contract_strkey(1),
        token_0: "XLM".into(),
        token_1: "USDC".into(),
        base_fee_stroops: 100,
        fee_ceiling_stroops: 1_000_000,
    };
    ContractClient::new(
        SorobanRpc::with_transport(Box::new(transport)),
        &pool,
        Network::Testnet,
    )
}

// ── Fixture builders (through official stellar-xdr types) ───────────────────

fn contract_addr(byte: u8) -> ScAddress {
    ScAddress::Contract(ContractId(Hash([byte; 32])))
}

/// A `Symbol` ScVal.
fn sym(name: &str) -> ScVal {
    ScVal::Symbol(ScSymbol(
        StringM::try_from(name.as_bytes().to_vec()).unwrap(),
    ))
}

/// A positive `I128` ScVal with the given two 64-bit halves.
fn i128_scval(hi: i64, lo: u64) -> ScVal {
    ScVal::I128(Int128Parts { hi, lo })
}

/// Wraps a full instance storage `ScMap` in a `LedgerEntryData::ContractData`
/// with `key = LedgerKeyContractInstance`, as the deployed pool stores it.
fn instance_contract_data_xdr(contract: ScAddress, storage: ScMap) -> String {
    let entry_val = ScVal::ContractInstance(stellar_xdr::curr::ScContractInstance {
        executable: ContractExecutable::Wasm(stellar_xdr::curr::Hash([0u8; 32])),
        storage: Some(storage),
    });
    let cd = stellar_xdr::curr::ContractDataEntry {
        ext: stellar_xdr::curr::ExtensionPoint::V0,
        contract,
        key: ScVal::LedgerKeyContractInstance,
        durability: ContractDataDurability::Persistent,
        val: entry_val,
    };
    LedgerEntryData::ContractData(cd)
        .to_xdr_base64(Limits::none())
        .unwrap()
}

/// The pool instance ledger key, via `xdr::contract_instance_ledger_key`.
fn pool_instance_key() -> String {
    nodus_core_engine::pool::xdr::contract_instance_ledger_key(&contract_strkey(1)).unwrap()
}

/// Builds a realistic pool instance storage map: `Reserve0`, `Reserve1`,
/// `TimestampLast`, and `LpToken` (the SEP-41 LP token contract).
fn pool_instance_map(lp_token_addr: ScAddress) -> ScMap {
    ScMap(
        stellar_xdr::curr::VecM::try_from(vec![
            ScMapEntry {
                key: sym("Reserve0"),
                val: i128_scval(1234, 5_678),
            },
            ScMapEntry {
                key: sym("Reserve1"),
                val: i128_scval(7777, 12_345),
            },
            ScMapEntry {
                key: sym("TimestampLast"),
                val: ScVal::U64(1_700_000_000),
            },
            ScMapEntry {
                key: sym("LpToken"),
                val: ScVal::Address(lp_token_addr.clone()),
            },
        ])
        .unwrap(),
    )
}

/// The persistent SEP-41 `Balance(Address)` key, via the engine's own typed
/// builder — the same one production uses — so the test exercises the exact
/// key construction path.
fn balance_key_xdr(holder: &str) -> String {
    let key = nodus_core_engine::pool::xdr::sepal41_balance_key(holder).unwrap();
    nodus_core_engine::pool::xdr::contract_persistent_ledger_key(&contract_strkey(9), key).unwrap()
}

/// The LP token's instance ledger key, for reading `TotalSupply`.
fn lp_instance_key() -> String {
    nodus_core_engine::pool::xdr::contract_instance_ledger_key(&contract_strkey(9)).unwrap()
}

// ── Golden happy path: get_reserves ─────────────────────────────────────────

#[tokio::test]
async fn reserves_decodes_typed_instance_storage() {
    let lp_token_addr = contract_addr(9);

    let transport = MapTransport::new()
        .with(
            pool_instance_key(),
            instance_contract_data_xdr(contract_addr(1), pool_instance_map(lp_token_addr.clone())),
        )
        .with(
            lp_instance_key(),
            instance_contract_data_xdr(
                lp_token_addr,
                ScMap(
                    stellar_xdr::curr::VecM::try_from(vec![ScMapEntry {
                        key: sym("TotalSupply"),
                        val: i128_scval(0, 100_000),
                    }])
                    .unwrap(),
                ),
            ),
        );

    let reserves = client(transport)
        .get_reserves()
        .await
        .expect("decode reserves");

    assert_eq!(reserves.reserve_0, (1234u128 << 64) | 5_678);
    assert_eq!(reserves.reserve_1, (7777u128 << 64) | 12_345);
    assert_eq!(reserves.timestamp_last, 1_700_000_000);
    assert_eq!(reserves.lp_total_supply, 100_000);
    assert_eq!(reserves.token_0, "XLM");
    assert_eq!(reserves.token_1, "USDC");
}

// ── Golden happy path: lp_balance via the actual LP token contract ──────────

#[tokio::test]
async fn lp_balance_reads_sepal41_balance_from_lp_token_contract() {
    let holder = account_strkey(7);
    let lp_token_addr = contract_addr(9);

    // The holder's balance entry keyed under Balance(holder) on the LP token.
    let balance_entry = LedgerEntryData::ContractData(stellar_xdr::curr::ContractDataEntry {
        ext: stellar_xdr::curr::ExtensionPoint::V0,
        contract: lp_token_addr.clone(),
        key: nodus_core_engine::pool::xdr::sepal41_balance_key(&holder).unwrap(),
        durability: ContractDataDurability::Persistent,
        val: i128_scval(0, 42_000),
    })
    .to_xdr_base64(Limits::none())
    .unwrap();

    let transport = MapTransport::new()
        .with(
            pool_instance_key(),
            instance_contract_data_xdr(contract_addr(1), pool_instance_map(lp_token_addr)),
        )
        .with(balance_key_xdr(&holder), balance_entry);

    let balance = client(transport)
        .lp_balance(&holder)
        .await
        .expect("decode LP balance");

    assert_eq!(balance, 42_000);
}

#[tokio::test]
async fn lp_balance_is_zero_when_no_balance_entry_exists() {
    let holder = account_strkey(7);
    let lp_token_addr = contract_addr(9);

    // Pool exists with LpToken set, but there is no balance entry for holder.
    let transport = MapTransport::new().with(
        pool_instance_key(),
        instance_contract_data_xdr(contract_addr(1), pool_instance_map(lp_token_addr)),
    );

    let balance = client(transport)
        .lp_balance(&holder)
        .await
        .expect("missing balance entry means zero");
    assert_eq!(balance, 0);
}

// ── Mutation cases: malformed / wrong-type XDR must fail safely ─────────────

#[tokio::test]
async fn reserves_rejects_missing_required_key() {
    // Instance map that omits Reserve0 entirely.
    let map = ScMap(
        stellar_xdr::curr::VecM::try_from(vec![ScMapEntry {
            key: sym("Reserve1"),
            val: i128_scval(0, 100),
        }])
        .unwrap(),
    );
    let transport = MapTransport::new().with(
        pool_instance_key(),
        instance_contract_data_xdr(contract_addr(1), map),
    );

    let err = client(transport).get_reserves().await.unwrap_err();
    assert!(err.to_string().contains("missing required key"));
}

#[tokio::test]
async fn reserves_rejects_wrong_type_value() {
    // Reserve0 is present but holds a Bool instead of I128.
    let map = ScMap(
        stellar_xdr::curr::VecM::try_from(vec![
            ScMapEntry {
                key: sym("Reserve0"),
                val: ScVal::Bool(true),
            },
            ScMapEntry {
                key: sym("Reserve1"),
                val: i128_scval(0, 100),
            },
            ScMapEntry {
                key: sym("TimestampLast"),
                val: ScVal::U64(1),
            },
        ])
        .unwrap(),
    );
    let transport = MapTransport::new().with(
        pool_instance_key(),
        instance_contract_data_xdr(contract_addr(1), map),
    );

    let err = client(transport).get_reserves().await.unwrap_err();
    assert!(err.to_string().contains("expected ScVal::I128"));
}

#[tokio::test]
async fn decode_rejects_truncated_xdr() {
    // Take a valid ledger entry and chop bytes off the end; the typed decoder
    // must reject it, not fail open and report guessed zeros.
    let valid = instance_contract_data_xdr(contract_addr(1), pool_instance_map(contract_addr(9)));
    let truncated = &valid[..valid.len() / 2];

    let map = nodus_core_engine::pool::xdr::decode_instance_storage(truncated);
    assert!(map.is_err(), "truncated XDR must not decode");
}

#[tokio::test]
async fn decode_rejects_non_contract_data_entry() {
    // An account entry is not a ContractData entry; decoding it as instance
    // storage must error, not be misread.
    let account = LedgerEntryData::Account(stellar_xdr::curr::AccountEntry {
        account_id: stellar_xdr::curr::AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
            [1u8; 32],
        ))),
        balance: 1000,
        seq_num: stellar_xdr::curr::SequenceNumber(5),
        num_sub_entries: 0,
        inflation_dest: None,
        flags: 0,
        home_domain: stellar_xdr::curr::String32(StringM::default()),
        thresholds: stellar_xdr::curr::Thresholds([1, 0, 0, 0]),
        signers: stellar_xdr::curr::VecM::default(),
        ext: stellar_xdr::curr::AccountEntryExt::V0,
    })
    .to_xdr_base64(Limits::none())
    .unwrap();

    let map = nodus_core_engine::pool::xdr::decode_instance_storage(&account);
    assert!(
        map.is_err(),
        "non-ContractData entry must not decode as instance storage"
    );
}

#[test]
fn sepal41_balance_key_matches_ledger_key_layout() {
    // The engine-built balance key must decode back into a
    // LedgerKeyContractData whose key is Vec([Symbol("Balance"), Address]),
    // durability Persistent, on the intended contract — proving it is a
    // typed key, not a hand-assembled byte buffer.
    let holder = account_strkey(3);
    let key_xdr = nodus_core_engine::pool::xdr::contract_persistent_ledger_key(
        &contract_strkey(9),
        nodus_core_engine::pool::xdr::sepal41_balance_key(&holder).unwrap(),
    )
    .unwrap();

    let decoded = LedgerKey::from_xdr_base64(&key_xdr, Limits::none()).unwrap();
    let LedgerKey::ContractData(lkcd) = decoded else {
        panic!("expected ContractData ledger key");
    };
    assert_eq!(lkcd.durability, ContractDataDurability::Persistent);
    assert_eq!(lkcd.contract, contract_addr(9));
    match &lkcd.key {
        ScVal::Vec(Some(vec)) => {
            assert_eq!(vec.0.len(), 2);
            assert!(matches!(&vec.0[0], ScVal::Symbol(_)));
            // The second element is the holder's address as an Account.
            assert!(matches!(&vec.0[1], ScVal::Address(ScAddress::Account(_))));
        }
        other => panic!("expected Vec balance key, got {other:?}"),
    }
}
