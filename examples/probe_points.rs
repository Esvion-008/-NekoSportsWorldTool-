//! 只用账号拉一次实时点位，**不生成轨迹、不提交记录**。
//!
//! 用途：核对服务端这次到底下发了几个点、每个点带了哪些字段
//!（App 里显示 4 个而日志说是 5 个时，靠这里看原始 JSON）。
//!
//! ```text
//! ESVIM_USER=账号 ESVIM_PASS=密码 cargo run --release --offline --example probe_points
//! ```

use greenland::api::{client::ApiClient, login, points};
use greenland::crypto::header::HeaderIdentity;

fn main() {
    let user = std::env::var("ESVIM_USER").expect("需要环境变量 ESVIM_USER");
    let pass = std::env::var("ESVIM_PASS").expect("需要环境变量 ESVIM_PASS");

    // 直接读运行数据目录里的设备身份：本机沙箱下 data_dir() 会落到 exe 目录，
    // 读不到就会用未配置的占位锚点，导致点位/定位全错。
    let identity_path = greenland::platform::data_dir().join("identity.json");
    let identity: HeaderIdentity = std::fs::read_to_string(&identity_path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    println!("设备身份：platform={} city={}", identity.platform, identity.city);

    let mut log = |m: &str| println!("{m}");
    let mut client = ApiClient::new(identity.clone(), None);
    match login::login(&mut client, &user, &pass, &mut log) {
        Ok(session) => {
            println!("登录成功 uid={} name={}", session.uid, session.name);
            // 点位接口要带 session（token），不设就会 401「头信息不全」
            client.login = Some(session);
        }
        Err(error) => {
            eprintln!("登录失败：{error}");
            std::process::exit(1);
        }
    }

    let anchor = match client.identity.anchor_coordinate() {
        Ok(value) => value,
        Err(error) => {
            eprintln!("锚点无效：{error}");
            std::process::exit(1);
        }
    };
    let pts = match points::fetch_points(&mut client, anchor, &mut log) {
        Ok(list) => list,
        Err(error) => {
            eprintln!("点位拉取失败：{error}");
            std::process::exit(1);
        }
    };

    println!("\n===== 服务端下发 {} 个点 =====", pts.len());
    for (index, point) in pts.iter().enumerate() {
        println!("\n--- P{} ---", index + 1);
        println!("{}", serde_json::to_string_pretty(point).unwrap_or_default());
    }
    let unique: std::collections::HashSet<String> = pts
        .iter()
        .map(|p| {
            format!(
                "{:.6},{:.6}",
                p.get("lat").and_then(|v| v.as_f64()).unwrap_or(0.0),
                p.get("lon").and_then(|v| v.as_f64()).unwrap_or(0.0)
            )
        })
        .collect();
    println!("\n去重后坐标数：{}（少于 {} 说明有重复点）", unique.len(), pts.len());
}
