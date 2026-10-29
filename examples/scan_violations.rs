//! 遍历所有账号：拉跑步记录 → 逐条查违规详情 → 汇总每个账号哪条记录因为什么被判违规。
use greenland::api::{client::ApiClient, login, model, records};
use greenland::crypto::header::HeaderIdentity;
use serde_json::{json, Value};

fn reason_text(detail: &Value) -> (bool, String, Vec<String>) {
    let complete = detail.get("complete").and_then(|v| v.as_bool()).unwrap_or(true);
    let un_complete = detail.get("unCompleteReason").and_then(|v| v.as_i64()).unwrap_or(0);
    let is_cheat = detail.get("isCheat").and_then(|v| v.as_i64()).unwrap_or(0);
    let mut bad: Vec<String> = Vec::new();
    if let Some(list) = detail.get("reasonList").and_then(|v| v.as_array()) {
        for item in list {
            let ok = item.get("complete").and_then(|v| v.as_bool()).unwrap_or(true);
            if !ok {
                let reason = item.get("reason").and_then(|v| v.as_str()).unwrap_or("");
                if !reason.is_empty() {
                    bad.push(reason.to_string());
                }
            }
        }
    }
    let flagged = !complete || un_complete != 0 || is_cheat == 1 || !bad.is_empty();
    let status = detail.get("statusInfo").and_then(|v| v.as_str()).unwrap_or("").to_string();
    (flagged, status, bad)
}

fn main() {
    let dir = greenland::platform::data_dir();
    let accounts = model::load_accounts();
    println!("账号数：{}", accounts.len());
    let base: HeaderIdentity = std::fs::read_to_string(dir.join("identity.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();

    let mut out: Vec<Value> = Vec::new();
    for (index, account) in accounts.iter().enumerate() {
        let tag = format!("[{}/{}] {}", index + 1, accounts.len(), account.username);
        if account.password.trim().is_empty() {
            println!("{tag} 无密码，跳过");
            continue;
        }
        let identity = greenland::crypto::header::derive_per_account(&base, &account.username);
        let mut client = ApiClient::new(identity, None);
        let mut sink: Vec<String> = Vec::new();
        let mut log = |m: &str| sink.push(m.to_string());
        let session = match login::login(&mut client, &account.username, &account.password, &mut log) {
            Ok(s) => s,
            Err(e) => {
                println!("{tag} 登录失败：{e}");
                out.push(json!({"username": account.username, "remark": account.remark, "error": e}));
                continue;
            }
        };
        client.login = Some(session);
        let rows = match records::fetch_records(&mut client) {
            Ok(r) => r,
            Err(e) => {
                println!("{tag} 记录列表失败：{e}");
                out.push(json!({"username": account.username, "remark": account.remark, "error": e}));
                continue;
            }
        };
        let mut items: Vec<Value> = Vec::new();
        for row in &rows {
            let mut bad_reason: Vec<String> = Vec::new();
            let mut status_info = String::new();
            let mut is_cheat = 0i64;
            let mut un_complete = 0i64;
            if !row.complete {
                if let Ok(detail) = records::fetch_one_record(&mut client, row.rrid) {
                    let (flagged, status, bad) = reason_text(&detail);
                    status_info = status;
                    bad_reason = bad;
                    is_cheat = detail.get("isCheat").and_then(|v| v.as_i64()).unwrap_or(0);
                    un_complete = detail.get("unCompleteReason").and_then(|v| v.as_i64()).unwrap_or(0);
                    if !flagged {
                        bad_reason.push("未完成（服务端未给出具体原因）".into());
                    }
                } else {
                    bad_reason.push("详情拉取失败".into());
                }
            }
            items.push(json!({
                "rrid": row.rrid,
                "start_ms": row.start_time,
                "distance_m": row.total_dis,
                "duration_s": row.total_time,
                "complete": row.complete,
                "isCheat": is_cheat,
                "unCompleteReason": un_complete,
                "statusInfo": status_info,
                "violations": bad_reason,
            }));
            std::thread::sleep(std::time::Duration::from_millis(180));
        }
        let bad_count = items.iter().filter(|i| i["complete"] == false).count();
        println!(
            "{tag} 记录 {} 条，违规/未完成 {} 条",
            items.len(),
            bad_count
        );
        for item in items.iter().filter(|i| i["complete"] == false) {
            println!(
                "    ✗ rrid={} {} {}m {}min 原因={}",
                item["rrid"],
                item["start_ms"],
                item["distance_m"].as_f64().unwrap_or(0.0) as i64,
                item["duration_s"].as_i64().unwrap_or(0) / 60,
                item["violations"]
            );
        }
        out.push(json!({
            "username": account.username,
            "remark": account.remark,
            "record_count": items.len(),
            "violation_count": bad_count,
            "records": items,
        }));
        std::thread::sleep(std::time::Duration::from_millis(1200));
    }

    let text = serde_json::to_string_pretty(&out).unwrap_or_default();
    match std::fs::write("violations.json", &text) {
        Ok(()) => println!("\n已写入 violations.json"),
        Err(e) => println!("\n写入失败：{e}"),
    }
    let total_viol: usize = out.iter().map(|a| a["violation_count"].as_u64().unwrap_or(0) as usize).sum();
    let hitting = out.iter().filter(|a| a["violation_count"].as_u64().unwrap_or(0) > 0).count();
    println!("汇总：{} 个账号，{} 个有违规记录，共 {} 条违规", out.len(), hitting, total_viol);
}
