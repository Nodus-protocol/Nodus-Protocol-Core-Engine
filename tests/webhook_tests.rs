use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};

fn verify_signature(secret: &str, header: &str, body: &str, tolerance: u64) -> bool {
    let parts: Vec<&str> = header.split(',').collect();
    if parts.len() != 2 {
        return false;
    }
    
    let t_part = parts[0].strip_prefix("t=").unwrap_or("");
    let v1_part = parts[1].strip_prefix("v1=").unwrap_or("");
    
    let timestamp = match t_part.parse::<u64>() {
        Ok(t) => t,
        Err(_) => return false,
    };
    
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    
    if now > timestamp + tolerance || timestamp > now + tolerance {
        return false;
    }
    
    let signed_payload = format!("t={}\n{}", timestamp, body);
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(signed_payload.as_bytes());
    let expected = hex::encode(mac.finalize().into_bytes());
    
    expected == v1_part
}

fn create_signature(secret: &str, body: &str, timestamp: u64) -> String {
    let signed_payload = format!("t={}\n{}", timestamp, body);
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(signed_payload.as_bytes());
    let sig = hex::encode(mac.finalize().into_bytes());
    format!("t={},v1={}", timestamp, sig)
}

#[test]
fn test_verify_signature_correct_timestamp_passes() {
    let secret = "my_secret_key";
    let body = r#"{"event":"payment.confirmed"}"#;
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    
    let header = create_signature(secret, body, timestamp);
    
    assert!(verify_signature(secret, &header, body, 300));
}

#[test]
fn test_verify_signature_old_timestamp_fails() {
    let secret = "my_secret_key";
    let body = r#"{"event":"payment.confirmed"}"#;
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() - 400; // 400s old
    
    let header = create_signature(secret, body, timestamp);
    
    assert!(!verify_signature(secret, &header, body, 300));
}

#[test]
fn test_verify_signature_tampered_body_fails() {
    let secret = "my_secret_key";
    let body = r#"{"event":"payment.confirmed"}"#;
    let tampered_body = r#"{"event":"payment.failed"}"#;
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    
    let header = create_signature(secret, body, timestamp);
    
    assert!(!verify_signature(secret, &header, tampered_body, 300));
}
