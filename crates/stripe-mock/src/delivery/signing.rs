//! `Stripe-Signature` header: `t=<unix>,v1=<hex HMAC-SHA256(secret, "<t>.<body>")>` (spec section 9).

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

fn mac(secret: &str, t: i64, body: &[u8]) -> HmacSha256 {
    let mut m = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC takes any key length");
    m.update(t.to_string().as_bytes());
    m.update(b".");
    m.update(body);
    m
}

/// The header value, with one `v1` per secret in the given order.
pub(crate) fn sign(t: i64, body: &[u8], secrets: &[&str]) -> String {
    let mut header = format!("t={t}");
    for secret in secrets {
        let sig = hex::encode(mac(secret, t, body).finalize().into_bytes());
        header.push_str(",v1=");
        header.push_str(&sig);
    }
    header
}

/// Verifies a `Stripe-Signature` header the way Stripe's libraries do: any `v1` that
/// matches `secret` (constant-time), with `|now - t| <= tolerance`. Unknown keys and
/// `v0` are ignored.
pub fn verify_signature(
    header: &str,
    body: &[u8],
    secret: &str,
    tolerance_secs: i64,
    now_unix: i64,
) -> bool {
    let mut t = None;
    let mut sigs = Vec::new();
    for part in header.split(',') {
        match part.split_once('=') {
            Some(("t", v)) => t = v.trim().parse::<i64>().ok(),
            Some(("v1", v)) => sigs.push(v.trim()),
            _ => {}
        }
    }
    let Some(t) = t else {
        return false;
    };
    if (now_unix - t).abs() > tolerance_secs {
        return false;
    }
    sigs.iter().any(|sig| {
        hex::decode(sig)
            .map(|bytes| mac(secret, t, body).verify_slice(&bytes).is_ok())
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_then_verify() {
        let h = sign(1_700_000_000, b"{\"a\":1}", &["whsec_new", "whsec_old"]);
        assert!(h.starts_with("t=1700000000,v1="));
        assert_eq!(h.matches("v1=").count(), 2);
        assert!(verify_signature(
            &h,
            b"{\"a\":1}",
            "whsec_old",
            300,
            1_700_000_100
        ));
        assert!(!verify_signature(
            &h,
            b"{\"a\":2}",
            "whsec_new",
            300,
            1_700_000_100
        ));
        assert!(!verify_signature(
            &h,
            b"{\"a\":1}",
            "whsec_new",
            300,
            1_700_000_301
        ));
        assert!(verify_signature(
            &format!("{h},v0=junk,x=y"),
            b"{\"a\":1}",
            "whsec_new",
            300,
            1_700_000_000
        ));
        assert!(!verify_signature("garbage", b"", "whsec_new", 300, 0));
    }
}
