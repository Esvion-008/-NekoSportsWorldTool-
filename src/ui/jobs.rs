//! 后台任务与消息处理：IP 获取 / 登录 / 刷新 / 消息协议分发。

use super::{App, AI_DETAIL, AI_RECORDS, AI_LIST, CHEAT, IP, RANK, RECORDS, RUN_DETAIL, SEMESTER, USER};

impl App {
    /// 获取公网 IP：境内可达源优先，短超时逐个尝试，任一成功即止。
    pub(crate) fn fetch_ip(&mut self) {
        self.spawn_job(|tx| {
            // 独立短超时 agent：主 agent 30s 超时会让 IP 长时间停在「获取中」。
            // 连接超时 3s 单独收紧：DNS/握手失败时快速换下一个源。
            let agent = ureq::AgentBuilder::new()
                .timeout_connect(std::time::Duration::from_secs(3))
                .timeout(std::time::Duration::from_secs(6))
                .build();
            // 实测 ipip.net / ifconfig.me 等源对非浏览器 UA 会挂起不响应，
            // 必须带浏览器 UA（ureq 默认 UA 会被部分 CDN 直接挂到超时）。
            const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
                              AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36";
            let mut log = App::logger(tx.clone());
            let mut ip = String::new();
            // 实测（2026-10，本机直连）：3322 最快最稳；ipip/ifconfig 需 UA；
            // sohu 偶发回环假 IP（first_ipv4 会过滤）；ipify 国内常被重置，兜底。
            for url in [
                "http://members.3322.org/dyndns/getip",
                "https://myip.ipip.net",
                "https://ifconfig.me/ip",
                "https://pv.sohu.com/cityjson?ie=utf-8",
                "https://api.ipify.org?format=json",
            ] {
                let Ok(resp) = agent.get(url).set("User-Agent", UA).call() else {
                    continue;
                };
                let Ok(text) = resp.into_string() else { continue };
                if let Some(found) = first_ipv4(&text) {
                    ip = found;
                    break;
                }
            }
            if ip.is_empty() {
                log("公网 IP 获取失败");
                tx.send(format!("{IP}失败")).ok();
            } else {
                log(&format!("公网 IP：{ip}"));
                tx.send(format!("{IP}{ip}")).ok();
            }
        });
    }

    pub fn refresh_ai_list(&mut self) {
        let Some(session) = self.session.clone() else {
            self.status = "请先登录".into();
            return;
        };
        let identity = self.session_identity();
        self.ai_busy = true;
        self.status = "AI 列表拉取中…".into();
        self.spawn_job(move |tx| {
            let mut log = App::logger(tx.clone());
            let mut client = crate::api::client::ApiClient::new(identity, Some(session));
            let payload = match crate::api::flow::run_ai_list(&mut client, &mut log) {
                Ok(list) => serde_json::to_string(&list).unwrap_or_else(|_| "[]".into()),
                Err(e) => {
                    log(&format!("× AI 列表拉取失败: {e}"));
                    "[]".to_string()
                }
            };
            tx.send(format!("{AI_LIST}{payload}")).ok();
        });
    }

    pub fn refresh_records(&mut self) {
        // 数据源：records_view 指定账号优先（用其自身会话+专属设备身份），否则当前账号。
        // 指定账号已失效（未登录/被删）时自动回退当前账号。
        let (session, identity, view_name) = match self.records_view {
            Some(index) => {
                let valid = self
                    .accounts
                    .get(index)
                    .map(|a| a.logged_in())
                    .unwrap_or(false);
                if valid {
                    let a = &self.accounts[index];
                    (
                        a.session.clone(),
                        super::accounts::identity_for(&self.identity, a),
                        a.username.clone(),
                    )
                } else {
                    self.records_view = None;
                    (self.session.clone(), self.session_identity(), String::new())
                }
            }
            None => (self.session.clone(), self.session_identity(), String::new()),
        };
        let Some(session) = session else {
            self.status = "请先登录".into();
            return;
        };
        self.records_busy = true;
        self.status = if view_name.is_empty() {
            "记录拉取中…".into()
        } else {
            format!("拉取 {view_name} 的记录…")
        };
        self.spawn_job(move |tx| {
            let mut log = App::logger(tx.clone());
            let mut client = crate::api::client::ApiClient::new(identity, Some(session));
            let payload = match crate::api::flow::run_records(&mut client, &mut log) {
                Ok(rows) => serde_json::to_string(&rows).unwrap_or_else(|_| "[]".into()),
                Err(e) => {
                    log(&format!("× 记录拉取失败: {e}"));
                    "[]".to_string()
                }
            };
            tx.send(format!("{RECORDS}{payload}")).ok();
        });
    }

    /// 我的页数据聚合。
    pub fn refresh_user_page(&mut self) {
        let Some(session) = self.session.clone() else {
            self.status = "请先登录".into();
            return;
        };
        let identity = self.session_identity();
        self.user_busy = true;
        self.spawn_job(move |tx| {
            let mut log = App::logger(tx.clone());
            let mut client = crate::api::client::ApiClient::new(identity, Some(session));
            let info = crate::api::user::fetch_my_info(&mut client, &mut log);
            let payload = serde_json::json!({
                "profile": info.profile,
                "home_page": info.home_page,
                "personal_semester": info.personal_semester,
                "summary": info.summary,
                "completed": info.completed,
            });
            log("√ [user] 我的页数据已更新");
            tx.send(format!("{USER}{payload}")).ok();
            tx.send(format!("{USER}done")).ok();
        });
    }

    /// 拉取单条跑步详情。
    pub fn fetch_run_detail(&mut self, rrid: i64) {
        let Some(session) = self.session.clone() else { return };
        let identity = self.session_identity();
        self.records_page.detail_loading = true;
        self.spawn_job(move |tx| {
            let mut client = crate::api::client::ApiClient::new(identity, Some(session));
            match crate::api::records::fetch_detail(&mut client, rrid) {
                Ok(d) => {
                    let payload = serde_json::to_string(&d).unwrap_or_default();
                    tx.send(format!("{RUN_DETAIL}{payload}")).ok();
                }
                Err(e) => {
                    let payload = serde_json::json!({ "statusInfo": format!("详情拉取失败：{e}") });
                    tx.send(format!("{RUN_DETAIL}{payload}")).ok();
                }
            }
        });
    }

    /// 拉取单条 AI 记录全量详情。
    pub fn fetch_ai_detail(&mut self, id: i64) {
        let Some(session) = self.session.clone() else { return };
        let identity = self.session_identity();
        self.records_page.ai_detail_loading = true;
        self.spawn_job(move |tx| {
            let mut client = crate::api::client::ApiClient::new(identity, Some(session));
            let payload = match crate::api::ai::fetch_record_detail(&mut client, id) {
                Ok(d) => d.to_string(),
                Err(e) => serde_json::json!({ "fetchError": e }).to_string(),
            };
            tx.send(format!("{AI_DETAIL}{payload}")).ok();
        });
    }

    /// AI 记录：遍历全部项目合并（按天排序）。
    pub fn refresh_ai_records(&mut self) {
        let Some(session) = self.session.clone() else {
            self.status = "请先登录".into();
            return;
        };
        let identity = self.session_identity();
        self.records_busy = true;
        self.spawn_job(move |tx| {
            let mut log = App::logger(tx.clone());
            let mut client = crate::api::client::ApiClient::new(identity, Some(session));
            // 先拉项目列表
            let list = match crate::api::ai::fetch_list(&mut client) {
                Ok(l) => l,
                Err(e) => {
                    log(&format!("× [ai] 项目列表拉取失败: {e}"));
                    tx.send(format!("{AI_RECORDS}{{\"groups\":[],\"total\":0}}")).ok();
                    return;
                }
            };
            let mut all_groups: Vec<crate::api::ai::AiRecordGroup> = Vec::new();
            let mut total = 0i64;
            for sport in &list {
                match crate::api::ai::fetch_records(&mut client, sport.id, 50) {
                    Ok(page) => {
                        for mut g in page.groups {
                            for r in &mut g.records {
                                r.name = format!("{} {}", sport.name, r.name);
                            }
                            all_groups.push(g);
                        }
                        total += page.total_count;
                    }
                    Err(e) => log(&format!("× [ai] {} 拉取失败: {e}", sport.name)),
                }
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
            all_groups.sort_by(|a, b| b.score_date.cmp(&a.score_date));
            let count: usize = all_groups.iter().map(|g| g.records.len()).sum();
            log(&format!("√ [ai] {} 个项目共 {} 条记录", list.len(), count));
            let payload = serde_json::json!({ "groups": all_groups, "total": total });
            tx.send(format!("{AI_RECORDS}{payload}")).ok();
            tx.send("__AI_RECORDS_DONE__".to_string()).ok();
        });
    }

    /// 学期 + 违规自查（登录 / 跑步提交成功后自动调用）。
    pub fn refresh_data_page(&mut self) {
        let Some(session) = self.session.clone() else { return };
        let identity = self.session_identity();
        self.data_busy = true;
        self.spawn_job(move |tx| {
            let mut log = App::logger(tx.clone());
            let mut client = crate::api::client::ApiClient::new(identity, Some(session));
            match crate::api::semester::query(&mut client, &mut log) {
                Ok(r) => {
                    if let Some(s) = r.summary {
                        log(&format!(
                            "√ [semester] {} 有效 {}/{} 次，有效里程 {:.2} km",
                            s.sname, s.semester_valid_count, s.semester_count,
                            s.semester_valid_dis / 1000.0
                        ));
                        let payload = serde_json::to_string(&s).unwrap_or_default();
                        tx.send(format!("{SEMESTER}{payload}")).ok();
                    } else {
                        tx.send(format!("{SEMESTER}{{}}")).ok();
                    }
                }
                Err(e) => {
                    log(&format!("× [semester] 拉取失败: {e}"));
                    tx.send(format!("{SEMESTER}{{}}")).ok();
                }
            }
            match crate::api::cheat::query(&mut client, 1, &mut log) {
                Ok(r) => {
                    let payload = serde_json::json!({ "self": r.self_info, "list": r.list });
                    tx.send(format!("{CHEAT}{}", payload)).ok();
                }
                Err(e) => {
                    log(&format!("× [cheat] 检查失败: {e}"));
                    tx.send(format!("{CHEAT}{{\"self\":null,\"list\":[]}}")).ok();
                }
            }
        });
    }

    pub fn refresh_cheat_only(&mut self) {
        let Some(session) = self.session.clone() else {
            self.status = "请先登录".into();
            return;
        };
        let identity = self.session_identity();
        self.data_busy = true;
        self.spawn_job(move |tx| {
            let mut log = App::logger(tx.clone());
            let mut client = crate::api::client::ApiClient::new(identity, Some(session));
            match crate::api::cheat::query(&mut client, 1, &mut log) {
                Ok(r) => {
                    let payload = serde_json::json!({ "self": r.self_info, "list": r.list });
                    tx.send(format!("{CHEAT}{}", payload)).ok();
                }
                Err(e) => {
                    log(&format!("× [cheat] 检查失败: {e}"));
                    tx.send(format!("{CHEAT}{{\"self\":null,\"list\":[]}}")).ok();
                }
            }
        });
    }

    /// 榜单查询：kind=main|indoor|history。
    pub fn refresh_rank(&mut self, kind: &str, subtype: i64) {
        let Some(session) = self.session.clone() else {
            self.status = "请先登录".into();
            return;
        };
        let identity = self.session_identity();
        let kind = kind.to_string();
        self.data_busy = true;
        self.spawn_job(move |tx| {
            let mut log = App::logger(tx.clone());
            let mut client = crate::api::client::ApiClient::new(identity, Some(session));
            let res: Result<Vec<crate::api::rank::RankRow>, String> = (|| {
                match kind.as_str() {
                    "main" => {
                        // 当天无数据时自动回退最近 3 天（当日累计榜凌晨常为空）
                        let (rtype, sort_type) = (subtype / 10, subtype % 10);
                        let mut rows =
                            crate::api::rank::main_rank(&mut client, rtype, sort_type, None, None)?;
                        let mut back = 0;
                        while rows.is_empty() && back < 3 {
                            back += 1;
                            let d = (chrono::Local::now() - chrono::Duration::days(back))
                                .format("%Y-%m-%d")
                                .to_string();
                            log(&format!("[rank] 当天暂无数据，回退查询 {d}"));
                            rows = crate::api::rank::main_rank(
                                &mut client,
                                rtype,
                                sort_type,
                                None,
                                Some(d),
                            )?;
                        }
                        Ok(rows)
                    }
                    "indoor" => crate::api::rank::indoor_rank(&mut client, subtype, -1),
                    _ => crate::api::rank::history_rank(&mut client, subtype, -1),
                }
            })();
            let rows = match res {
                Ok(r) => r,
                Err(e) => {
                    log(&format!("× [rank] 查询失败: {e}"));
                    Vec::new()
                }
            };
            log(&format!("[rank] {} 行", rows.len()));
            tx.send(format!(
                "{RANK}{}",
                serde_json::to_string(&rows).unwrap_or_else(|_| "[]".into())
            )).ok();
        });
    }
}

/// 从响应文本中扫描第一个合法 IPv4 地址。
///
/// 各查询源的返回格式不一（纯文本 / `当前 IP：x.x.x.x 来自于：…` /
/// `var returnCitySN = {"cip":"x.x.x.x",…}` / `{"ip":"x.x.x.x"}`），
/// 直接按「数字与点」切词并做 a.b.c.d 校验，比逐源解析 JSON 更稳。
fn first_ipv4(text: &str) -> Option<String> {
    let mut token = String::new();
    for ch in text.chars().chain(std::iter::once(' ')) {
        if ch.is_ascii_digit() || ch == '.' {
            token.push(ch);
        } else if !token.is_empty() {
            if let Some(ip) = check_ipv4(&token) {
                return Some(ip);
            }
            token.clear();
        }
    }
    None
}

fn check_ipv4(token: &str) -> Option<String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut octets = [0u8; 4];
    for (i, p) in parts.iter().enumerate() {
        if p.is_empty()
            || p.len() > 3
            || !p.bytes().all(|b| b.is_ascii_digit())
            || p.parse::<u8>().is_err()
        {
            return None;
        }
        octets[i] = p.parse::<u8>().ok()?;
    }
    // 过滤非公网地址：回环 / 私有 / 链路本地 / 广播。
    // 实测 sohu cityjson 偶发返回 {"cip":"127.0.0.1"} 的假回环，不能当公网 IP 用。
    let [a, b, _, _] = octets;
    let is_public = !(a == 127
        || a == 10
        || a == 0
        || a == 255
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 169 && b == 254)
        || (a == 100 && (64..=127).contains(&b)));
    if is_public {
        Some(token.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_ip_from_plain_text_and_wrapped_payloads() {
        assert_eq!(first_ipv4("203.0.113.7"), Some("203.0.113.7".into()));
        assert_eq!(
            first_ipv4("当前 IP：203.0.113.7 来自于：中国 广东 广州  移动"),
            Some("203.0.113.7".into())
        );
        assert_eq!(
            first_ipv4(r#"var returnCitySN = {"cip": "203.0.113.7", "cid": "440100"};"#),
            Some("203.0.113.7".into())
        );
        assert_eq!(first_ipv4(r#"{"ip":"198.51.100.23"}"#), Some("198.51.100.23".into()));
    }

    #[test]
    fn rejects_malformed_or_out_of_range_candidates() {
        assert_eq!(first_ipv4("v1.2.3"), None);
        assert_eq!(first_ipv4("999.1.2.3"), None);
        assert_eq!(first_ipv4("2026-10-11"), None);
        assert_eq!(first_ipv4("没有 IP"), None);
    }

    /// 回环/私有段不能当公网 IP（sohu 偶发返回 cip=127.0.0.1 的假响应）。
    #[test]
    fn rejects_loopback_and_private_ranges() {
        assert_eq!(first_ipv4("127.0.0.1"), None);
        assert_eq!(first_ipv4("10.1.2.3"), None);
        assert_eq!(first_ipv4("192.168.1.1"), None);
        assert_eq!(first_ipv4("172.16.0.1"), None);
        assert_eq!(first_ipv4("169.254.1.1"), None);
        // 假响应里没有公网地址 → 整体判无效，换下一个源
        assert_eq!(
            first_ipv4(r#"var returnCitySN = {"cip": "127.0.0.1", "cid": "00", "cname": "未知"};"#),
            None
        );
        // 混合场景：先出现私有地址，继续扫到后面的公网地址
        assert_eq!(
            first_ipv4("proxy 192.168.0.1 real 203.0.113.7"),
            Some("203.0.113.7".into())
        );
    }
}
