//! 认证：scrypt 密码 + HMAC 签名 cookie。
//!
//! 不用 JWT（前端存 token 是 XSS 放大器），不用 session 表（单用户、无状态签名足够）。
//! cookie 里只放 `用户名|过期时间戳`，签名用服务端密钥，篡改即失效。

use hmac::{Hmac, Mac};
use scrypt::{scrypt, Params};
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

const SCRYPT_LOG_N: u8 = 15; // N = 2^15
const SCRYPT_R: u32 = 8;
const SCRYPT_P: u32 = 1;

/// 生成 `scrypt$ln$r$p$salt_hex$hash_hex`。
pub fn hash_password(password: &str) -> String {
    let mut salt = [0u8; 16];
    use rand::RngCore;
    rand::thread_rng().fill_bytes(&mut salt);
    let params = Params::new(SCRYPT_LOG_N, SCRYPT_R, SCRYPT_P, 32).expect("scrypt 参数非法");
    let mut out = [0u8; 32];
    scrypt(password.as_bytes(), &salt, &params, &mut out).expect("scrypt 计算失败");
    format!(
        "scrypt${}${}${}${}${}",
        SCRYPT_LOG_N,
        SCRYPT_R,
        SCRYPT_P,
        hex::encode(salt),
        hex::encode(out)
    )
}

/// 校验。格式不对一律返回 false（不 panic）。
pub fn verify_password(password: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.split('$').collect();
    if parts.len() != 6 || parts[0] != "scrypt" {
        return false;
    }
    let (Ok(ln), Ok(r), Ok(p)) = (
        parts[1].parse::<u8>(),
        parts[2].parse::<u32>(),
        parts[3].parse::<u32>(),
    ) else {
        return false;
    };
    let Ok(salt) = hex::decode(parts[4]) else {
        return false;
    };
    let Ok(expected) = hex::decode(parts[5]) else {
        return false;
    };
    let Ok(params) = Params::new(ln, r, p, expected.len()) else {
        return false;
    };
    let mut out = vec![0u8; expected.len()];
    if scrypt(password.as_bytes(), &salt, &params, &mut out).is_err() {
        return false;
    }
    // 常数时间比较，避免时序侧信道
    out.ct_eq(&expected).into()
}

/// 生成 `payload.signature`（都是 hex/base64url 安全字符）。
pub fn sign(secret: &[u8], payload: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC 接受任意长度密钥");
    mac.update(payload.as_bytes());
    let sig = hex::encode(mac.finalize().into_bytes());
    format!("{payload}.{sig}")
}

/// 验签并返回 payload。失败返回 None。
pub fn verify(secret: &[u8], token: &str) -> Option<String> {
    let (payload, sig) = token.rsplit_once('.')?;
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC 接受任意长度密钥");
    mac.update(payload.as_bytes());
    let expected = hex::encode(mac.finalize().into_bytes());
    if expected.len() == sig.len() && expected.as_bytes().ct_eq(sig.as_bytes()).into() {
        Some(payload.to_string())
    } else {
        None
    }
}

/// 读取或首次生成服务端密钥（0600）。
pub fn load_or_create_secret(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    if let Ok(b) = std::fs::read(path) {
        if b.len() >= 32 {
            return Ok(b);
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut key = vec![0u8; 32];
    use rand::RngCore;
    rand::thread_rng().fill_bytes(&mut key);
    std::fs::write(path, &key)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(key)
}

/// 首次启动时生成初始密码并写入 hash 文件，返回明文（只打印一次）。
pub fn bootstrap_password(hash_path: &std::path::Path) -> std::io::Result<Option<String>> {
    if hash_path.exists() {
        return Ok(None);
    }
    use rand::Rng;
    const CHARS: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    let pw: String = (0..16)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect();
    if let Some(dir) = hash_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(hash_path, hash_password(&pw))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(hash_path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(Some(pw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_roundtrip() {
        let h = hash_password("correct horse battery");
        assert!(verify_password("correct horse battery", &h));
        assert!(!verify_password("wrong", &h));
    }

    #[test]
    fn malformed_stored_value_is_rejected_not_panicking() {
        assert!(!verify_password("x", ""));
        assert!(!verify_password("x", "bcrypt$1$2$3$4$5"));
        assert!(!verify_password("x", "scrypt$notanum$8$1$aa$bb"));
        assert!(!verify_password("x", "scrypt$15$8$1$zz$bb"));
    }

    #[test]
    fn signature_detects_tampering() {
        let secret = b"0123456789abcdef0123456789abcdef";
        let t = sign(secret, "admin|1700000000");
        assert_eq!(verify(secret, &t), Some("admin|1700000000".into()));

        let mut bad = t.clone();
        bad.push('x');
        assert_eq!(verify(secret, &bad), None, "尾部篡改必须发现");

        let tampered = t.replace("admin", "root");
        assert_eq!(verify(secret, &tampered), None, "负载篡改必须发现");

        assert_eq!(verify(b"another-secret-key-32-bytes-long!", &t), None);
    }

    #[test]
    fn different_passwords_get_different_salts() {
        let a = hash_password("same");
        let b = hash_password("same");
        assert_ne!(a, b, "盐必须随机");
        assert!(verify_password("same", &a));
        assert!(verify_password("same", &b));
    }
}
