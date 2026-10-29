//! 批量登录所有账号，拉取各自的违规记录（cheat/cheatlist），结果存到桌面。
use greenland::api::{cheat, client::ApiClient, login, model};
use greenland::crypto::header::HeaderIdentity;
use serde_json::json;

fn main() {
    let dir = greenland::platform::data_dir();
    println!("数据目录：{}", dir.display());
    let accounts = model::load_accounts();
    println!("账号数：{}", accounts.len());

    let base: HeaderIdentity = std::fs::read_to_string(dir.join("identity.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();

    let mut rows: Vec<serde_json::Value> = Vec::new();
    for (index, account) in accounts.iter().enumerate() {
        let tag = format!("[{}/{}] {}", index + 1, accounts.len(), account.username);
        if account.password.trim().is_empty() {
            println!("{tag} 跳过：没有密码");
            rows.push(json!({ "username": account.username, "remark": account.remark, "result": "无密码" }));
            continue;
        }
        let identity = greenland::crypto::header::derive_per_account(&base, &account.username);
        let mut client = ApiClient::new(identity, None);
        let mut sink: Vec<String> = Vec::new();
        let mut log = |m: &str| sink.push(m.to_string());
        match login::login(&mut client, &account.username, &account.password, &mut log) {
            Ok(session) => {
                client.login = Some(session);
                match cheat::query(&mut client, 1, &mut log) {
                    Ok(report) => {
                        let flagged = !report.self_info.is_null();
                        println!(
                            "{tag} {} self={} 全校违规={}",
                            if flagged { "★已标记" } else { "干净" },
                            serde_json::to_string(&report.self_info).unwrap_or_default(),
                            report.list.len()
                        );
                        rows.push(json!({
                            "username": account.username,
                            "remark": account.remark,
                            "result": "ok",
                            "flagged": flagged,
                            "self": report.self_info,
                            "school_list": report.list,
                        }));
                    }
                    Err(error) => {
                        println!("{tag} 违规查询失败：{error}");
                        rows.push(json!({ "username": account.username, "remark": account.remark, "result": format!("违规查询失败：{error}") }));
                    }
                }
            }
            Err(error) => {
                println!("{tag} 登录失败：{error}");
                rows.push(json!({ "username": account.username, "remark": account.remark, "result": format!("登录失败：{error}") }));
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }

    let desktop = std::path::PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default()).join("Desktop");
    let json_path = desktop.join("违规记录.json");
    let text = serde_json::to_string_pretty(&rows).unwrap_or_default();
    match std::fs::write(&json_path, &text) {
        Ok(()) => println!("\n已写入 {}", json_path.display()),
        Err(error) => println!("\n写入失败：{error}"),
    }
    let flagged: Vec<&serde_json::Value> = rows.iter().filter(|r| r["flagged"] == true).collect();
    println!("汇总：{} 个账号，{} 个被标记", rows.len(), flagged.len());
    for row in &flagged {
        println!(
            "  ★ {}  备注={}  self={}",
            row["username"].as_str().unwrap_or(""),
            row["remark"].as_str().unwrap_or(""),
            serde_json::to_string(&row["self"]).unwrap_or_default()
        );
    }
}
