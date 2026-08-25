//! The pool contract's audited ABI, embedded and hashed.
//!
//! This is the "manifest" transaction preparation is tied to: an explicit,
//! version-controlled table of the exact functions and argument shapes the
//! deployed `NodusAmm` contract accepts (see
//! `contracts/pool/src/lib.rs::NodusAmm` in the
//! `Nodus-protocol/Nodus-Protocol-Smart-Contract` repo, `#[contractimpl]`
//! block — `swap`, `add_liquidity`, `remove_liquidity`). Every prepared
//! transaction is built by looking up its function here; there is no path
//! from user input to an arbitrary function name or argument encoding.
//!
//! Ideally this table would be generated at build time from the contract's
//! published Wasm/spec (`soroban-spec`) tied to a spec hash the contract
//! itself exposes. That requires the compiled Wasm artifact, which does not
//! live in this repository (it's built from the separate smart-contract
//! repo). Until that artifact is wired into this build, [`SPEC_HASH`] hashes
//! this embedded manifest itself: it changes if and only if the manifest
//! below is edited, so [`tests::spec_hash_is_pinned`] catches accidental
//! drift the same way a generated-spec hash mismatch would.

use sha2::{Digest, Sha256};
use std::sync::OnceLock;

use crate::utils::EngineError;

/// One typed argument slot in a contract function's signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbiType {
    /// Soroban `Address` (either a `G...` account or `C...` contract).
    Address,
    /// Soroban `i128`.
    I128,
    /// Soroban `u64`.
    U64,
}

impl AbiType {
    fn as_str(self) -> &'static str {
        match self {
            AbiType::Address => "address",
            AbiType::I128 => "i128",
            AbiType::U64 => "u64",
        }
    }
}

/// One contract function the engine knows how to prepare a transaction for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolFunction {
    Swap,
    AddLiquidity,
    RemoveLiquidity,
}

impl PoolFunction {
    pub fn wire_name(self) -> &'static str {
        match self {
            PoolFunction::Swap => "swap",
            PoolFunction::AddLiquidity => "add_liquidity",
            PoolFunction::RemoveLiquidity => "remove_liquidity",
        }
    }

    /// Looks up a function by its on-chain symbol. Anything not in this
    /// table is unknown ABI and must be rejected before it is ever encoded.
    pub fn lookup(wire_name: &str) -> Option<Self> {
        MANIFEST
            .iter()
            .find(|f| f.name == wire_name)
            .map(|f| f.function)
    }

    pub fn params(self) -> &'static [AbiParam] {
        MANIFEST
            .iter()
            .find(|f| f.function == self)
            .map(|f| f.params)
            .unwrap_or(&[])
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AbiParam {
    pub name: &'static str,
    pub ty: AbiType,
}

const fn param(name: &'static str, ty: AbiType) -> AbiParam {
    AbiParam { name, ty }
}

struct ManifestEntry {
    function: PoolFunction,
    name: &'static str,
    params: &'static [AbiParam],
}

const SWAP_PARAMS: &[AbiParam] = &[
    param("to", AbiType::Address),
    param("amount_0_out", AbiType::I128),
    param("amount_1_out", AbiType::I128),
    param("deadline", AbiType::U64),
];

const ADD_LIQUIDITY_PARAMS: &[AbiParam] = &[
    param("from", AbiType::Address),
    param("to", AbiType::Address),
    param("amount_0_desired", AbiType::I128),
    param("amount_1_desired", AbiType::I128),
    param("amount_0_min", AbiType::I128),
    param("amount_1_min", AbiType::I128),
    param("deadline", AbiType::U64),
];

const REMOVE_LIQUIDITY_PARAMS: &[AbiParam] = &[
    param("from", AbiType::Address),
    param("to", AbiType::Address),
    param("liquidity", AbiType::I128),
    param("amount_0_min", AbiType::I128),
    param("amount_1_min", AbiType::I128),
    param("deadline", AbiType::U64),
];

/// The manifest itself. Adding, removing, renaming, or reordering anything
/// here changes [`SPEC_HASH`].
const MANIFEST: &[ManifestEntry] = &[
    ManifestEntry {
        function: PoolFunction::Swap,
        name: "swap",
        params: SWAP_PARAMS,
    },
    ManifestEntry {
        function: PoolFunction::AddLiquidity,
        name: "add_liquidity",
        params: ADD_LIQUIDITY_PARAMS,
    },
    ManifestEntry {
        function: PoolFunction::RemoveLiquidity,
        name: "remove_liquidity",
        params: REMOVE_LIQUIDITY_PARAMS,
    },
];

/// Manifest version tag. Bump this (and expect [`SPEC_HASH`] to change)
/// whenever the ABI table above changes.
pub const MANIFEST_VERSION: &str = "pool-abi-v1";

fn canonical_manifest() -> String {
    let mut out = String::new();
    out.push_str(MANIFEST_VERSION);
    for entry in MANIFEST {
        out.push('|');
        out.push_str(entry.name);
        for p in entry.params {
            out.push(',');
            out.push_str(p.name);
            out.push(':');
            out.push_str(p.ty.as_str());
        }
    }
    out
}

static SPEC_HASH_CELL: OnceLock<String> = OnceLock::new();

/// Hex-encoded SHA-256 of the canonical manifest above. Every prepared
/// transaction's review summary reports this, so a caller (Frontend,
/// Mobile) can pin the exact ABI version it independently verified against.
pub fn spec_hash() -> &'static str {
    SPEC_HASH_CELL.get_or_init(|| hex::encode(Sha256::digest(canonical_manifest().as_bytes())))
}

/// Validates that `wire_name` is a known function and that `arg_count`
/// matches its arity, without yet inspecting argument values. Callers still
/// need [`crate::pool::xdr::encode_args`] (or an XDR round-trip decode) to
/// confirm the argument *types* line up.
pub fn require_known_function(
    wire_name: &str,
    arg_count: usize,
) -> Result<PoolFunction, EngineError> {
    let func = PoolFunction::lookup(wire_name).ok_or_else(|| {
        EngineError::InvalidRequest(format!("unknown contract function: {wire_name}"))
    })?;
    let expected = func.params().len();
    if arg_count != expected {
        return Err(EngineError::InvalidRequest(format!(
            "{wire_name} expects {expected} argument(s), got {arg_count}"
        )));
    }
    Ok(func)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the manifest hash so an accidental edit to the ABI table is
    /// caught in review, the same way a generated-spec hash mismatch would
    /// be. If this test fails because you *intentionally* changed the
    /// manifest, update this constant and bump [`MANIFEST_VERSION`].
    #[test]
    fn spec_hash_is_pinned() {
        assert_eq!(
            spec_hash(),
            "53939281c5eeff34b8f0d3e3b817919901d98329cec0ae6532e7dc9b944089dc"
        );
    }

    #[test]
    fn known_functions_resolve() {
        assert_eq!(PoolFunction::lookup("swap"), Some(PoolFunction::Swap));
        assert_eq!(
            PoolFunction::lookup("add_liquidity"),
            Some(PoolFunction::AddLiquidity)
        );
        assert_eq!(
            PoolFunction::lookup("remove_liquidity"),
            Some(PoolFunction::RemoveLiquidity)
        );
    }

    #[test]
    fn unknown_function_is_rejected() {
        assert_eq!(PoolFunction::lookup("drain_pool"), None);
        assert!(require_known_function("drain_pool", 0).is_err());
    }

    #[test]
    fn wrong_arity_is_rejected() {
        assert!(require_known_function("swap", 2).is_err());
        assert!(require_known_function("swap", 4).is_ok());
    }
}
