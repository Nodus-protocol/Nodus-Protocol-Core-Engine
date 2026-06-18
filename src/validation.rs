use crate::utils::EngineError;

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

pub fn amount(amount: u64) -> Result<(), EngineError> {
    if amount == 0 {
        Err(EngineError::InvalidRequest(
            "amount must be greater than 0".into(),
        ))
    } else {
        Ok(())
    }
}

pub fn token(token: &str) -> Result<(), EngineError> {
    if token.is_empty() || token.len() > 12 || !token.chars().all(|c| c.is_ascii_alphanumeric()) {
        Err(EngineError::InvalidRequest(format!(
            "invalid token symbol '{token}'"
        )))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn rejects_empty_token() {
        assert!(token("").is_err());
    }

    #[test]
    fn rejects_long_token() {
        assert!(token("TOOLONGTOKEN123").is_err());
    }

    #[test]
    fn accepts_valid_token() {
        assert!(token("XLM").is_ok());
        assert!(token("USDC").is_ok());
    }
}
