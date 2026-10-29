//! 数据模型与本地持久化。
//!
//! 持久化文件位于桌面 exe 目录或 Android 应用私有目录：identity.json（设备身份，device_id 固定复用
//! ——严禁每次随机，会触发 10121 风控）、session.json（登录态）、config.json
//! （账号/参数）、points_cache.json（点位缓存）。

pub use crate::crypto::header::HeaderIdentity;

pub const HOST: &str = "https://run.gxapp.iydsj.com";
/// 排行榜 / 违规名单域名（信封链与 RUN 相同）。
pub const DISCOVERY: &str = "https://discovery.gxapp.iydsj.com";

/// 登录态（session.json）。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Session {
    #[serde(default)]
    pub uid: i64,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub unid: String,
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_weight")]
    pub weight: f64,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub device_id: String,
    /// 登录响应完整业务数据（我的页展示）
    #[serde(default)]
    pub profile: serde_json::Value,
}

fn default_weight() -> f64 {
    68.0
}

impl Session {
    pub fn is_logged_in(&self) -> bool {
        self.uid >= 1 && !self.token.is_empty()
    }
}

/// 批量账号条目（accounts.json）。
///
/// 一个条目 = 一组账号密码 + 它自己的登录态；批量登录 / 批量刷跑都以它为单位。
/// `session` 为 `None` 表示该账号当前未登录。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Account {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// 登录成功后的会话（uid/token/unid/name/weight）。
    #[serde(default)]
    pub session: Option<Session>,
    /// 该账号的专属设备身份（批量多账号防关联；None = 尚未登录过，用时按账号名派生）。
    #[serde(default)]
    pub identity: Option<HeaderIdentity>,
    /// 最近一次操作失败的提示（空串表示没有失败记录）。
    #[serde(default)]
    pub last_error: String,
    /// 平台标记/风控原因（空 = 正常）。命中后账号行标红。
    #[serde(default)]
    pub flagged_reason: String,
    /// 预计解冻时间（毫秒时间戳；0 = 未知）。账号行显示「还剩多久」。
    #[serde(default)]
    pub flagged_until_ms: i64,
    /// 最近一次登录成功的毫秒时间戳（0 表示从未成功）。
    #[serde(default)]
    pub last_login_ms: i64,
    /// 最近一次刷跑结果摘要（仅展示用）。
    #[serde(default)]
    pub last_run: String,
    /// 用户备注（自由填写，仅本地展示，随 accounts.json 持久化）。
    #[serde(default)]
    pub remark: String,
    /// 学期有效里程（公里；由「刷新达标与公里」拉取，随 accounts.json 持久化）。
    #[serde(default)]
    pub semester_valid_km: f64,
    /// 学期目标里程（公里；已按 48 公里规则收敛，随 accounts.json 持久化）。
    #[serde(default)]
    pub semester_goal_km: f64,
    /// 学期目标异常标记（接口返回值 > 64000 米）。
    #[serde(default)]
    pub goal_abnormal: bool,
    /// 客户已购公里（手动记账：客户按 10 公里等分批付了多少）。
    #[serde(default)]
    pub bought_km: f64,
    /// 每账号固定设备平台："" = 跟随设备页；"ios" / "android" = 固定不变。
    #[serde(default)]
    pub device_platform: String,
    /// 每账号固定机型："" = 按平台自动挑选（账号名确定性）。
    #[serde(default)]
    pub device_model: String,
    /// 每账号固定系统版本："" = 按平台自动挑选。
    #[serde(default)]
    pub device_os: String,
    /// 该账号专属代理（socks5:// 或 http://）；空 = 用全局代理/直连。
    #[serde(default)]
    pub proxy: String,
    /// 违规自查结果：None = 未查；Some(true) = 干净；Some(false) = 已被标记。
    #[serde(default)]
    pub cheat_flag: Option<bool>,
    /// 违规自查时间戳（毫秒）。
    #[serde(default)]
    pub cheat_ms: i64,
    /// 违规标记原因（自查返回）。
    #[serde(default)]
    pub cheat_reason: String,
    /// 自动定时刷跑：该账号最近一次自动跑过的日期（YYYY-MM-DD）。
    #[serde(default)]
    pub auto_done_date: String,
    /// 学期公里拉取时间戳（毫秒；0 = 从未拉取）。
    #[serde(default)]
    pub semester_checked_ms: i64,
    /// 登录成功后记录的真实名字/昵称（登出后保留，便于管理识别）。
    #[serde(default)]
    pub name: String,
    /// 批量刷跑勾选状态（默认勾选；随 accounts.json 持久化）。
    #[serde(default = "default_selected")]
    pub selected: bool,
    /// 当日达标检查结果（None = 未查询）。
    #[serde(default)]
    pub today_ok: Option<bool>,
    /// 达标检查时间戳（毫秒；非当日检查结果不展示）。
    #[serde(default)]
    pub today_checked_ms: i64,
}

fn default_selected() -> bool {
    true
}

impl Account {
    pub fn logged_in(&self) -> bool {
        self.session.as_ref().map(|s| s.is_logged_in()).unwrap_or(false)
    }
}

/// UI 参数（config.json）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Config {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub remember: bool,
    #[serde(default = "default_f32")]
    pub dist_min: f32,
    #[serde(default = "default_f32")]
    pub dist_max: f32,
    #[serde(default = "default_pace_min")]
    pub pace_min: f32,
    #[serde(default = "default_pace_max")]
    pub pace_max: f32,
    #[serde(default)]
    pub face_check: bool,
    /// 跑步时覆盖轨迹点 bdA；为空表示使用生成器的海拔曲线。
    #[serde(default)]
    pub manual_altitude: Option<f64>,
    /// 跑步时将生成器海拔曲线映射到此范围；为空表示不使用范围覆盖。
    #[serde(default)]
    pub manual_altitude_range: Option<crate::track::altitude::AltitudeRange>,
    #[serde(default = "default_ai_minutes")]
    pub ai_minutes: i64,
    #[serde(default = "default_ai_reps")]
    pub ai_reps: i64,
    /// 启动时检查更新：silent（静默，发现新版才弹窗）/ ask（每次询问）/ off
    #[serde(default = "default_update_check")]
    pub update_check: String,
    /// 批量登录 / 批量刷跑并发数（1-32）。
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    /// 全局默认代理（socks5:// 或 http://）；账号自己填了用账号的。
    #[serde(default)]
    pub proxy: String,
    /// 定时分散刷跑开关。
    #[serde(default)]
    pub auto_run: bool,
    /// 定时窗口：起始小时（含）。
    #[serde(default = "default_auto_start")]
    pub auto_start_hour: i64,
    /// 定时窗口：结束小时（不含）。
    #[serde(default = "default_auto_end")]
    pub auto_end_hour: i64,
    /// 两次触发间隔下限（分钟）。
    #[serde(default = "default_auto_gap_min")]
    pub auto_gap_min: i64,
    /// 两次触发间隔上限（分钟）。
    #[serde(default = "default_auto_gap_max")]
    pub auto_gap_max: i64,
    /// 每次触发跑几个账号。
    #[serde(default = "default_auto_batch")]
    pub auto_batch: i64,
    /// 下次触发时间戳（毫秒，运行期状态）。
    #[serde(default)]
    pub auto_next_ms: i64,
    /// 路线引擎：v1（拟合环，默认）/ v2（校园路网）。
    #[serde(default = "default_route_engine")]
    pub route_engine: String,
}

fn default_route_engine() -> String {
    "v4".into()
}

fn default_concurrency() -> usize {
    5
}
fn default_auto_start() -> i64 {
    7
}
fn default_auto_end() -> i64 {
    20
}
fn default_auto_gap_min() -> i64 {
    8
}
fn default_auto_gap_max() -> i64 {
    20
}
fn default_auto_batch() -> i64 {
    1
}

fn default_f32() -> f32 {
    1.0
}
fn default_pace_min() -> f32 {
    360.0
}
fn default_pace_max() -> f32 {
    480.0
}
fn default_ai_minutes() -> i64 {
    1
}
fn default_ai_reps() -> i64 {
    5
}
fn default_update_check() -> String {
    "silent".into()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            username: String::new(),
            password: String::new(),
            remember: false,
            dist_min: 1.0,
            dist_max: 1.5,
            pace_min: default_pace_min(),
            pace_max: default_pace_max(),
            face_check: true,
            manual_altitude: None,
            manual_altitude_range: None,
            ai_minutes: default_ai_minutes(),
            ai_reps: default_ai_reps(),
            update_check: default_update_check(),
            concurrency: default_concurrency(),
            proxy: String::new(),
            auto_run: false,
            auto_start_hour: default_auto_start(),
            auto_end_hour: default_auto_end(),
            auto_gap_min: default_auto_gap_min(),
            auto_gap_max: default_auto_gap_max(),
            auto_batch: default_auto_batch(),
            auto_next_ms: 0,
            route_engine: default_route_engine(),
        }
    }
}

// ── 持久化（平台数据目录）──────────────────────────────────────

fn exe_dir() -> std::path::PathBuf {
    crate::platform::data_dir()
}

/// 兜底目录：数据目录写不进去时改写到 exe 同目录（至少不让账号凭空消失）。
fn fallback_dir() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let parent = exe.parent()?.to_path_buf();
    if parent == exe_dir() {
        None
    } else {
        Some(parent)
    }
}

fn read_json<T: serde::de::DeserializeOwned>(name: &str) -> Option<T> {
    // 先读主数据目录，再读兜底目录（写失败时会落到那里）
    for base in std::iter::once(exe_dir()).chain(fallback_dir()) {
        let path = base.join(name);
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(value) = serde_json::from_str(&text) {
                return Some(value);
            }
        }
    }
    None
}

fn write_json<T: serde::Serialize>(name: &str, value: &T) -> Result<(), String> {
    let json =
        serde_json::to_string_pretty(value).map_err(|e| format!("序列化 {name} 失败: {e}"))?;
    let primary = exe_dir().join(name);
    match std::fs::write(&primary, &json) {
        Ok(()) => return Ok(()),
        Err(primary_error) => {
            // 主数据目录写不进去（权限/占用）时改写到 exe 同目录，
            // 宁可换个位置存，也不能让用户加了账号却一条都没落盘。
            if let Some(dir) = fallback_dir() {
                let alt = dir.join(name);
                if std::fs::write(&alt, &json).is_ok() {
                    return Err(format!(
                        "主目录写入失败（{primary_error}），已改写到 {}；下次启动会优先读主目录，请在软件里确认数据是否到位",
                        alt.display()
                    ));
                }
            }
            Err(format!("写入 {} 失败: {primary_error}", primary.display()))
        }
    }
}

/// 加载设备身份；device_id / app_install_time 缺失时生成一次并立即落盘，
/// 此后同一设备全生命周期复用（逐请求漂移会影响设备一致性）。
pub fn load_identity() -> HeaderIdentity {
    load_identity_for_platform(if cfg!(target_os = "android") { "android" } else { "ios" })
}

fn load_identity_for_platform(platform: &str) -> HeaderIdentity {
    let mut id: HeaderIdentity = read_json("identity.json").unwrap_or_else(|| {
        let mut identity = HeaderIdentity::default();
        if platform == "android" {
            identity.platform = "android".into();
            // Generic fallback until the user opts in or fills the fields manually.
            identity.device_name = "Android".into();
            identity.os_version = "16".into();
        }
        identity
    });
    let mut dirty = false;
    if id.device_id.is_empty() {
        id.device_id = uuid::Uuid::new_v4().to_string().to_uppercase();
        dirty = true;
    }
    if id.app_install_time <= 0 {
        id.app_install_time =
            crate::crypto::header::HeaderIdentity::fresh_install_time(&id.platform);
        dirty = true;
    }
    if id.mac_address.is_empty() {
        id.mac_address = crate::crypto::header::random_mac();
        dirty = true;
    }
    if dirty {
        let _ = save_identity(&id);
    }
    id
}

#[cfg(any(target_os = "android", test))]
#[derive(Debug, serde::Deserialize)]
pub struct DeviceInfo {
    pub manufacturer: String,
    pub model: String,
    pub os_version: String,
}

#[cfg(any(target_os = "android", test))]
pub fn identity_with_device_info(identity: &HeaderIdentity, info: &DeviceInfo) -> Result<HeaderIdentity, String> {
    if info.model.trim().is_empty() || info.os_version.trim().is_empty() {
        return Err("未能读取完整的机型和系统版本，请手动填写".into());
    }
    let mut updated = identity.clone();
    updated.platform = "android".into();
    updated.device_name = info.model.trim().into();
    updated.manufacturer = info.manufacturer.trim().into();
    updated.os_version = info.os_version.trim().into();
    Ok(updated)
}

pub fn save_identity(id: &HeaderIdentity) -> Result<(), String> {
    write_json("identity.json", id)
}

/// AI 项目列表永久缓存：拉取成功即落盘，网络异常时兜底展示。
pub fn load_ai_sports() -> Option<Vec<crate::api::ai::AiSport>> {
    read_json("ai_sports.json")
}

pub fn save_ai_sports(list: &[crate::api::ai::AiSport]) -> Result<(), String> {
    write_json("ai_sports.json", &list.to_vec())
}

pub fn load_session() -> Session {
    read_json("session.json").unwrap_or_default()
}

pub fn save_session(s: &Session) -> Result<(), String> {
    write_json("session.json", s)
}

pub fn clear_session() {
    let _ = std::fs::remove_file(exe_dir().join("session.json"));
}

pub fn load_config() -> Config {
    let mut config: Config = read_json("config.json").unwrap_or_default();
    // V4 无敌算法为默认；旧配置的 v1 自动迁移到 v4。
    config.route_engine = "v4".into();
    config
}

pub fn save_config(c: &Config) -> Result<(), String> {
    write_json("config.json", c)
}

// ── 批量账号（accounts.json）──────────────────────────────────

/// 批量账号上限：导入与手动添加都不得超过。
pub const MAX_ACCOUNTS: usize = 200;

/// 读取批量账号列表（文件缺失或损坏时返回空表）。
pub fn load_accounts() -> Vec<Account> {
    let mut list: Vec<Account> = read_json("accounts.json").unwrap_or_default();
    list.truncate(MAX_ACCOUNTS);
    list
}

pub fn save_accounts(list: &[Account]) -> Result<(), String> {
    write_json("accounts.json", &list.to_vec())
}

#[cfg(test)]
mod storage_tests {
    use super::*;

    #[test]
    fn android_identity_import_persists_brand_and_reuses_uuid() {
        let directory = std::env::temp_dir().join(format!("greenland-identity-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = Some(directory.clone()));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = None);
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory);
        let first = load_identity_for_platform("android");
        assert_eq!(first.platform, "android");
        assert_ne!(first.device_name, "iPhone");
        assert!(uuid::Uuid::parse_str(&first.device_id).is_ok());
        let again = load_identity_for_platform("android");
        assert_eq!(serde_json::to_value(&first).unwrap(), serde_json::to_value(&again).unwrap());

        let mut existing = first.clone();
        existing.platform = "ios".into();
        existing.device_name = "Existing manual name".into();
        existing.idfa = "manual-value".into();
        let mut legacy = serde_json::to_value(&existing).unwrap();
        legacy.as_object_mut().unwrap().remove("manufacturer");
        write_json("identity.json", &legacy).unwrap();
        let loaded = load_identity_for_platform("android");
        assert_eq!(serde_json::to_value(&loaded).unwrap(), serde_json::to_value(&existing).unwrap(),
            "opening an existing install must not overwrite a manual identity");

        let info: DeviceInfo = serde_json::from_str(
            r#"{"manufacturer":"Example","model":"Phone 16","os_version":"16"}"#,
        ).unwrap();
        assert_eq!(info.manufacturer, "Example");
        let imported = identity_with_device_info(&loaded, &info).unwrap();
        assert_eq!(imported.platform, "android");
        assert_eq!(imported.device_name, "Phone 16");
        assert_eq!(imported.os_version, "16");
        let mut expected = serde_json::to_value(&existing).unwrap();
        expected["platform"] = "android".into();
        expected["device_name"] = "Phone 16".into();
        expected["manufacturer"] = "Example".into();
        expected["os_version"] = "16".into();
        assert_eq!(serde_json::to_value(&imported).unwrap(), expected,
            "import must preserve UUID, MAC, manual IMEI/IDFA, install time and location");
        assert_eq!(load_identity().device_name, existing.device_name, "preview must not save implicitly");
        save_identity(&imported).unwrap();
        let stored: serde_json::Value = read_json("identity.json").unwrap();
        assert_eq!(stored.get("manufacturer").and_then(|value| value.as_str()), Some("Example"),
            "the consented brand must be written to identity.json");
        let reloaded = load_identity();
        assert_eq!(serde_json::to_value(&reloaded).unwrap(), serde_json::to_value(&imported).unwrap(),
            "brand and identity must survive a fresh load from disk");
        assert_eq!(reloaded.device_id, first.device_id);
        let (header, _) = crate::crypto::header::build_header_for(&reloaded, 0, "", Some(1_700_000_000_000));
        let header: serde_json::Value = serde_json::from_str(&header).unwrap();
        assert_eq!(header["DeviceId"], first.device_id);
        assert_eq!(header["deviceName"], "Phone 16", "brand must not be prepended to the protocol model field");
        assert_eq!(header["osVersion"], "16");
        assert!(header.get("manufacturer").is_none() && header.get("brand").is_none(),
            "persisting local brand metadata must not invent new protocol fields");
    }

    #[test]
    fn incomplete_native_device_info_cannot_replace_identity() {
        let identity = HeaderIdentity::default();
        for (model, os_version) in [("", "16"), ("Phone", " ")] {
            let info = DeviceInfo { manufacturer: String::new(), model: model.into(), os_version: os_version.into() };
            assert!(identity_with_device_info(&identity, &info).is_err());
        }
    }

    #[test]
    fn session_roundtrip_and_logout_use_private_data_directory() {
        let directory = std::env::temp_dir().join(format!("greenland-storage-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = Some(directory.clone()));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = None);
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory.clone());
        assert_eq!(exe_dir(), directory, "storage must use the injected app directory");
        let session = Session { uid: 123, token: "test-only-token".into(), ..Default::default() };
        save_session(&session).unwrap();
        assert!(directory.join("session.json").is_file());
        assert_eq!(load_session().uid, 123);
        assert_eq!(load_session().token, session.token);
        clear_session();
        assert!(!directory.join("session.json").exists());
        assert_eq!(load_session().uid, 0);
    }
}

/// 点位缓存：{ts_ms, points}，TTL 300s（服务端限流 10603：5 分钟 3 次）。
pub const POINTS_TTL_MS: i64 = 300_000;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct PointsCache {
    ts: i64,
    anchor: crate::location::Coordinate,
    points: Vec<serde_json::Value>,
}

/// 只返回与本次锚点相同的缓存。旧版未记录锚点的缓存会自然失效，避免串城市。
pub fn load_points_cache_for(anchor: crate::location::Coordinate) -> Option<(i64, Vec<serde_json::Value>)> {
    let v: serde_json::Value = read_json("points_cache.json")?;
    let cache: PointsCache = serde_json::from_value(v).ok()?;
    if !cache.anchor.is_near(anchor, 0.0001) { return None; }
    let ts = cache.ts;
    let pts = cache.points;
    Some((ts, pts))
}

pub fn save_points_cache(anchor: crate::location::Coordinate, points: &[serde_json::Value]) -> Result<(), String> {
    let doc = PointsCache { ts: crate::crypto::envelope::now_ms(), anchor, points: points.to_vec() };
    write_json("points_cache.json", &doc)
}

/// 预览用：不校验锚点，直接取最近一次点位缓存。
pub fn load_points_cache_any() -> Option<Vec<serde_json::Value>> {
    let value: serde_json::Value = read_json("points_cache.json")?;
    value
        .get("points")
        .and_then(|points| points.as_array())
        .cloned()
}

#[cfg(test)]
mod points_cache_tests {
    use super::*;
    #[test]
    fn cache_is_scoped_to_anchor() {
        let cache = PointsCache { ts: 1, anchor: crate::location::Coordinate::new(39.9, 116.4, 0.0).unwrap(), points: vec![] };
        let decoded: PointsCache = serde_json::from_value(serde_json::to_value(cache).unwrap()).unwrap();
        assert!(decoded.anchor.is_near(crate::location::Coordinate::new(39.9, 116.4, 0.0).unwrap(), 0.0001));
        assert!(!decoded.anchor.is_near(crate::location::Coordinate::new(38.9, 121.5, 0.0).unwrap(), 0.0001));
    }
}
