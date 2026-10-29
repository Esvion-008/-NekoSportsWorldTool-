//! `campus.json` 持久化与版本快照。
//!
//! 每次保存：原子写入 `campus.json`，并在 `campus_versions/` 写一份快照；
//! 只保留最近 [`SNAPSHOT_KEEP`] 份，供历史轨迹复现。

use std::fs;
use std::path::{Path, PathBuf};

use super::{Campus, SCHEMA_VERSION, SNAPSHOT_KEEP};

pub const CAMPUS_FILE: &str = "campus.json";
pub const SNAPSHOT_DIR: &str = "campus_versions";

/// 保存结果：主文件、快照与不影响保存成功的警告。
#[derive(Clone, Debug)]
pub struct SaveOutcome {
    pub path: PathBuf,
    pub snapshot: Option<PathBuf>,
    pub warnings: Vec<String>,
}

/// 历史快照元信息。
#[derive(Clone, Debug)]
pub struct SnapshotMeta {
    pub path: PathBuf,
    pub name: String,
    pub updated_ms: i64,
    pub bytes: u64,
}

/// 从默认数据目录载入 campus.json；不存在返回 `Ok(None)`。
pub fn load() -> Result<Option<Campus>, String> {
    load_from(&crate::platform::data_dir())
}

/// 从指定目录载入 campus.json。
pub fn load_from(dir: &Path) -> Result<Option<Campus>, String> {
    let path = dir.join(CAMPUS_FILE);
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("读取 {} 失败：{e}", path.display()))?;
    let campus: Campus = serde_json::from_str(&text)
        .map_err(|e| format!("解析 {} 失败：{e}", path.display()))?;
    campus
        .validate_basic()
        .map_err(|e| format!("{} 数据不合法：{e}", path.display()))?;
    Ok(Some(campus))
}

/// 保存到默认数据目录。
pub fn save(campus: &mut Campus) -> Result<SaveOutcome, String> {
    save_at(
        &crate::platform::data_dir(),
        campus,
        crate::crypto::envelope::now_ms(),
    )
}

/// 保存到指定目录，并生成版本快照。
pub fn save_at(dir: &Path, campus: &mut Campus, now_ms: i64) -> Result<SaveOutcome, String> {
    campus.version = SCHEMA_VERSION;
    campus.updated_ms = now_ms.max(campus.created_ms);
    campus.validate_basic()?;

    fs::create_dir_all(dir).map_err(|e| format!("创建数据目录 {} 失败：{e}", dir.display()))?;
    let path = dir.join(CAMPUS_FILE);
    let text = serde_json::to_string_pretty(campus)
        .map_err(|e| format!("序列化 campus.json 失败：{e}"))?;

    // 先写临时文件再改名，异常时不会留下半截 campus.json
    let tmp = dir.join(format!(".{CAMPUS_FILE}.tmp-{}", std::process::id()));
    fs::write(&tmp, text.as_bytes())
        .map_err(|e| format!("写入临时文件 {} 失败：{e}", tmp.display()))?;
    if let Err(e) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("写入 {} 失败：{e}", path.display()));
    }

    let mut warnings = Vec::new();
    let snapshot = match write_snapshot(dir, &text, campus.updated_ms) {
        Ok(path) => Some(path),
        Err(e) => {
            warnings.push(format!("版本快照写入失败：{e}"));
            None
        }
    };
    if let Err(e) = prune_snapshots(dir, SNAPSHOT_KEEP) {
        warnings.push(format!("清理旧快照失败：{e}"));
    }
    Ok(SaveOutcome {
        path,
        snapshot,
        warnings,
    })
}

/// 列出现有快照，按时间从新到旧。
pub fn list_snapshots(dir: &Path) -> Vec<SnapshotMeta> {
    let snapshot_dir = dir.join(SNAPSHOT_DIR);
    let entries = match fs::read_dir(&snapshot_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut items = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };
        if !name.starts_with("campus_") || !name.ends_with(".json") {
            continue;
        }
        let updated_ms = parse_snapshot_time(&name).unwrap_or(0);
        let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
        items.push(SnapshotMeta {
            path,
            name,
            updated_ms,
            bytes,
        });
    }
    items.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms).then_with(|| b.name.cmp(&a.name)));
    items
}

fn snapshot_dir(dir: &Path) -> PathBuf {
    dir.join(SNAPSHOT_DIR)
}

fn write_snapshot(dir: &Path, text: &str, updated_ms: i64) -> Result<PathBuf, String> {
    let snapshot_dir = snapshot_dir(dir);
    fs::create_dir_all(&snapshot_dir)
        .map_err(|e| format!("创建快照目录 {} 失败：{e}", snapshot_dir.display()))?;
    let mut path = snapshot_dir.join(format!("campus_{updated_ms:013}.json"));
    let mut suffix = 1u32;
    while path.exists() {
        path = snapshot_dir.join(format!("campus_{updated_ms:013}_{suffix}.json"));
        suffix += 1;
        if suffix > 1_000 {
            return Err("快照文件名冲突过多".into());
        }
    }
    fs::write(&path, text.as_bytes())
        .map_err(|e| format!("写入快照 {} 失败：{e}", path.display()))?;
    Ok(path)
}

fn prune_snapshots(dir: &Path, keep: usize) -> Result<(), String> {
    let snapshot_dir = snapshot_dir(dir);
    let entries = match fs::read_dir(&snapshot_dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(()),
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|name| name.starts_with("campus_") && name.ends_with(".json"))
                    .unwrap_or(false)
        })
        .collect();
    if files.len() <= keep {
        return Ok(());
    }
    files.sort();
    let remove_count = files.len() - keep;
    for path in files.into_iter().take(remove_count) {
        fs::remove_file(&path)
            .map_err(|e| format!("删除旧快照 {} 失败：{e}", path.display()))?;
    }
    Ok(())
}

fn parse_snapshot_time(name: &str) -> Option<i64> {
    let rest = name.strip_prefix("campus_")?;
    let digits = rest.split(['.', '_']).next()?;
    digits.parse::<i64>().ok()
}

/// 直接读取一份快照（版本回看用）。
pub fn read_snapshot(path: &Path) -> Result<Campus, String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("读取快照 {} 失败：{e}", path.display()))?;
    let campus: Campus = serde_json::from_str(&text)
        .map_err(|e| format!("解析快照 {} 失败：{e}", path.display()))?;
    campus.validate_basic()?;
    Ok(campus)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::campus::Polygon;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "greenland-campus-{tag}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("创建测试目录失败");
        dir
    }

    fn sample_campus() -> Campus {
        Campus::new(
            Polygon::new(vec![
                [114.298, 30.498],
                [114.302, 30.498],
                [114.302, 30.502],
                [114.298, 30.502],
            ]),
            1_000,
        )
    }

    #[test]
    fn save_and_load_round_trip_creates_snapshot() {
        let dir = temp_dir("roundtrip");
        let mut campus = sample_campus();
        campus.base_altitude_m = 88.5;
        let outcome = save_at(&dir, &mut campus, 2_000).unwrap();
        assert!(outcome.path.exists());
        assert!(outcome.snapshot.as_ref().unwrap().exists());
        assert_eq!(outcome.warnings, Vec::<String>::new());

        let loaded = load_from(&dir).unwrap().expect("应能载入");
        assert_eq!(loaded, campus);
        assert_eq!(loaded.updated_ms, 2_000);
        assert_eq!(loaded.base_altitude_m, 88.5);
        let snapshots = list_snapshots(&dir);
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].updated_ms, 2_000);
        assert!(snapshots[0].bytes > 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn snapshots_are_pruned_to_recent_versions() {
        let dir = temp_dir("prune");
        let mut campus = sample_campus();
        for i in 0..(SNAPSHOT_KEEP as i64 + 5) {
            save_at(&dir, &mut campus, 1_000 + i * 1_000).unwrap();
        }
        let snapshots = list_snapshots(&dir);
        assert_eq!(snapshots.len(), SNAPSHOT_KEEP, "快照数量应保留最近 {SNAPSHOT_KEEP} 份");
        // 最新快照时间应为最后一次保存
        assert_eq!(
            snapshots[0].updated_ms,
            1_000 + (SNAPSHOT_KEEP as i64 + 4) * 1_000
        );
        // 主文件仍可读
        assert!(load_from(&dir).unwrap().is_some());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_file_is_none() {
        let dir = temp_dir("missing");
        assert!(load_from(&dir).unwrap().is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupted_file_reports_readable_error() {
        let dir = temp_dir("corrupt");
        fs::write(dir.join(CAMPUS_FILE), b"{ this is not json }").unwrap();
        let error = load_from(&dir).unwrap_err();
        assert!(error.contains("解析"), "error={error}");
        assert!(error.contains(CAMPUS_FILE), "error={error}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let dir = temp_dir("version");
        let mut campus = sample_campus();
        campus.version = 99;
        fs::write(
            dir.join(CAMPUS_FILE),
            serde_json::to_string_pretty(&campus).unwrap(),
        )
        .unwrap();
        let error = load_from(&dir).unwrap_err();
        assert!(error.contains("版本"), "error={error}");
        let _ = fs::remove_dir_all(dir);
    }
}
