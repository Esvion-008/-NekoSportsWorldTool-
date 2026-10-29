//! 消息协议分发与弹窗构建。

use super::{App, ACCT_LOGIN, ACCT_LOGIN_DONE, ACCT_PROG, ACCT_RUN, ACCT_RUN_DONE, ACCT_CHEAT, ACCT_CHEAT_DONE, ACCT_TODAY, ACCT_TODAY_DONE, AI_DETAIL, AI_DONE, AI_LIST, AI_RECORDS, CHEAT, IP, PopupInfo, RANK, RECORDS, RUN_DETAIL, RUN_DONE, SEMESTER, TWO_DAY_DONE, TWO_DAY_PROG, USER};
use super::accounts::BatchProgress;
use chrono::TimeZone;

impl App {
    pub(crate) fn poll_messages(&mut self) {
        let mut done_ip = None;
        let mut done_run = None;
        let mut done_ai = None;
        let mut records_json = None;
        let mut ai_list_json = None;
        let mut semester_json = None;
        let mut cheat_json = None;
        let mut rank_json = None;
        let mut user_json = None;
        let mut ai_records_json = None;
        let mut ai_detail_raw = None;
        let mut detail_raw = None;
        let mut acct_login: Vec<serde_json::Value> = Vec::new();
        let mut acct_run: Vec<serde_json::Value> = Vec::new();
        let mut acct_prog: Option<serde_json::Value> = None;
        let mut acct_login_done: Option<serde_json::Value> = None;
        let mut acct_run_done: Option<serde_json::Value> = None;
        let mut two_day_prog: Option<serde_json::Value> = None;
        let mut two_day_done: Option<serde_json::Value> = None;
        let mut acct_today: Vec<serde_json::Value> = Vec::new();
        let mut acct_today_done = false;
        let mut acct_cheat: Vec<serde_json::Value> = Vec::new();
        let mut acct_cheat_done = false;
        while let Ok(msg) = self.rx.try_recv() {
            if let Some(v) = msg.strip_prefix(IP) {
                done_ip = Some(v.to_string());
            } else if msg.strip_prefix(ACCT_TODAY_DONE).is_some() {
                acct_today_done = true;
            } else if msg.strip_prefix(ACCT_CHEAT_DONE).is_some() {
                acct_cheat_done = true;
            } else if let Some(v) = msg.strip_prefix(ACCT_CHEAT) {
                if let Ok(val) = serde_json::from_str(v) {
                    acct_cheat.push(val);
                }
            } else if let Some(v) = msg.strip_prefix(ACCT_TODAY) {
                if let Ok(val) = serde_json::from_str(v) {
                    acct_today.push(val);
                }
            } else if let Some(v) = msg.strip_prefix(ACCT_LOGIN_DONE) {
                acct_login_done = serde_json::from_str(v).ok();
            } else if let Some(v) = msg.strip_prefix(TWO_DAY_PROG) {
                two_day_prog = serde_json::from_str(v).ok();
            } else if let Some(v) = msg.strip_prefix(TWO_DAY_DONE) {
                two_day_done = serde_json::from_str(v).ok();
            } else if let Some(v) = msg.strip_prefix(ACCT_RUN_DONE) {
                acct_run_done = serde_json::from_str(v).ok();
            } else if let Some(v) = msg.strip_prefix(ACCT_PROG) {
                acct_prog = serde_json::from_str(v).ok();
            } else if let Some(v) = msg.strip_prefix(ACCT_LOGIN) {
                if let Ok(val) = serde_json::from_str(v) {
                    acct_login.push(val);
                }
            } else if let Some(v) = msg.strip_prefix(ACCT_RUN) {
                if let Ok(val) = serde_json::from_str(v) {
                    acct_run.push(val);
                }
            } else if let Some(v) = msg.strip_prefix(RUN_DONE) {
                done_run = Some(v.to_string());
            } else if let Some(v) = msg.strip_prefix(AI_DONE) {
                done_ai = Some(v.to_string());
            } else if let Some(v) = msg.strip_prefix(RECORDS) {
                records_json = Some(v.to_string());
            } else if let Some(v) = msg.strip_prefix(AI_LIST) {
                ai_list_json = Some(v.to_string());
            } else if let Some(v) = msg.strip_prefix(SEMESTER) {
                semester_json = Some(v.to_string());
            } else if let Some(v) = msg.strip_prefix(CHEAT) {
                cheat_json = Some(v.to_string());
            } else if let Some(v) = msg.strip_prefix(RANK) {
                rank_json = Some(v.to_string());
            } else if let Some(v) = msg.strip_prefix(USER) {
                if v != "done" {
                    user_json = Some(v.to_string());
                } else {
                    self.user_busy = false;
                }
            } else if let Some(v) = msg.strip_prefix(AI_RECORDS) {
                ai_records_json = Some(v.to_string());
            } else if let Some(v) = msg.strip_prefix(RUN_DETAIL) {
                if let Ok(raw) = serde_json::from_str::<serde_json::Value>(v) {
                    detail_raw = Some(raw);
                }
            } else if let Some(v) = msg.strip_prefix(AI_DETAIL) {
                if let Ok(raw) = serde_json::from_str::<serde_json::Value>(v) {
                    ai_detail_raw = Some(raw);
                }
            } else {
                self.log.push(&msg);
            }
        }
        let got_semester = semester_json.is_some();
        let got_cheat = cheat_json.is_some();
        let got_rank = rank_json.is_some();
        if let Some(ip) = done_ip {
            self.ip = ip;
        }
        for val in acct_login {
            self.apply_account_login(val);
        }
        for val in acct_today {
            self.apply_today_status(val);
        }
        for val in acct_cheat {
            self.apply_cheat_result(val);
        }
        if acct_cheat_done {
            self.finish_cheat_check();
        }
        if acct_today_done {
            self.finish_today_check();
        }
        for val in acct_run {
            self.apply_account_run(val);
        }
        if let Some(v) = acct_prog {
            let phase = v["phase"].as_str().unwrap_or("");
            self.batch_progress = Some(BatchProgress {
                label: match phase {
                    "run" => "批量刷跑".into(),
                    "today" => "达标检查".into(),
                    _ => "批量登录".into(),
                },
                done: v["done"].as_u64().unwrap_or(0) as usize,
                total: v["total"].as_u64().unwrap_or(0) as usize,
            });
        }
        if let Some(v) = acct_login_done {
            self.finish_batch_login(&v);
        }
        if let Some(v) = acct_run_done {
            self.finish_batch_run(&v);
        }
        if let Some(v) = two_day_prog {
            self.apply_two_day_progress(&v);
        }
        if let Some(v) = two_day_done {
            self.finish_two_day(&v);
        }
        if let Some(v) = done_run {
            self.run_busy = false;
            let val: serde_json::Value = serde_json::from_str(&v).unwrap_or_default();
            if val.get("ok").and_then(|b| b.as_bool()).unwrap_or(false) {
                self.status = format!(
                    "跑步提交成功 rrid={}（OBS {}/2）",
                    val["rrid"].as_i64().unwrap_or(0),
                    val["obs_ok"].as_i64().unwrap_or(0)
                );
                self.popup = Some(run_popup(&val));
                self.refresh_data_page();
                // 单跑成功后刷新达标与公里：购剩 = 已购 - 有效公里 需要最新有效公里
                self.start_today_check();
            } else {
                self.status = format!("跑步提交失败：{}", val["message"].as_str().unwrap_or(""));
            }
        }
        if let Some(v) = done_ai {
            self.ai_busy = false;
            let val: serde_json::Value = serde_json::from_str(&v).unwrap_or_default();
            if val.get("ok").and_then(|b| b.as_bool()).unwrap_or(false) {
                self.status = "AI 运动提交成功".into();
                self.popup = Some(ai_popup(&val));
            } else {
                self.status = format!("AI 提交失败：{}", val["message"].as_str().unwrap_or(""));
            }
        }
        if let Some(v) = records_json {
            self.records_busy = false;
            match serde_json::from_str::<Vec<crate::api::records::RecordRow>>(&v) {
                Ok(rows) => {
                    self.status = format!("已刷新 {} 条记录", rows.len());
                    self.records_page.rows = rows;
                }
                Err(_) => self.status = "记录数据解析失败".into(),
            }
        }
        if let Some(v) = ai_list_json {
            self.ai_busy = false;
            match serde_json::from_str::<Vec<crate::api::ai::AiSport>>(&v) {
                Ok(list) => {
                    self.status = format!("已拉取 {} 个 AI 项目", list.len());
                    self.ai_page.list = list;
                    self.ai_page.selected = 0;
                }
                Err(_) => self.status = "AI 列表解析失败".into(),
            }
        }
        if let Some(v) = semester_json {
            match serde_json::from_str::<crate::api::semester::SemesterSummary>(&v) {
                Ok(sm) => self.data_page.semester = Some(sm),
                Err(_) => self.status = "学期数据解析失败".into(),
            }
        }
        if let Some(v) = cheat_json {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&v) {
                let self_info = val.get("self").cloned().unwrap_or(serde_json::Value::Null);
                // 把「自己是否被标记」写回账号：账号行会标红并显示原因。
                // 原来只存进 data_page.cheat（全局），所以底栏显示「已标记：某某」
                // 但账号列表里那一行还是绿的，看不到。
                let flagged = !self_info.is_null();
                // 解冻时间：优先取服务端返回的时间字段，取不到按 24 小时估
                let until = if flagged {
                    let mut found = 0i64;
                    for key in [
                        "endTime", "punishEndTime", "expireTime", "deadline",
                        "endDate", "unlockTime", "releaseTime", "finishTime",
                    ] {
                        if let Some(value) = self_info.get(key) {
                            if let Some(number) = value.as_i64() {
                                found = if number > 1_000_000_000_000 { number } else { number * 1000 };
                                break;
                            }
                            if let Some(text) = value.as_str() {
                                if let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(
                                    text.trim(),
                                    "%Y-%m-%d %H:%M:%S",
                                ) {
                                    found = parsed.and_utc().timestamp_millis();
                                    break;
                                }
                                if let Ok(parsed) = chrono::NaiveDate::parse_from_str(
                                    text.trim().get(..10).unwrap_or(""),
                                    "%Y-%m-%d",
                                ) {
                                    found = parsed
                                        .and_hms_opt(0, 0, 0)
                                        .map(|d| d.and_utc().timestamp_millis())
                                        .unwrap_or(0);
                                    break;
                                }
                            }
                        }
                    }
                    if found > crate::crypto::envelope::now_ms() {
                        found
                    } else {
                        crate::crypto::envelope::now_ms() + 24 * 3_600_000
                    }
                } else {
                    0
                };
                let reason = if flagged {
                    self_info
                        .get("reason")
                        .or_else(|| self_info.get("punishReason"))
                        .or_else(|| self_info.get("cause"))
                        .and_then(|r| r.as_str())
                        .unwrap_or("记录被判异常")
                        .to_string()
                } else {
                    String::new()
                };
                if let Some(index) = self.active_account {
                    if let Some(account) = self.accounts.get_mut(index) {
                        account.cheat_flag = Some(flagged);
                        account.cheat_reason = reason.clone();
                        if flagged {
                            account.flagged_reason = reason.clone();
                            account.flagged_until_ms = until;
                        } else {
                            account.flagged_reason.clear();
                            account.flagged_until_ms = 0;
                        }
                    }
                    let _ = self.persist_accounts();
                }
                if !self_info.is_null() {
                    let raw: String = serde_json::to_string(&self_info)
                        .unwrap_or_default()
                        .chars()
                        .take(260)
                        .collect();
                    self.status = format!("自查·自己被标记：{raw}");
                }
                self.data_page.cheat = Some(crate::api::cheat::CheatReport {
                    self_info,
                    list: val
                        .get("list")
                        .and_then(|l| l.as_array())
                        .cloned()
                        .unwrap_or_default(),
                });
            }
        }
        if let Some(v) = rank_json {
            match serde_json::from_str::<Vec<crate::api::rank::RankRow>>(&v) {
                Ok(rows) => self.data_page.rank_rows = rows,
                Err(_) => self.status = "排行榜解析失败".into(),
            }
        }
        if let Some(v) = user_json {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&v) {
                self.user_page.info = Some(crate::api::user::MyInfo {
                    profile: val.get("profile").cloned().unwrap_or(serde_json::Value::Null),
                    home_page: val.get("home_page").cloned().unwrap_or(serde_json::Value::Null),
                    personal_semester: val
                        .get("personal_semester")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                    summary: val.get("summary").cloned().unwrap_or(serde_json::Value::Null),
                    completed: val.get("completed").cloned().unwrap_or(serde_json::Value::Null),
                });
            }
        }
        if let Some(raw) = ai_detail_raw {
            self.records_page.ai_detail_raw = Some(raw);
            self.records_page.ai_detail_loading = false;
        }
        if let Some(v) = ai_records_json {
            self.records_busy = false;
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&v) {
                self.records_page.ai_groups = val
                    .get("groups")
                    .and_then(|x| serde_json::from_value(x.clone()).ok())
                    .unwrap_or_default();
                self.records_page.ai_total =
                    val.get("total").and_then(|x| x.as_i64()).unwrap_or(0);
            }
        }
        if let Some(raw) = detail_raw {
            self.records_page.detail_raw = Some(raw);
            self.records_page.detail_loading = false;
        }
        if got_semester || got_cheat || got_rank {
            self.data_busy = false;
        }
    }
}

fn fmt_hms(ms: i64) -> String {
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

fn fmt_dur(sec: i64) -> String {
    format!("{}:{:02}:{:02}", sec / 3600, sec % 3600 / 60, sec % 60)
}

fn run_popup(v: &serde_json::Value) -> PopupInfo {
    let dist = v["dist"].as_f64().unwrap_or(0.0);
    let dur = v["dur"].as_i64().unwrap_or(0);
    let pace = if dist > 0.0 { dur as f64 / (dist / 1000.0) } else { 0.0 };
    let mut lines = vec![
        format!("记录号：{}", v["rrid"].as_i64().unwrap_or(0)),
        format!("UUID：{}", v["uuid"].as_str().unwrap_or("")),
        format!(
            "距离：{:.2} km（达标线 {} m）",
            dist / 1000.0,
            v["sel_distance"].as_i64().unwrap_or(0)
        ),
        format!(
            "时长：{} · 配速 {}:{:02}/km",
            fmt_dur(dur),
            (pace / 60.0) as i64,
            (pace as i64) % 60
        ),
        format!(
            "步数：{}（步频 {} spm）",
            v["steps"].as_i64().unwrap_or(0),
            v["avg_step_freq"].as_i64().unwrap_or(0)
        ),
        format!(
            "卡路里：{} kcal · 功率 {} W",
            v["calorie"].as_i64().unwrap_or(0),
            v["avg_power"].as_i64().unwrap_or(0)
        ),
        format!("开始时间：{}", fmt_hms(v["start"].as_i64().unwrap_or(0))),
        format!(
            "OBS 上传：{}/2 · 详情验证：{}",
            v["obs_ok"].as_i64().unwrap_or(0),
            if v["verify"].as_bool().unwrap_or(false) { "通过" } else { "未通过" }
        ),
    ];
    // 达标判定明细（详情接口 reasonList）
    if let Ok(detail) = crate::api::flow::VERIFY_DETAIL.lock() {
        if let Some(d) = detail.as_ref() {
            if let Some(list) = d.get("reasonList").and_then(|x| x.as_array()) {
                for r in list {
                    let ok = r.get("complete").and_then(|x| x.as_bool()).unwrap_or(false);
                    let reason = r.get("reason").and_then(|x| x.as_str()).unwrap_or("");
                    lines.push(format!(
                        "判定：{} {}",
                        if ok { "达标" } else { "未达标" },
                        reason
                    ));
                }
            }
        }
    }
    PopupInfo {
        title: "跑步结果".into(),
        lines,
    }
}

fn ai_popup(v: &serde_json::Value) -> PopupInfo {
    if v["batch"].as_bool().unwrap_or(false) {
        return PopupInfo {
            title: "AI 批量补签结果".into(),
            lines: vec![
                format!("成功：{}/{}", v["success"].as_i64().unwrap_or(0), v["total"].as_i64().unwrap_or(0)),
                format!(
                    "覆盖：{} 天 × 每天 {} 次 × {} 个项目",
                    v["days"].as_i64().unwrap_or(0),
                    v["per_day"].as_i64().unwrap_or(0),
                    v["sports"].as_i64().unwrap_or(0)
                ),
            ],
        };
    }
    PopupInfo {
        title: "AI 运动提交结果".into(),
        lines: vec![
            format!("项目 sportId：{}", v["sport_id"].as_i64().unwrap_or(0)),
            format!("模式：{}", v["mode"].as_str().unwrap_or("")),
            format!("成绩：{}", v["score"].as_str().unwrap_or("")),
            format!(
                "用时：{:.1} 秒 · 速度 {}/分 · 消耗 {:.1}",
                v["secs"].as_f64().unwrap_or(0.0),
                v["per_min"].as_i64().unwrap_or(0),
                v["consume"].as_f64().unwrap_or(0.0),
            ),
            format!(
                "服务器：{} {}",
                v["resp_error"].as_i64().unwrap_or(0),
                v["resp_msg"].as_str().unwrap_or("")
            ),
        ],
    }
}
