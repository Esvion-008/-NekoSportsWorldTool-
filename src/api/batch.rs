//! 批量账号：文件解析 + 有界并发执行 + 单账号登录 / 刷跑封装。
//!
//! 单个账号走的仍是 `api::login` 与 `api::flow::run_full_flow` 这两条原链，
//! 本模块只负责「解析文件」「控制并发」「把结果归拢」，不复制任何协议逻辑。

use crate::api::client::ApiClient;
use crate::api::model::{Session, MAX_ACCOUNTS};
use crate::crypto::header::HeaderIdentity;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// 账号文件里一行解析出的账号 / 密码。
pub type Credential = (String, String);

/// 一条失败记录（弹窗展示与文件导出共用）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FailItem {
    pub username: String,
    pub password: String,
    pub message: String,
}

/// 账号文件解析结果。
#[derive(Debug, Default)]
pub struct ParsedAccounts {
    pub entries: Vec<Credential>,
    /// 解析失败的行（原样保留）。
    pub skipped: Vec<String>,
    /// 因重复被忽略的行数。
    pub duplicated: usize,
    /// 因超出 200 上限被丢弃的行数。
    pub dropped: usize,
}

impl ParsedAccounts {
    /// 可直接回填的错误摘要（供弹窗使用）。
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.duplicated > 0 {
            out.push(format!("重复账号 {} 行（已忽略，同一账号只保留第一条）", self.duplicated));
        }
        if self.dropped > 0 {
            out.push(format!("超出上限被丢弃 {} 行（最多 {MAX_ACCOUNTS} 个账号）", self.dropped));
        }
        for line in self.skipped.iter().take(10) {
            out.push(format!("格式不正确：{line}"));
        }
        if self.skipped.len() > 10 {
            out.push(format!("……另有 {} 行格式不正确", self.skipped.len() - 10));
        }
        out
    }
}

/// 解析账号文件内容。
///
/// 每行 `账号----密码`（推荐）；也兼容 `---` / `--`、`:`、`：`、`,`、`，`、
/// 制表符、空格分隔。空行与 `#`、`//` 开头的注释行会被忽略。
pub fn parse_accounts(text: &str) -> ParsedAccounts {
    let mut out = ParsedAccounts::default();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for raw in text.lines() {
        let line = raw.trim_start_matches('\u{feff}').trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
            continue;
        }
        match split_credential(line) {
            Some((user, pass)) if !user.is_empty() && !pass.is_empty() => {
                if !seen.insert(user.clone()) {
                    out.duplicated += 1;
                    continue;
                }
                if out.entries.len() >= MAX_ACCOUNTS {
                    out.dropped += 1;
                    continue;
                }
                out.entries.push((user, pass));
            }
            _ => out.skipped.push(line.to_string()),
        }
    }
    out
}

fn split_credential(line: &str) -> Option<(String, String)> {
    if let Some(pair) = split_on_dashes(line) {
        if !pair.0.is_empty() && !pair.1.is_empty() {
            return Some(pair);
        }
        return None;
    }
    for sep in [':', '：', ',', '，', '\t', ' '] {
        if let Some(i) = line.find(sep) {
            let user = line[..i].trim();
            let pass = line[i + sep.len_utf8()..].trim();
            if !user.is_empty() && !pass.is_empty() {
                return Some((user.to_string(), pass.to_string()));
            }
        }
    }
    None
}

/// 在第一个长度 >= 2 的连续 `-` 处切分（切完两侧都要有内容）。
fn split_on_dashes(line: &str) -> Option<(String, String)> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'-' && bytes[i + 1] == b'-' {
            let mut end = i;
            while end < bytes.len() && bytes[end] == b'-' {
                end += 1;
            }
            return Some((line[..i].trim().to_string(), line[end..].trim().to_string()));
        }
        i += 1;
    }
    None
}

/// 失败账号导出文本：先给一段可直接重新导入的 `账号----密码`，
/// 再用注释行附上失败原因（注释行导入时会被忽略）。
pub fn fail_export_text(items: &[FailItem]) -> String {
    let mut out = String::from(
        "# 登录/刷跑失败账号（失败原因见文件末尾注释；上半部分可直接重新导入）\n",
    );
    for it in items {
        out.push_str(&format!("{}----{}\n", it.username, it.password));
    }
    out.push_str("\n# ===== 失败原因 =====\n");
    for it in items {
        out.push_str(&format!("# {} ({})：{}\n", it.username, it.password, it.message));
    }
    out
}

/// 有界并发执行：`items` 由至多 `concurrency` 个线程消费，结果按输入顺序返回。
///
/// 用 `worker.clone()` 给每个线程一份闭包，避免要求闭包 `Sync`。
pub fn parallel_map<T, R, F>(items: Vec<T>, concurrency: usize, worker: F) -> Vec<R>
where
    T: Send + 'static,
    R: Send + 'static,
    F: Fn(T, usize) -> R + Send + Clone + 'static,
{
    let total = items.len();
    if total == 0 {
        return Vec::new();
    }
    let queue: Arc<Mutex<VecDeque<(usize, T)>>> =
        Arc::new(Mutex::new(items.into_iter().enumerate().collect()));
    let results: Arc<Mutex<Vec<(usize, R)>>> = Arc::new(Mutex::new(Vec::with_capacity(total)));
    let workers = concurrency.clamp(1, total);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let results = Arc::clone(&results);
            let worker = worker.clone();
            scope.spawn(move || loop {
                let next = queue.lock().map(|mut q| q.pop_front()).unwrap_or(None);
                let Some((index, item)) = next else { break };
                let out = worker(item, index);
                if let Ok(mut slot) = results.lock() {
                    slot.push((index, out));
                }
            });
        }
    });
    let mut collected = results
        .lock()
        .map(|mut slot| std::mem::take(&mut *slot))
        .unwrap_or_default();
    collected.sort_by_key(|(index, _)| *index);
    collected.into_iter().map(|(_, out)| out).collect()
}

/// 单账号登录（与界面上单个登录同一条链路）；成功后写盘交给调用方。
/// proxy 为空 = 直连；代理配置非法时返回 Err（不会静默直连）。
pub fn login_account(
    identity: &HeaderIdentity,
    username: &str,
    password: &str,
    proxy: Option<String>,
    log: &mut dyn FnMut(&str),
) -> Result<Session, String> {
    let mut client = ApiClient::with_proxy(identity.clone(), None, proxy)?;
    crate::api::login::login(&mut client, username, password, log)
}

/// 单账号刷跑（与「开始跑步」同一条链路）。
pub fn run_account(
    identity: &HeaderIdentity,
    session: Session,
    params: &crate::api::flow::RunParams,
    proxy: Option<String>,
    log: &mut dyn FnMut(&str),
) -> Result<crate::api::flow::RunOutcome, String> {
    let mut client = ApiClient::with_proxy(identity.clone(), Some(session), proxy)?;
    crate::api::flow::run_full_flow(&mut client, params, log)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dash_separated_file() {
        let text = "2023001----abc123\n2023002----def456\n";
        let p = parse_accounts(text);
        assert_eq!(p.entries.len(), 2);
        assert_eq!(p.entries[0], ("2023001".into(), "abc123".into()));
        assert!(p.skipped.is_empty());
    }

    #[test]
    fn accepts_other_separators_and_skips_comments_and_blank_lines() {
        let text = "\u{feff}# 注释\n\n13800000000:pass:word\n13900000000，pw\na\tb\nc d\n";
        let p = parse_accounts(text);
        assert_eq!(
            p.entries,
            vec![
                ("13800000000".to_string(), "pass:word".to_string()),
                ("13900000000".to_string(), "pw".to_string()),
                ("a".to_string(), "b".to_string()),
                ("c".to_string(), "d".to_string()),
            ]
        );
        assert!(p.skipped.is_empty());
    }

    #[test]
    fn reports_bad_lines_and_duplicates() {
        let text = "a----1\nnonsense\nb----2\n a \n a----9\n";
        let p = parse_accounts(text);
        assert_eq!(
            p.entries,
            vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "2".to_string()),
            ]
        );
        assert_eq!(p.duplicated, 1, "重复账号应被计入 duplicated");
        assert_eq!(p.skipped, vec!["nonsense".to_string(), "a".to_string()]);
    }

    #[test]
    fn keeps_dashes_inside_password() {
        let p = parse_accounts("user----pa--ss--word\n");
        assert_eq!(p.entries[0], ("user".to_string(), "pa--ss--word".to_string()));
    }

    #[test]
    fn truncates_over_the_account_limit() {
        let text: String = (0..250).map(|i| format!("u{i}----p{i}\n")).collect();
        let p = parse_accounts(&text);
        assert_eq!(p.entries.len(), MAX_ACCOUNTS);
        assert_eq!(p.dropped, 50);
    }

    #[test]
    fn parallel_map_keeps_input_order_and_covers_every_item() {
        let items: Vec<usize> = (0..37).collect();
        let out = parallel_map(items, 4, |item, _| item * 2);
        assert_eq!(out, (0..37).map(|i| i * 2).collect::<Vec<_>>());
    }

    #[test]
    fn parallel_map_handles_empty_input() {
        let out: Vec<usize> = parallel_map(Vec::new(), 3, |item: usize, _| item);
        assert!(out.is_empty());
    }

    #[test]
    fn fail_export_is_reimportable() {
        let items = vec![FailItem {
            username: "u1".into(),
            password: "p1".into(),
            message: "密码错误".into(),
        }];
        let text = fail_export_text(&items);
        let p = parse_accounts(&text);
        assert_eq!(p.entries, vec![("u1".to_string(), "p1".to_string())]);
    }
}
