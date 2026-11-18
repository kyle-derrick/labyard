//! 规格文件的存取与结构化编辑。
//!
//! 两层：
//! - **文件层**：`paths.specs/*.yaml`，一份文件一组 zone。多份是为了让
//!   不同的实验互不干扰（`red.yaml` / `blue.yaml`），也让 git 历史更干净。
//! - **结构层**：按 zone / gateway / mirror 做增删改。用 `yaml-edit` 就地
//!   编辑 CST，**只动目标节点**——注释、空行、引号风格全部保留。
//!   手写 YAML 被表单一改就丢注释，是那种"用一次就不敢再用"的功能。
//!
//! 为什么规格不放 SQLite：运维要能 `cat`、能进 git、能手改回去。文件是
//! 唯一真相源，DB 只放运行态（vmid 分配、步骤状态、retire 标记）。

use serde::Serialize;
use serde_yaml_ng::Value;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use yaml_edit::{Document, Mapping, Sequence};

pub const DEFAULT_SPEC: &str = "lab";
pub const SPEC_MAX_BYTES: usize = 512 * 1024;

#[derive(Debug, Serialize)]
pub struct SpecMeta {
    pub name: String,
    pub zones: usize,
    pub hosts: usize,
    pub bytes: u64,
    pub modified: Option<String>,
    /// 解析失败的文件也要能被列出来——不然改坏一份 YAML 就等于把它弄丢了
    pub valid: bool,
    pub error: Option<String>,
}

/// 文件名白名单。规格名会进 URL 与文件路径，必须收死。
pub fn check_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 40
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && !name.starts_with('-')
        && !name.starts_with('_');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "规格名 `{name}` 非法：只允许小写字母、数字、- _，不能以 - _ 开头，最长 40"
        ))
    }
}

pub fn path_of(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.yaml"))
}

/// 列目录。解析失败的文件不隐藏，带 `valid=false` 与错误原因返回。
pub fn list(dir: &Path, wan: &str) -> Vec<SpecMeta> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("yaml") {
            continue;
        }
        let Some(name) = p.file_stem().and_then(|x| x.to_str()).map(|s| s.to_string()) else {
            continue;
        };
        let text = std::fs::read_to_string(&p).unwrap_or_default();
        let bytes = text.len() as u64;
        let modified = e
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .map(|t| {
                let d: chrono::DateTime<chrono::Utc> = t.into();
                d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            });
        let (zones, hosts, err) = match crate::core::spec::Spec::parse(&text)
            .and_then(|s| s.resolve_all(wan))
        {
            Ok(r) => (
                r.zones.len(),
                r.zones.iter().map(|z| z.hosts.len()).sum(),
                None,
            ),
            Err(e) => (0, 0, Some(e.to_string())),
        };
        out.push(SpecMeta {
            name,
            zones,
            hosts,
            bytes,
            modified,
            valid: err.is_none(),
            error: err,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub fn read(dir: &Path, name: &str) -> Result<Option<String>, String> {
    check_name(name)?;
    match std::fs::read_to_string(path_of(dir, name)) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("读取规格 {name} 失败: {e}")),
    }
}

pub fn exists(dir: &Path, name: &str) -> bool {
    path_of(dir, name).exists()
}

/// 原子写：同目录 tmp + rename。
///
/// 直接写会在掉电/被杀时留下半份 YAML，而半份 YAML 比"没有 YAML"更糟——
/// 它解析失败，但看起来像用户自己写错了。
pub fn write(dir: &Path, name: &str, text: &str) -> Result<(), String> {
    check_name(name)?;
    if text.len() > SPEC_MAX_BYTES {
        return Err(format!("规格超过 {} KB 上限", SPEC_MAX_BYTES / 1024));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("建目录失败: {e}"))?;
    let p = path_of(dir, name);
    let tmp = p.with_extension("yaml.tmp");
    std::fs::write(&tmp, text.as_bytes()).map_err(|e| format!("写临时文件失败: {e}"))?;
    std::fs::rename(&tmp, &p).map_err(|e| format!("替换规格失败: {e}"))?;
    Ok(())
}

pub fn remove(dir: &Path, name: &str) -> Result<bool, String> {
    check_name(name)?;
    match std::fs::remove_file(path_of(dir, name)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("删除规格 {name} 失败: {e}")),
    }
}

// ───────────────────────── 结构化编辑 ─────────────────────────

fn parse(doc: &str) -> Result<Document, String> {
    Document::from_str(doc).map_err(|e| format!("YAML 解析失败: {e}"))
}

fn root(doc: &Document) -> Result<Mapping, String> {
    doc.as_mapping()
        .ok_or_else(|| "规格根节点不是映射".to_string())
}

fn zones_of(doc: &Document) -> Result<Mapping, String> {
    let r = root(doc)?;
    if let Some(m) = r.get_mapping("zones") {
        return Ok(m);
    }
    r.set("zones", Mapping::new());
    root(doc)?
        .get_mapping("zones")
        .ok_or_else(|| "无法创建 zones".to_string())
}

fn zone_of(doc: &Document, zone: &str) -> Result<Mapping, String> {
    zones_of(doc)?
        .get_mapping(zone)
        .ok_or_else(|| format!("规格里没有 zone `{zone}`"))
}

fn gateway_of(doc: &Document, zone: &str) -> Result<Mapping, String> {
    let z = zone_of(doc, zone)?;
    if let Some(g) = z.get_mapping("gateway") {
        return Ok(g);
    }
    z.set("gateway", Mapping::new());
    zone_of(doc, zone)?
        .get_mapping("gateway")
        .ok_or_else(|| "无法创建 gateway".to_string())
}

fn key_str(k: &Value) -> String {
    match k {
        Value::String(s) => s.clone(),
        other => other.as_str().unwrap_or_default().to_string(),
    }
}

/// 把 serde_yaml 的值写进目标映射的某个键。
///
/// 不用 `YamlValue::Mapping(BTreeMap)` 中转：那会把键按字典序排（`bridge`
/// 跑到 `uplink` 前面），读起来就不像人写的了。这里按原文顺序逐个 `set`。
fn set_key(parent: &Mapping, key: &str, v: &Value) {
    match v {
        Value::Mapping(m) => {
            let child = Mapping::new();
            for (k, vv) in m {
                set_key(&child, &key_str(k), vv);
            }
            parent.set(key, child);
        }
        Value::Sequence(s) => {
            let child = Sequence::new();
            for it in s {
                push_item(&child, it);
            }
            parent.set(key, child);
        }
        Value::Bool(b) => parent.set(key, *b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                parent.set(key, i);
            } else {
                parent.set(key, n.as_f64().unwrap_or(0.0));
            }
        }
        Value::String(s) => parent.set(key, s.as_str()),
        // 规格里不该有 null（None 字段压根不写）。真写了就保留成 null 字面量。
        Value::Null => parent.set(key, Value::Null.as_str().unwrap_or("null")),
        other => parent.set(key, other.as_str().unwrap_or_default()),
    }
}

fn push_item(seq: &Sequence, v: &Value) {
    match v {
        Value::Mapping(m) => {
            let child = Mapping::new();
            for (k, vv) in m {
                set_key(&child, &key_str(k), vv);
            }
            seq.push(child);
        }
        Value::Sequence(s) => {
            let child = Sequence::new();
            for it in s {
                push_item(&child, it);
            }
            seq.push(child);
        }
        Value::Bool(b) => seq.push(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                seq.push(i);
            } else {
                seq.push(n.as_f64().unwrap_or(0.0));
            }
        }
        Value::String(s) => seq.push(s.as_str()),
        Value::Null => seq.push("null"),
        other => seq.push(other.as_str().unwrap_or_default()),
    }
}

fn parse_fragment(fragment: &str) -> Result<Value, String> {
    let v: Value = serde_yaml_ng::from_str(fragment)
        .map_err(|e| format!("zone 片段解析失败: {e}"))?;
    if !v.is_mapping() {
        return Err("zone 片段必须是一个映射（uplink / v4 / hosts ...）".into());
    }
    Ok(v)
}

/// 新增或整段替换一个 zone。
pub fn upsert_zone(doc: &str, zone: &str, fragment: &str) -> Result<String, String> {
    check_name(zone)?;
    let v = parse_fragment(fragment)?;
    let d = parse(doc)?;
    let zones = zones_of(&d)?;
    let child = Mapping::new();
    if let Value::Mapping(m) = &v {
        for (k, vv) in m {
            set_key(&child, &key_str(k), vv);
        }
    }
    zones.set(zone, child);
    Ok(d.to_string())
}

pub fn delete_zone(doc: &str, zone: &str) -> Result<String, String> {
    let d = parse(doc)?;
    let zones = zones_of(&d)?;
    if zones.remove(zone).is_none() {
        return Err(format!("规格里没有 zone `{zone}`"));
    }
    Ok(d.to_string())
}

/// 取出某个 zone 的 YAML 片段，作为前端"编辑 zone"的起点。
///
/// 用 serde_yaml_ng 整份解析再取子树：**该 zone 内部的注释会丢**。这是有意的
/// ——片段只是编辑框里的种子，保存时整段 zone 被 `upsert_zone` 替换掉，本来
/// 也留不住。zone 之外的注释不受影响，因为落盘走的是 yaml-edit 就地替换。
pub fn zone_snippet(doc: &str, zone: &str) -> Result<Option<String>, String> {
    let v: Value = serde_yaml_ng::from_str(doc).map_err(|e| format!("YAML 解析失败: {e}"))?;
    let Some(z) = v.get("zones").and_then(|zs| zs.get(zone)) else {
        return Ok(None);
    };
    serde_yaml_ng::to_string(z)
        .map(|s| Some(s))
        .map_err(|e| format!("zone `{zone}` 序列化失败: {e}"))
}

pub fn set_egress(doc: &str, zone: &str, on: bool) -> Result<String, String> {
    let d = parse(doc)?;
    gateway_of(&d, zone)?.set("egress", on);
    Ok(d.to_string())
}

pub fn add_pub(doc: &str, zone: &str, fragment: &str) -> Result<String, String> {
    let v = parse_fragment(fragment)?;
    let d = parse(doc)?;
    let g = gateway_of(&d, zone)?;
    if g.get_sequence("pub").is_none() {
        g.set("pub", Sequence::new());
    }
    let seq = gateway_of(&d, zone)?
        .get_sequence("pub")
        .ok_or_else(|| "无法创建 pub 列表".to_string())?;
    push_item(&seq, &v);
    Ok(d.to_string())
}

pub fn remove_pub(doc: &str, zone: &str, idx: usize) -> Result<String, String> {
    let d = parse(doc)?;
    let seq = gateway_of(&d, zone)?
        .get_sequence("pub")
        .ok_or_else(|| format!("zone `{zone}` 没有端口映射"))?;
    if seq.remove(idx).is_none() {
        return Err(format!("端口映射索引 {idx} 越界（共 {} 条）", seq.len()));
    }
    Ok(d.to_string())
}

/// `which` 只接受 wireguard / openvpn。
pub fn set_vpn(doc: &str, zone: &str, which: &str, on: bool) -> Result<String, String> {
    if !matches!(which, "wireguard" | "openvpn") {
        return Err("vpn 只支持 wireguard / openvpn".into());
    }
    let d = parse(doc)?;
    let g = gateway_of(&d, zone)?;
    if g.get_mapping("vpn").is_none() {
        g.set("vpn", Mapping::new());
    }
    gateway_of(&d, zone)?
        .get_mapping("vpn")
        .ok_or_else(|| "无法创建 vpn".to_string())?
        .set(which, on);
    Ok(d.to_string())
}

/// 一条 mirrors 记录里的某个字段是否等于 `want`。
///
/// 允许 YAML 写裸 host（`to: analysis`）而调用方给的是解析后的 `zone:host`
/// （列表接口就是把解析结果原样回传的）。不认这种写法的话，手写的连线在
/// UI 里"加得上、删不掉"，重复添加也拦不住。
fn link_field_matches(m: &Mapping, k: &str, want: &str) -> bool {
    let Some(v) = m.get(k).and_then(|n| n.as_scalar().map(|s| s.to_string())) else {
        return false;
    };
    if v == want {
        return true;
    }
    if v.contains(':') {
        return false;
    }
    want.rsplit_once(':').map(|(_, h)| h == v).unwrap_or(false)
}

/// 加一条镜像连线（幂等：重复加返回错误，让 UI 能提示而不是静默成功）。
pub fn add_mirror(doc: &str, from: &str, to: &str) -> Result<String, String> {
    let d = parse(doc)?;
    let r = root(&d)?;
    if r.get_sequence("mirrors").is_none() {
        r.set("mirrors", Sequence::new());
    }
    let seq = root(&d)?
        .get_sequence("mirrors")
        .ok_or_else(|| "无法创建 mirrors".to_string())?;
    for i in 0..seq.len() {
        if let Some(m) = seq.get(i).and_then(|n| n.as_mapping().cloned()) {
            if link_field_matches(&m, "from", from) && link_field_matches(&m, "to", to) {
                return Err(format!("连线 {from} → {to} 已存在"));
            }
        }
    }
    let item = Mapping::new();
    item.set("from", from);
    item.set("to", to);
    seq.push(item);
    Ok(d.to_string())
}

pub fn remove_mirror(doc: &str, from: &str, to: &str) -> Result<String, String> {
    let d = parse(doc)?;
    let Some(seq) = root(&d)?.get_sequence("mirrors") else {
        return Err("规格里没有 mirrors".into());
    };
    for i in 0..seq.len() {
        if let Some(m) = seq.get(i).and_then(|n| n.as_mapping().cloned()) {
            if link_field_matches(&m, "from", from) && link_field_matches(&m, "to", to) {
                seq.remove(i);
                return Ok(d.to_string());
            }
        }
    }
    Err(format!("没有找到连线 {from} → {to}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"# 顶部注释：这份规格是手写的
defaults:
  cores: 2      # 内联注释
  memory: 1024
  disk: 8
  template: debian-13-standard
  storage: local-lvm
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    hosts:
      gateway: { role: gateway }
      web: { role: server }
"#;

    fn valid(doc: &str) -> bool {
        crate::core::spec::Spec::parse(doc).is_ok()
    }

    #[test]
    fn name_whitelist() {
        assert!(check_name("lab").is_ok());
        assert!(check_name("red-team_1").is_ok());
        assert!(check_name("../etc/passwd").is_err());
        assert!(check_name("Lab").is_err(), "大写会带来大小写文件系统歧义");
        assert!(check_name("").is_err());
        assert!(check_name("-x").is_err());
    }

    #[test]
    fn upsert_zone_keeps_comments_and_adds_zone() {
        let out = upsert_zone(
            DOC,
            "sub",
            "uplink: main\nv4: 10.20.1.0/24\nhosts:\n  gateway: { role: gateway }\n",
        )
        .unwrap();
        assert!(out.contains("# 顶部注释"), "注释必须保留:\n{out}");
        assert!(out.contains("# 内联注释"), "内联注释也要保留:\n{out}");
        assert!(out.contains("sub:"), "新 zone 没写进去:\n{out}");
        assert!(valid(&out), "结果必须仍能被解析:\n{out}");
        // 字段顺序按片段里的顺序，不是字典序
        let i_up = out.find("uplink: main").unwrap();
        let i_v4 = out.find("v4: 10.20.1.0/24").unwrap();
        assert!(i_up < i_v4, "字段顺序被打乱:\n{out}");
    }

    #[test]
    fn upsert_zone_replaces_existing() {
        let out = upsert_zone(DOC, "main", "uplink: wan\nv4: 10.99.0.0/24\nhosts:\n  gateway: { role: gateway }\n").unwrap();
        assert!(out.contains("10.99.0.0/24"));
        assert!(!out.contains("10.10.1.0/24"));
        assert_eq!(out.matches("main:").count(), 1, "不该出现重复的 zone 键");
        assert!(valid(&out));
    }

    #[test]
    fn delete_zone_removes_block_only() {
        let out = delete_zone(DOC, "main").unwrap();
        assert!(!out.contains("10.10.1.0/24"));
        assert!(out.contains("# 顶部注释"));
        assert!(out.contains("defaults:"), "别的段落不能被动到");
    }

    #[test]
    fn delete_missing_zone_is_an_error() {
        assert!(delete_zone(DOC, "nope").is_err());
    }

    #[test]
    fn gateway_fields_are_created_on_demand() {
        let out = set_egress(DOC, "main", false).unwrap();
        assert!(out.contains("gateway:"), "没有 gateway 时要自动建:\n{out}");
        assert!(out.contains("egress: false"));
        assert!(valid(&out));

        let out = add_pub(&out, "main", "{ proto: tcp, port: 2222, to: \"10.10.1.20:22\" }").unwrap();
        assert!(out.contains("port: 2222"));
        assert!(out.contains("# 顶部注释"), "多次编辑后注释仍在");
        assert!(valid(&out));

        let out = set_vpn(&out, "main", "wireguard", true).unwrap();
        assert!(out.contains("vpn:"));
        assert!(out.contains("wireguard: true"));
        assert!(valid(&out));

        let out = remove_pub(&out, "main", 0).unwrap();
        assert!(!out.contains("2222"), "删掉的那条不该还在:\n{out}");
        assert!(valid(&out));
    }

    #[test]
    fn remove_pub_out_of_range_is_rejected() {
        assert!(remove_pub(DOC, "main", 3).is_err());
    }

    #[test]
    fn vpn_kind_is_whitelisted() {
        assert!(set_vpn(DOC, "main", "ipsec", true).is_err());
    }

    #[test]
    fn mirrors_round_trip() {
        let out = add_mirror(DOC, "main:web", "main:analysis").unwrap();
        assert!(out.contains("mirrors:"));
        assert!(out.contains("from: main:web"));
        assert!(valid(&out));
        assert!(add_mirror(&out, "main:web", "main:analysis").is_err(), "重复连线要拒绝");
        let out = remove_mirror(&out, "main:web", "main:analysis").unwrap();
        assert!(!out.contains("main:web"));
    }

    /// 手写 YAML 常写裸 host（`to: cap`），而列表接口回传的是解析后的
    /// `zone:host`。两种写法必须互相认，否则连线"加得上、删不掉"。
    #[test]
    fn mirror_links_with_bare_hosts_are_matched() {
        let doc = r#"defaults:
  cores: 1
  memory: 512
  disk: 2
  template: debian-13-standard
  storage: local-lvm
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    hosts:
      gateway: { role: gateway }
      web: { role: server }
      cap: { role: analysis }
mirrors:
  - { from: web, to: cap }
"#;
        assert!(valid(doc), "样例本身要能解析");
        let out = remove_mirror(doc, "main:web", "main:cap").expect("裸 host 写法也必须能删");
        assert!(valid(&out));
        assert!(
            remove_mirror(&out, "main:web", "main:cap").is_err(),
            "删过一次之后就找不到了（说明第一次真的删掉了）:\n{out}"
        );
        assert!(
            add_mirror(doc, "main:web", "main:cap").is_err(),
            "等价写法应视为同一条连线，重复添加要拒绝"
        );
    }

    #[test]
    fn zone_snippet_round_trips_through_upsert() {
        let snip = zone_snippet(DOC, "main").unwrap().expect("main 存在");
        assert!(snip.contains("10.10.1.0/24"), "片段里要有 zone 的内容:\n{snip}");
        // 片段本身必须能被 upsert_zone 吃回去，且结果仍可解析
        let out = upsert_zone(DOC, "main", &snip).unwrap();
        assert!(valid(&out), "片段回写后必须仍可解析:\n{out}");
        assert!(out.contains("# 顶部注释"), "zone 之外的注释不该受影响:\n{out}");
        assert!(zone_snippet(DOC, "nope").unwrap().is_none());
    }
}
