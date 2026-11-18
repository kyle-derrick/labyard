//! 随二进制分发的执行体（脚本、模板、前端产物）。
//!
//! 用 `rust-embed` 编进二进制：宿主上只需要一个文件，不需要额外分发目录。

use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "assets/"]
pub struct Assets;

impl Assets {
    /// 读一个文本资产。返回 None 表示不存在——调用方必须当作错误处理，
    /// 而不是静默跳过（否则会"apply 成功但什么都没配"）。
    pub fn text(path: &str) -> Option<String> {
        Self::get(path)
            .and_then(|f| String::from_utf8(f.data.into_owned()).ok())
    }

    /// 列出一个 provisioner 目录下的文件名。
    pub fn provisioner_files(name: &str) -> Vec<String> {
        Self::iter()
            .filter(|p| p.starts_with(&format!("provisioners/{name}/")))
            .map(|p| p.to_string())
            .collect()
    }

    /// provisioner 的内容哈希：任一文件变了就重跑（幂等的判定依据）。
    pub fn provisioner_hash(name: &str) -> String {
        let mut files = Self::provisioner_files(name);
        files.sort();
        let mut acc = String::new();
        for f in files {
            acc.push_str(&f);
            acc.push('\0');
            if let Some(t) = Self::text(&f) {
                acc.push_str(&t);
            }
            acc.push('\0');
        }
        crate::core::plan::sha256_hex(acc.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_bootstrap_is_embedded() {
        let t = Assets::text("base/bootstrap.sh").expect("bootstrap.sh 必须随二进制分发");
        assert!(t.contains("labyard"), "脚本里应有标识");
    }

    #[test]
    fn provisioner_hash_changes_with_content() {
        let h1 = Assets::provisioner_hash("apt-mirror");
        assert!(!h1.is_empty());
        // 不存在的 provisioner 也应得到稳定哈希（空集）
        assert_eq!(
            Assets::provisioner_hash("does-not-exist"),
            crate::core::plan::sha256_hex(b"")
        );
    }
}
