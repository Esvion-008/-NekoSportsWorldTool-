//! 校园点位：POST /api/v560/get/1/distance/1（sportType=4）。
//!
//! runec = 信封(f"{uid}{经度6位}{纬度6位}{起始时间秒级ms整}"，insert→observed 序列化)；
//! sign = MD5(http版URL + 盐)。带 300s 缓存（限流 10603：5 分钟 3 次），失败回退最近缓存。

use super::client::ApiClient;
use super::model;
use crate::crypto::envelope::{build_envelope, OuterOrder};
use crate::crypto::sign::md5_url_sign;
use crate::location::Coordinate;
use serde_json::{json, Value};

pub const POINTS_PATH: &str = "/api/v560/get/1/distance/1";

/// 经纬度六位小数字符串。
fn six_digit(v: f64) -> String {
    format!("{v:.6}")
}

/// 拉取实时点位。**不读任何缓存**：每个账号都现拉最新点位，
/// 失败就重试 + 报错，绝不拿旧点位去生成轨迹。
pub fn fetch_points(
    client: &mut ApiClient,
    anchor: Coordinate,
    log: &mut dyn FnMut(&str),
) -> Result<Vec<Value>, String> {
    const ATTEMPTS: u32 = 12;
    // 首次请求前随机错开 0–1.2 秒：批量刷跑时几十个账号同时开跑，
    // 全撞在同一毫秒上最容易触发「5 分钟 3 次」限流。
    // 这段跑在任务线程里，不在 UI 线程，界面不会卡。
    let jitter_ms = (crate::crypto::envelope::now_ms() as u64 % 1200) as u64;
    if jitter_ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(jitter_ms));
    }
    let mut last = String::new();
    for attempt in 1..=ATTEMPTS {
        match fetch_points_ext(client, anchor, None, log) {
            Ok(points) if !points.is_empty() => return Ok(points),
            Ok(_) => {
                // 明确「无派发任务」，不用等
                return Err(NO_TASK.to_string());
            }
            Err(error) => {
                last = error.clone();
                // 限流（10603）：排队等，别硬撞；其他错误短等重试
                let limited = error.contains("10603") || error.contains("限流");
                let wait_s = if limited { 100 } else { 2 * attempt as u64 };
                if attempt == ATTEMPTS {
                    break;
                }
                log(&format!(
                    "[points] {}{} 秒后重试（{}/{}）",
                    if limited { "被限流，" } else { "请求失败，" },
                    wait_s,
                    attempt,
                    ATTEMPTS
                ));
                std::thread::sleep(std::time::Duration::from_secs(wait_s));
            }
        }
    }
    Err(format!(
        "{last}（已重试 {ATTEMPTS} 次；限流时每次间隔 100 秒）"
    ))
}

/// 进程内共享点位快照（批量刷跑时所有账号共用一份）。
struct SharedPoints {
    ts_ms: i64,
    latitude: f64,
    longitude: f64,
    points: Vec<Value>,
}

static SHARED_POINTS: std::sync::Mutex<Option<SharedPoints>> = std::sync::Mutex::new(None);

/// 批量刷跑专用：同锚点的点位在进程内只真正请求一次。
///
/// 点位接口限流为「5 分钟 3 次」（10603），两百个账号并发各请求一次必然被限流。
/// 这里用一把进程级互斥锁把并发收敛成「第一个线程请求、其余线程等结果」，
/// 并且只在锁内做一次网络请求，天然避免惊群。
pub fn fetch_points_shared(
    client: &mut ApiClient,
    anchor: Coordinate,
    log: &mut dyn FnMut(&str),
) -> Result<Vec<Value>, String> {
    // **不做任何共享**：服务端是按「每个账号自己那组点位」校验的，
    // 复用别的账号拉到的点位，提交时就会被判「没有打卡点」。
    // 限流交给 fetch_points 里的排队重试处理。
    let _ = &SHARED_POINTS;
    fetch_points(client, anchor, log)
}

/// run_area_id：学校配置了区域时 App 会附带；未配置则不传（与 App 一致）。
pub fn fetch_points_ext(
    client: &mut ApiClient,
    anchor: Coordinate,
    run_area_id: Option<String>,
    log: &mut dyn FnMut(&str),
) -> Result<Vec<Value>, String> {
    anchor.validate()?;
    // ① 不再读磁盘缓存：每个账号都现拉，保证拿到的就是服务端此刻派发的点位。
    // ② 请求接口
    let uid = client.login.as_ref().map(|s| s.uid).unwrap_or(0);
    let unid = client
        .login
        .as_ref()
        .map(|s| s.unid.clone())
        .unwrap_or_else(|| "0".into());
    let lat = anchor.latitude;
    let lon = anchor.longitude;
    let url = format!("{}{}", model::HOST, POINTS_PATH);

    let start_ms = crate::crypto::envelope::now_ms();
    let runec_input = format!("{uid}{}{}{}", six_digit(lon), six_digit(lat), (start_ms / 1000) * 1000);
    // runec：共用会话 Four，外层 observed 序
    let runec_env = build_envelope(&mut client.session, &runec_input, OuterOrder::Observed);
    let runec = runec_env.json;

    let mut body = json!({
        "sportType": 4,
        "longitude": lon,
        "latitude": lat,
        "sign": md5_url_sign(&url),
        "uuid": uuid::Uuid::new_v4().to_string(),
        "selectedUnid": unid,
        "runec": runec,
    });
    if let Some(area) = run_area_id {
        body["runAreaId"] = json!(area);
    }
    let body = body.to_string();

    let out = client.envelope_request("POST", &url, &body, crate::crypto::header::UA_IOS, &[])?;
    // 接口失败**不回退任何缓存**：宁可不跑，也不拿旧点位生成一条对不上的轨迹。
    // （用户明确要求：每个账号都要拿服务端此刻的真实点位。）
    let fallback = |log: &mut dyn FnMut(&str)| -> Result<Vec<Value>, String> {
        log("[points] 接口失败，按「不读缓存」策略直接报错");
        Err("点位接口失败（不读缓存，避免拿旧点位生成对不上的轨迹）；稍后重试即可".into())
    };
    let Some(dec) = out.decrypted else {
        return fallback(log);
    };
    let payload = &dec.business;
    // pointsResModels：优先顶层，其次 data 子对象
    let pts: Vec<Value> = super::client::get_field(payload, "pointsResModels")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();
    if !pts.is_empty() {
        // 点位数不是恒定 5 个：服务端按账号/时段派发，App 显示几个就刷几个。
        // 这里只做中性播报，别写成「警告」——4 个点是正常现象，不是故障。
        let names: Vec<String> = pts
            .iter()
            .filter_map(|p| p.get("pointName").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .collect();
        log(&format!(
            "[points] 本次下发 {} 个打卡点（{}）→ 按实际点数生成轨迹",
            pts.len(),
            names.join(" / ")
        ));
        // 每个点再打一行关键标记：App 少显示点位时，靠这行对一下是哪个点被 App 过滤了
        for point in &pts {
            let name = point.get("pointName").and_then(|v| v.as_str()).unwrap_or("?");
            let fixed = point.get("isFixed").map(|v| v.to_string()).unwrap_or_default();
            let state = point.get("state").map(|v| v.to_string()).unwrap_or_default();
            let ptype = point.get("type").map(|v| v.to_string()).unwrap_or_default();
            let area = point
                .get("runAreaId")
                .or_else(|| point.get("areaId"))
                .map(|v| v.to_string())
                .unwrap_or_default();
            log(&format!(
                "  [points] {name} isFixed={fixed} state={state} type={ptype} area={area}"
            ));
            // 完整字段（截断），App 少画一个点时靠这行对一下那个点带了什么标记
            let raw = serde_json::to_string(point).unwrap_or_default();
            let preview: String = raw.chars().take(320).collect();
            log(&format!("  [points]   raw={preview}"));
        }
        let _ = model::save_points_cache(anchor, &pts);
        return Ok(pts);
    }
    let err = payload.get("error").and_then(|e| e.as_i64()).unwrap_or(0);
    let message = payload
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    log(&format!("[points] 接口无点位 error={err}: {message}"));
    // 接口正常返回但一个点都没派发 = 这个账号当前没有任务（本学期已完成 / 未到开放时间）。
    // 这里绝对不能回退旧缓存：拿旧点位去提交只会得到一条「没有打卡点」的失败记录，
    // 批量刷跑时这类账号应当按「跳过」处理，而不是算失败。
    if err == 0 {
        let suffix = if message.is_empty() {
            String::new()
        } else {
            format!("：{message}")
        };
        return Err(format!(
            "{NO_TASK} 该账号当前没有可跑任务（本学期已完成或未到开放时间）{suffix}"
        ));
    }
    fallback(log)
}

/// 「当前没有派发打卡点」的专用前缀：不是失败，批量刷跑里应当按「跳过」统计。
pub const NO_TASK: &str = "NO_TASK";

/// 点位中心（BD 系）。
#[allow(dead_code)]
pub fn center_bd(points: &[Value]) -> (f64, f64) {
    let n = points.len().max(1) as f64;
    let lat = points.iter().filter_map(|p| p.get("lat").and_then(|v| v.as_f64())).sum::<f64>() / n;
    let lon = points.iter().filter_map(|p| p.get("lon").and_then(|v| v.as_f64())).sum::<f64>() / n;
    (lat, lon)
}

/// 点位里偶发把坐标发成字符串（`"39.917678"`），两种都要认。
///
/// 原来只认 `as_f64()`，一旦服务端改发字符串，那个点会被 `filter_map` **静默丢掉**，
/// 5 个打卡点就变成 4 个，最后轨迹少打一个点、提交被判缺卡。
fn numeric(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
}

/// 点位 → (lat, lon) BD 系数组（轨迹输入）。
pub fn points_bd(points: &[Value]) -> Vec<(f64, f64)> {
    points
        .iter()
        .filter_map(|p| {
            let lat = numeric(p.get("lat")?)?;
            let lon = numeric(p.get("lon")?)?;
            Some((lat, lon))
        })
        .collect()
}
