//! 学期完成度。
//! 汇总：POST /api/v55/runnings/recordssummary/semester {}；
//! 个人完成度：POST /api/v41/running/getPersonalSemesterInfo {}（结构按实际响应落地）。

use super::client::ApiClient;
use serde_json::{json, Value};

pub const SUMMARY_PATH: &str = "/api/v55/runnings/recordssummary/semester";
pub const PERSONAL_PATH: &str = "/api/v41/running/getPersonalSemesterInfo";

/// 学期汇总（B1 实测字段）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SemesterSummary {
    pub sname: String,
    pub semester_dis: f64,
    pub semester_valid_dis: f64,
    pub semester_count: i64,
    pub semester_valid_count: i64,
}

/// 仅拉取里程数据：返回（有效里程公里, 学期目标公里；未知为 0）。
/// 批量账号列表的「公里」列使用；比完整 query 少 1-2 个请求。
/// 有效里程多源取最大值：汇总接口 / 个人完成度接口 / 完成状态接口
/// （部分学校只有完成状态接口返回真实有效里程，需最近记录的 rrid）。
pub fn query_km(
    client: &mut ApiClient,
    log: &mut dyn FnMut(&str),
    latest_rrid: Option<i64>,
) -> (f64, f64) {
    let mut valid = 0.0f64;
    let mut goal = 0.0f64;
    let take_km = |v: &serde_json::Value, keys: &[&str]| -> f64 {
        for k in keys {
            if let Some(f) = v.get(k).and_then(|x| x.as_f64()) {
                return f / 1000.0;
            }
        }
        0.0
    };
    match client.call("POST", SUMMARY_PATH, "{}", &[]) {
        Ok(biz) => {
            let d = super::client::parse_data_field(&biz);
            valid = take_km(&d, &["semesterValidDis"]);
        }
        Err(e) => log(&format!("[semester] 学期汇总失败: {e}")),
    }
    match client.call("POST", PERSONAL_PATH, &json!({ "runMode": 1 }).to_string(), &[]) {
        Ok(biz) => {
            let d = super::client::parse_data_field(&biz);
            let pv = take_km(&d, &["curValidDis", "semesterValidDis"]);
            if pv > valid {
                valid = pv;
            }
            let raw = d
                .get("semesterGoalDis")
                .and_then(|v| v.as_f64())
                .or_else(|| d.get("semesterGoal").and_then(|v| v.as_f64()))
                .unwrap_or(0.0);
            // 目标单位兼容：>=10000 视为米，否则视为公里
            goal = if raw >= 10_000.0 {
                raw / 1000.0
            } else if raw > 0.0 {
                raw
            } else {
                0.0
            };
        }
        Err(e) => log(&format!("[semester] 个人完成度失败: {e}")),
    }
    // 完成状态接口：真实有效里程（需最近记录 rrid；「我的」页数据同源）
    if let Some(rrid) = latest_rrid {
        let body = json!({ "rrid": rrid }).to_string();
        match client.call("POST", super::user::SEMESTER_COMPLETED_PATH, &body, &[]) {
            Ok(biz) => {
                let d = super::client::parse_data_field(&biz);
                let cv = take_km(&d, &["semesterValidDis", "curValidDis"]);
                if cv > valid {
                    valid = cv;
                }
            }
            Err(e) => log(&format!("[semester] 完成状态失败: {e}")),
        }
    }
    (valid, goal)
}

#[allow(dead_code)]
pub struct SemesterResult {
    pub summary: Option<SemesterSummary>,
    /// getPersonalSemesterInfo 原始 data（结构未定型，全量落地供核对）。
    pub personal_raw: Value,
}

pub fn query(client: &mut ApiClient, log: &mut dyn FnMut(&str)) -> Result<SemesterResult, String> {
    // 汇总
    let biz = client.call("POST", SUMMARY_PATH, "{}", &[])?;
    let data = super::client::parse_data_field(&biz);
    let mut summary = SemesterSummary {
        sname: data.get("sname").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        semester_dis: data.get("semesterDis").and_then(|v| v.as_f64()).unwrap_or(0.0),
        semester_valid_dis: data.get("semesterValidDis").and_then(|v| v.as_f64()).unwrap_or(0.0),
        semester_count: data.get("semesterCount").and_then(|v| v.as_i64()).unwrap_or(0),
        semester_valid_count: data.get("semesterValidCount").and_then(|v| v.as_i64()).unwrap_or(0),
    };

    // 个人完成度：学期起止 / 考核方式 / 有效次数
    let personal_raw = match client.call("POST", PERSONAL_PATH, &json!({ "runMode": 1 }).to_string(), &[]) {
        Ok(biz) => super::client::parse_data_field(&biz),
        Err(e) => {
            log(&format!("[semester] 个人完成度接口失败（不影响汇总）: {e}"));
            Value::Null
        }
    };
    if summary.sname.is_empty() {
        summary.sname = personal_raw
            .get("sname")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
    }

    // 部分学校汇总接口计数恒为 0，有效次数退回个人完成度接口
    if summary.semester_valid_count == 0 {
        if let Some(n) = personal_raw.get("semesterValid").and_then(|v| v.as_i64()) {
            summary.semester_valid_count = n;
        }
    }

    // 目标次数仅存在于完成状态接口（需最近一条记录的 rrid）
    if summary.semester_count == 0 {
        let rrid = super::records::fetch_records(client)
            .ok()
            .and_then(|rows| rows.first().map(|r| r.rrid));
        if let Some(rrid) = rrid {
            let body = json!({ "rrid": rrid }).to_string();
            if let Ok(biz) = client.call("POST", super::user::SEMESTER_COMPLETED_PATH, &body, &[]) {
                let done = super::client::parse_data_field(&biz);
                if let Some(n) = done.get("semesterGoalCount").and_then(|v| v.as_i64()) {
                    summary.semester_count = n;
                }
                if summary.semester_valid_count == 0 {
                    if let Some(n) = done.get("semesterValidCount").and_then(|v| v.as_i64()) {
                        summary.semester_valid_count = n;
                    }
                }
            }
        }
    }

    Ok(SemesterResult { summary: Some(summary), personal_raw })
}
