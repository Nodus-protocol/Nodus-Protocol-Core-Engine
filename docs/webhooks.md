# Webhooks Documentation

Nodus Protocol Core Engine uses webhooks to notify your application when an event occurs in your account. 

## Signature Verification

Nodus signs the webhook events it sends to your endpoints by including a signature in each event's `x-nodus-signature` header. This allows you to verify that the events were sent by Nodus, not by a third party.

### Header Format

The `x-nodus-signature` header contains the timestamp and the signature, separated by a comma:
`x-nodus-signature: t=<unix_timestamp>,v1=<hmac_hex>`

### Signed Payload Structure

The signed payload is created by concatenating:
1. The timestamp (as a string)
2. The character `\n`
3. The actual JSON payload (i.e., the request body)

Format:
`t=<unix_timestamp>\n<body>`

### Replay Tolerance

To prevent replay attacks, we recommend rejecting webhooks with timestamps older than 5 minutes (300 seconds).

### Example Verification Code (Python)

```python
import hmac
import hashlib
import time

def verify_signature(secret: str, header: str, body: str, tolerance_seconds: int = 300) -> bool:
    parts = dict(p.split('=', 1) for p in header.split(','))
    timestamp = int(parts['t'])
    signature = parts['v1']

    # Reject events older than tolerance_seconds
    if abs(time.time() - timestamp) > tolerance_seconds:
        raise ValueError("Webhook timestamp too old — possible replay attack")

    signed_payload = f"t={timestamp}\n{body}"
    expected = hmac.new(secret.encode(), signed_payload.encode(), hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, signature)
```
