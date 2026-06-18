// AMM constant-product math — mirrors src/math.rs in the Smart Contract repo.
// Any formula change in the contract must be reflected here.

pub const FEE_NUMERATOR: u128 = 997;
pub const FEE_DENOMINATOR: u128 = 1_000;
pub const MINIMUM_LIQUIDITY: u128 = 1_000;

#[derive(Debug, thiserror::Error)]
pub enum MathError {
    #[error("zero amount")]
    ZeroAmount,
    #[error("insufficient liquidity")]
    InsufficientLiquidity,
    #[error("arithmetic overflow")]
    Overflow,
}

pub fn get_amount_out(
    amount_in: u128,
    reserve_in: u128,
    reserve_out: u128,
) -> Result<u128, MathError> {
    if amount_in == 0 {
        return Err(MathError::ZeroAmount);
    }
    if reserve_in == 0 || reserve_out == 0 {
        return Err(MathError::InsufficientLiquidity);
    }

    let fee_in = amount_in
        .checked_mul(FEE_NUMERATOR)
        .ok_or(MathError::Overflow)?;
    let numerator = fee_in.checked_mul(reserve_out).ok_or(MathError::Overflow)?;
    let denominator = reserve_in
        .checked_mul(FEE_DENOMINATOR)
        .ok_or(MathError::Overflow)?
        .checked_add(fee_in)
        .ok_or(MathError::Overflow)?;

    Ok(numerator / denominator)
}

pub fn get_amount_in(
    amount_out: u128,
    reserve_in: u128,
    reserve_out: u128,
) -> Result<u128, MathError> {
    if amount_out == 0 {
        return Err(MathError::ZeroAmount);
    }
    if reserve_in == 0 || reserve_out == 0 {
        return Err(MathError::InsufficientLiquidity);
    }

    let numerator = reserve_in
        .checked_mul(amount_out)
        .ok_or(MathError::Overflow)?
        .checked_mul(FEE_DENOMINATOR)
        .ok_or(MathError::Overflow)?;
    let denominator = reserve_out
        .checked_sub(amount_out)
        .ok_or(MathError::InsufficientLiquidity)?
        .checked_mul(FEE_NUMERATOR)
        .ok_or(MathError::Overflow)?;

    numerator
        .checked_div(denominator)
        .ok_or(MathError::Overflow)?
        .checked_add(1)
        .ok_or(MathError::Overflow)
}

pub fn price_impact_bps(amount_in: u128, reserve_in: u128) -> u64 {
    if reserve_in == 0 {
        return 10_000;
    }
    let impact = (amount_in * 10_000) / (reserve_in + amount_in);
    impact.min(10_000) as u64
}

pub fn lp_tokens_to_mint(
    amount_0: u128,
    amount_1: u128,
    reserve_0: u128,
    reserve_1: u128,
    total_supply: u128,
) -> Result<u128, MathError> {
    if total_supply == 0 {
        let product = amount_0.checked_mul(amount_1).ok_or(MathError::Overflow)?;
        let lp = integer_sqrt(product).saturating_sub(MINIMUM_LIQUIDITY);
        if lp == 0 {
            return Err(MathError::InsufficientLiquidity);
        }
        return Ok(lp);
    }
    let lp_0 = amount_0
        .checked_mul(total_supply)
        .ok_or(MathError::Overflow)?
        / reserve_0;
    let lp_1 = amount_1
        .checked_mul(total_supply)
        .ok_or(MathError::Overflow)?
        / reserve_1;
    Ok(lp_0.min(lp_1))
}

pub fn withdrawal_amounts(
    liquidity: u128,
    reserve_0: u128,
    reserve_1: u128,
    total_supply: u128,
) -> Result<(u128, u128), MathError> {
    let a0 = liquidity
        .checked_mul(reserve_0)
        .ok_or(MathError::Overflow)?
        / total_supply;
    let a1 = liquidity
        .checked_mul(reserve_1)
        .ok_or(MathError::Overflow)?
        / total_supply;
    Ok((a0, a1))
}

fn integer_sqrt(n: u128) -> u128 {
    if n < 4 {
        return if n == 0 { 0 } else { 1 };
    }
    let mut z = n;
    let mut x = n / 2 + 1;
    while x < z {
        z = x;
        x = (n / x + x) / 2;
    }
    z
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amount_out_standard() {
        let out = get_amount_out(1_000_000, 10_000_000, 10_000_000).unwrap();
        assert!(out < 1_000_000, "fee must be deducted");
        assert!(
            out > 900_000,
            "output should be close to input for balanced pool"
        );
    }

    #[test]
    fn amount_out_zero_rejects() {
        assert!(get_amount_out(0, 1_000, 1_000).is_err());
    }

    #[test]
    fn amount_in_roundtrip() {
        let reserve = 10_000_000u128;
        let desired_out = 500_000u128;
        let amount_in = get_amount_in(desired_out, reserve, reserve).unwrap();
        let actual_out = get_amount_out(amount_in, reserve, reserve).unwrap();
        assert!(
            actual_out >= desired_out,
            "roundtrip must satisfy desired output"
        );
    }

    #[test]
    fn price_impact_increases_with_trade_size() {
        let small = price_impact_bps(100, 1_000_000);
        let large = price_impact_bps(500_000, 1_000_000);
        assert!(large > small);
    }
}
