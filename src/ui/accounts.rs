//! 账号页：批量导入 / 批量登录 / 批量登出 / 批量刷跑 + 单账号增删。
//!
//! 后台任务全部走 `App::spawn_job` + 带内消息协议；单个账号走的仍是
//! `api::login` 与 `api::flow::run_full_flow` 两条原链，只是被并发调度。

use super::{
    mobile, theme, App, PopupInfo, ACCT_CHEAT, ACCT_CHEAT_DONE, ACCT_LOGIN, ACCT_LOGIN_DONE,
    ACCT_PROG, ACCT_RUN, ACCT_RUN_DONE, ACCT_TODAY, ACCT_TODAY_DONE, TAB_ACCOUNTS, TAB_RECORDS,
};
use crate::api::batch::{self, FailItem};
use crate::api::model::{self, Account, HeaderIdentity, MAX_ACCOUNTS};
use chrono::TimeZone;
use eframe::egui;
use egui::RichText;
use egui_extras::{Column, TableBuilder};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// 批量任务进度（状态栏展示）。
#[derive(Debug, Clone)]
pub struct BatchProgress {
    pub label: String,
    pub done: usize,
    pub total: usize,
}

/// 账号页界面状态（不落盘）。
pub struct AccountsPage {
    pub new_user: String,
    pub new_pass: String,
    pub paste_buf: String,
    pub show_paste: bool,
    pub concurrency: usize,
    /// 批量登录时是否把已登录账号也重新登录一遍。
    pub relogin: bool,
    pub show_password: bool,
    /// 账号列表搜索关键字（按账号名/登录昵称包含匹配，忽略大小写；不落盘）。
    pub search: String,
    /// 备注筛选：None=全部；Some("")=无备注；Some(x)=备注为 x 的账号（不落盘）。
    pub remark_filter: Option<String>,
    /// 只看已购账号开关（在备注筛选行「无备注」后面）。
    pub only_bought: bool,
    /// 批量设置备注的输入缓冲。
    pub batch_remark: String,
    /// 批量增加已购公里的输入缓冲。
    pub batch_bought: String,
    /// 批量设置账号代理的输入缓冲。
    pub batch_proxy: String,
    /// 最近一次批量的失败名单（登录 + 刷跑各自覆盖）。
    pub last_fails: Vec<FailItem>,
    pub last_export_path: String,
}

impl AccountsPage {
    pub fn new(concurrency: usize) -> Self {
        Self {
            new_user: String::new(),
            new_pass: String::new(),
            paste_buf: String::new(),
            show_paste: false,
            concurrency: concurrency.clamp(1, 32),
            relogin: false,
            show_password: false,
            search: String::new(),
            remark_filter: None,
            only_bought: false,
            batch_remark: String::new(),
            batch_bought: String::new(),
            batch_proxy: String::new(),
            last_fails: Vec::new(),
            last_export_path: String::new(),
        }
    }
}

impl Default for AccountsPage {
    fn default() -> Self {
        Self::new(5)
    }
}

/// 行内操作。
enum RowAction {
    Login(usize),
    Logout(usize),
    Activate(usize),
    Delete(usize),
    /// 勾选/取消勾选单个账号（index, 新值）。
    Select(usize, bool),
    /// 修改单个账号备注（index, 新备注）。
    Remark(usize, String),
    /// 给勾选账号批量设置备注（新备注）。
    RemarkSelected(String),
    /// 修改单个账号已购公里（index, 新值）。
    Bought(usize, f64),
    /// 给勾选账号的已购公里统一增加（增量）。
    BoughtAddSelected(f64),
    /// 把勾选账号固定为某设备平台（"ios" / "android" / ""=跟随设备页）。
    SetPlatform(String),
    /// 给勾选账号设置专属代理（空串 = 清除，回到全局代理/直连）。
    SetProxy(String),
    /// 查看某个账号的完整设备信息（index）。
    ShowDevice(usize),
    /// 查看某个账号的运动记录（index）。
    ViewRecords(usize),
    /// 全选/全不选（新值）。
    SelectAll(bool),
    /// 勾选/取消勾选搜索可见的账号（原始下标列表, 新值）。
    SelectVisible(Vec<usize>, bool),
}

/// 当日达标状态展示（颜色, 文本）。非当日查询结果不展示。
fn today_text(account: &Account) -> (egui::Color32, &'static str) {
    let today = chrono::Local::now().date_naive();
    let fresh = chrono::Local
        .timestamp_millis_opt(account.today_checked_ms)
        .single()
        .map(|t| t.date_naive())
        == Some(today);
    match (fresh, account.today_ok) {
        (true, Some(true)) => (theme::ok(), "已达标"),
        (true, Some(false)) => (theme::err(), "未达标"),
        _ => (theme::text_dim(), "-"),
    }
}

/// 把「还剩多久解冻」格式化成「2 小时 15 分」这种。
fn remaining_text(until_ms: i64) -> String {
    if until_ms <= 0 {
        return "解冻时间未知".into();
    }
    let left = until_ms - crate::crypto::envelope::now_ms();
    if left <= 0 {
        return "已到期，可再试".into();
    }
    let mins = left / 60_000;
    if mins >= 60 {
        format!("{} 小时 {} 分后解冻", mins / 60, mins % 60)
    } else {
        format!("{} 分钟后解冻", mins.max(1))
    }
}

/// 从登录/提交失败的提示里判断「账号被封禁/风控」，并估算解冻时间。
///
/// 平台没有专门的接口告诉你「封到什么时候」，只能从 message 里认关键词 +
/// 抓「X 小时 / X 天」这类字样；认不出来就按 24 小时估。
pub(crate) fn detect_flag(message: &str) -> Option<(String, i64)> {
    const KEYS: [&str; 8] = ["封禁", "封号", "冻结", "风控", "标记", "黑名单", "异常账号", "限制登录"];
    if !KEYS.iter().any(|key| message.contains(key)) {
        return None;
    }
    // 抓「请过一小时后登录」「24 小时后」这类
    let mut hours = 0.0f64;
    let digits: String = message
        .chars()
        .map(|c| if c.is_ascii_digit() { c } else { ' ' })
        .collect();
    for token in digits.split_whitespace() {
        if let Ok(value) = token.parse::<f64>() {
            hours = hours.max(value);
        }
    }
    if message.contains("天") {
        hours = if hours > 0.0 { hours * 24.0 } else { 24.0 };
    } else if hours <= 0.0 {
        hours = 24.0;
    }
    let until = crate::crypto::envelope::now_ms() + (hours * 3_600_000.0) as i64;
    Some((message.to_string(), until))
}

/// 表格列里用的短倒计时：「2h15m」「45m」「已到期」
fn remaining_short(until_ms: i64) -> String {
    if until_ms <= 0 {
        return "-".into();
    }
    let left = until_ms - crate::crypto::envelope::now_ms();
    if left <= 0 {
        return "已到期".into();
    }
    let mins = left / 60_000;
    if mins >= 60 {
        format!("{}h{}m", mins / 60, mins % 60)
    } else {
        format!("{}m", mins.max(1))
    }
}

fn status_text(account: &Account) -> String {
    // 状态列只说登录状态；是否被标记靠账号名标红来体现。
    if account.logged_in() {
        let uid = account.session.as_ref().map(|s| s.uid).unwrap_or(0);
        let name = account.session.as_ref().map(|s| s.name.clone()).unwrap_or_default();
        if name.is_empty() {
            format!("已登录（uid={uid}）")
        } else {
            format!("已登录：{name}（uid={uid}）")
        }
    } else if account.last_error.is_empty() {
        "未登录".to_string()
    } else {
        format!("登录失败：{}", account.last_error)
    }
}

fn status_color(account: &Account) -> egui::Color32 {
    if account.logged_in() {
        theme::ok()
    } else if account.last_error.is_empty() {
        theme::text_dim()
    } else {
        theme::err()
    }
}

/// 账号实际使用的设备身份。
///
/// - 设备指纹（device_id / idfa / MAC / 安装时间）优先用已持久化的专属身份，
///   否则按账号名确定性派生（同一账号恒定，账号间互异）；
/// - 地理字段（锚点 / 城市）始终实时跟随设备页当前保存值；
/// - 平台 / 机型 / 系统版本：账号固定了就用账号自己的（device_platform /
///   device_model / device_os），没固定则跟随设备页并按账号名确定性挑机型。
pub(crate) fn identity_for(base: &HeaderIdentity, account: &Account) -> HeaderIdentity {
    let mut id = account
        .identity
        .clone()
        .unwrap_or_else(|| crate::crypto::header::derive_per_account(base, &account.username));
    id.anchor_lat = base.anchor_lat;
    id.anchor_lon = base.anchor_lon;
    id.city = base.city.clone();
    let platform = if account.device_platform.is_empty() {
        base.platform.clone()
    } else {
        account.device_platform.clone()
    };
    let (auto_model, auto_os) =
        crate::crypto::header::platform_profile(&platform, &account.username, base);
    id.platform = platform;
    id.device_name = if account.device_model.is_empty() {
        auto_model
    } else {
        account.device_model.clone()
    };
    id.os_version = if account.device_os.is_empty() {
        auto_os
    } else {
        account.device_os.clone()
    };
    id
}

/// 账号平台展示标签：苹果 / 安卓（未固定时跟随设备页）。
fn platform_label(account: &Account, base_platform: &str) -> (&'static str, String) {
    let platform = if account.device_platform.is_empty() {
        base_platform
    } else {
        account.device_platform.as_str()
    };
    let model = if account.device_model.is_empty() {
        String::new()
    } else {
        account.device_model.clone()
    };
    let tag = if platform == "android" { "安卓" } else { "苹果" };
    (tag, model)
}

impl App {
    /// 账号页顶部统计条：数量 / 勾选 / 进度。
    fn draw_accounts_header(&mut self, ui: &mut egui::Ui, busy: bool, logged: usize, total: usize) {
        let selected_count = self.accounts.iter().filter(|a| a.selected).count();
        egui::Frame::none()
            .fill(theme::card_bg())
            .rounding(egui::Rounding::same(8.0))
            .inner_margin(egui::Margin::symmetric(12.0, 8.0))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("账号管理").strong().size(15.0).color(theme::text()));
                    ui.separator();
                    ui.colored_label(
                        if logged > 0 { theme::ok() } else { theme::text_dim() },
                        format!("已登录 {logged} / {total}"),
                    );
                    ui.separator();
                    ui.colored_label(
                        if selected_count > 0 { theme::accent() } else { theme::text_dim() },
                        format!("已勾选 {selected_count}"),
                    );
                    // 手动保存：任何时候点一下就把当前内存状态写盘，并给出明确结果
                    if ui.small_button("保存到磁盘").clicked() {
                        self.status = match self.persist_accounts() {
                            Ok(()) => format!("已保存 {} 个账号到 accounts.json", self.accounts.len()),
                            Err(error) => format!("⚠ 保存失败：{error}"),
                        };
                    }
                    if selected_count > 0 && ui.small_button("清空勾选").clicked() {
                        for account in self.accounts.iter_mut() {
                            account.selected = false;
                        }
                        let _ = self.persist_accounts();
                        self.status = "已清空全部勾选".into();
                    }
                    if let Some(p) = &self.batch_progress {
                        ui.separator();
                        ui.colored_label(
                            theme::warn(),
                            format!("{} {}/{}", p.label, p.done, p.total),
                        );
                    }
                    // 账号文件落盘时间：一眼看出「改动到底存没存」
                    let saved = std::fs::metadata(crate::platform::data_dir().join("accounts.json"))
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as i64);
                    let age_text = saved.map(|ts| {
                        let now = crate::crypto::envelope::now_ms() / 1000;
                        let mins = ((now - ts).max(0)) / 60;
                        if mins < 2 {
                            "刚刚保存".to_string()
                        } else if mins < 60 {
                            format!("{mins} 分钟前保存")
                        } else if mins < 60 * 48 {
                            format!("{} 小时前保存", mins / 60)
                        } else {
                            format!("{} 天前保存", mins / 60 / 24)
                        }
                    });
                    if let Some(text) = &age_text {
                        ui.separator();
                        let stale = saved
                            .map(|ts| crate::crypto::envelope::now_ms() / 1000 - ts > 6 * 3600)
                            .unwrap_or(false);
                        ui.colored_label(
                            if stale { theme::warn() } else { theme::text_dim() },
                            text,
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.colored_label(theme::text_dim(), format!("上限 {MAX_ACCOUNTS}"));
                    });
                });
                if busy {
                    let (done, total) = self
                        .batch_progress
                        .as_ref()
                        .map(|p| (p.done, p.total))
                        .unwrap_or((0, 0));
                    let fraction = if total > 0 { done as f32 / total as f32 } else { 0.0 };
                    ui.add_space(4.0);
                    ui.add(egui::ProgressBar::new(fraction).text(format!("{done}/{total}")));
                }
            });
    }

    /// 一行小标题（分区标题）。
    fn section_title(ui: &mut egui::Ui, text: &str) {
        ui.add_space(10.0);
        ui.label(
            RichText::new(text)
                .strong()
                .size(13.0)
                .color(theme::accent()),
        );
        ui.add_space(2.0);
    }

    pub fn draw_accounts(&mut self, ui: &mut egui::Ui) {
        let busy = self.accounts_busy || self.batch_run_busy;
        let logged = self.logged_account_count();
        let total = self.accounts.len();

        egui::ScrollArea::vertical()
            .id_salt("acct_page_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(8.0);
                // ── 顶部统计条 ──────────────────────────────────
                self.draw_accounts_header(ui, busy, logged, total);

                // ── 账号列表（主区域优先呈现）────────────────────
                Self::section_title(ui, "账号列表");
                if self.accounts.is_empty() {
                    egui::Frame::none()
                        .fill(theme::card_bg())
                        .rounding(egui::Rounding::same(8.0))
                        .inner_margin(egui::Margin::same(16.0))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.vertical_centered(|ui| {
                                ui.add_space(8.0);
                                ui.colored_label(theme::text_dim(), "还没有账号");
                                ui.add_space(4.0);
                                ui.label("用下方「导入账号」添加，或「手动添加」单个录入");
                                ui.add_space(8.0);
                            });
                        });
                } else {
                    // 操作区放在表格上方：按钮紧贴列表，随手可点
                    self.draw_accounts_actions(ui, busy, logged);
                    self.draw_accounts_toolbar(ui, busy);
                    self.draw_accounts_table(ui, busy);
                }

                // ── 失败名单 ────────────────────────────────────
                if !self.accounts_page.last_fails.is_empty() {
                    Self::section_title(ui, "失败名单");
                    egui::Frame::none()
                        .fill(theme::danger_bg())
                        .rounding(egui::Rounding::same(8.0))
                        .inner_margin(egui::Margin::symmetric(12.0, 10.0))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal_wrapped(|ui| {
                                ui.colored_label(
                                    theme::err(),
                                    format!(
                                        "失败 {} 个",
                                        self.accounts_page.last_fails.len()
                                    ),
                                );
                                if ui.button("导出").clicked() {
                                    self.export_fails();
                                }
                                if ui.button("复制").clicked() {
                                    let text =
                                        batch::fail_export_text(&self.accounts_page.last_fails);
                                    ui.ctx().output_mut(|o| o.copied_text = text);
                                    self.status = "失败名单已复制到剪贴板".into();
                                }
                                if ui.button("清除").clicked() {
                                    self.accounts_page.last_fails.clear();
                                    self.accounts_page.last_export_path.clear();
                                }
                            });
                            if !self.accounts_page.last_export_path.is_empty() {
                                ui.colored_label(
                                    theme::ok(),
                                    format!("已导出：{}", self.accounts_page.last_export_path),
                                );
                            }
                            egui::ScrollArea::vertical()
                                .id_salt("acct_fail_scroll")
                                .max_height(120.0)
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    for item in &self.accounts_page.last_fails {
                                        ui.colored_label(
                                            theme::err(),
                                            format!(
                                                "{}----{}  ·  {}",
                                                item.username, item.password, item.message
                                            ),
                                        );
                                    }
                                });
                        });
                }

                ui.add_space(40.0);
            });
    }

    /// 账号表工具栏上方的主操作条：紧贴表格，常用动作一眼可点。
    fn draw_accounts_actions(&mut self, ui: &mut egui::Ui, busy: bool, logged: usize) {
        egui::Frame::none()
            .fill(theme::card_bg())
            .rounding(egui::Rounding::same(8.0))
            .inner_margin(egui::Margin::symmetric(10.0, 8.0))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(!busy, theme::primary_btn("批量登录"))
                        .clicked()
                    {
                        self.start_batch_login();
                    }
                    if ui
                        .add_enabled(!busy && logged > 0, egui::Button::new("批量登出"))
                        .clicked()
                    {
                        self.start_batch_logout();
                    }
                    if ui
                        .add_enabled(!busy && logged > 0, theme::primary_btn("批量刷跑"))
                        .clicked()
                    {
                        self.start_batch_run();
                    }
                    if ui
                        .add_enabled(!busy && logged > 0, egui::Button::new("刷新达标与公里"))
                        .clicked()
                    {
                        self.start_today_check();
                    }
                    if ui
                        .add_enabled(!busy, egui::Button::new("批量复制"))
                        .clicked()
                    {
                        if self.accounts.is_empty() {
                            self.status = "账号列表为空，没有可复制的内容".into();
                        } else {
                            let count = self.accounts.len();
                            let text = self.build_accounts_copy_text();
                            ui.ctx().output_mut(|o| o.copied_text = text);
                            self.status = format!("已复制 {count} 个账号（打码名单）到剪贴板");
                        }
                    }
                    ui.separator();
                    // ── 导入 / 导出 / 手动添加 ──────────────────
                    #[cfg(not(target_os = "android"))]
                    if ui.add_enabled(!busy, egui::Button::new("导入文件")).clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("账号文件", &["txt", "csv", "tsv", "log", "md"])
                            .add_filter("所有文件", &["*"])
                            .pick_file()
                        {
                            self.import_accounts_from_path(&path);
                        }
                    }
                    let paste_label = if self.accounts_page.show_paste {
                        "收起粘贴"
                    } else {
                        "粘贴导入"
                    };
                    if ui.button(paste_label).clicked() {
                        self.accounts_page.show_paste = !self.accounts_page.show_paste;
                    }
                    if ui
                        .add_enabled(!busy, egui::Button::new("导出账号"))
                        .clicked()
                    {
                        self.export_accounts_txt();
                    }
                    if ui
                        .add_enabled(!busy, egui::Button::new("导出明细"))
                        .clicked()
                    {
                        self.export_accounts_csv();
                    }
                    if ui
                        .add_enabled(!busy, egui::Button::new("备份"))
                        .clicked()
                    {
                        self.backup_data();
                        self.status = "√ 数据已备份（accounts/config/identity）".into();
                    }
                    if ui
                        .add_enabled(!busy, egui::Button::new("备份目录"))
                        .clicked()
                    {
                        self.open_backup_dir();
                    }
                    ui.label("添加");
                    mobile::text_edit(
                        ui,
                        "acct_new_user",
                        &mut self.accounts_page.new_user,
                        crate::platform::InputKind::Text,
                        110.0,
                    );
                    mobile::text_edit(
                        ui,
                        "acct_new_pass",
                        &mut self.accounts_page.new_pass,
                        crate::platform::InputKind::Password,
                        95.0,
                    );
                    if ui.add_enabled(!busy, egui::Button::new("+")).clicked() {
                        self.add_account();
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(!busy, egui::Button::new("清空列表"))
                            .clicked()
                        {
                            self.clear_accounts();
                        }
                    });
                });
                // 粘贴导入展开区
                if self.accounts_page.show_paste {
                    ui.add_space(4.0);
                    ui.add(
                        egui::TextEdit::multiline(&mut self.accounts_page.paste_buf)
                            .desired_rows(3)
                            .desired_width(f32::INFINITY)
                            .hint_text("每行一条：账号----密码"),
                    );
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .add_enabled(!busy, theme::primary_btn("导入以上内容"))
                            .clicked()
                        {
                            let text = self.accounts_page.paste_buf.clone();
                            if text.trim().is_empty() {
                                self.status = "粘贴内容为空".into();
                            } else {
                                self.import_accounts_text(&text, "粘贴内容");
                                self.accounts_page.paste_buf.clear();
                            }
                        }
                        if ui.button("清空").clicked() {
                            self.accounts_page.paste_buf.clear();
                        }
                        ui.colored_label(
                            theme::text_dim(),
                            "格式：账号----密码；# 开头为注释",
                        );
                    });
                }
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label("备注");
                    mobile::text_edit(
                        ui,
                        "acct_batch_remark",
                        &mut self.accounts_page.batch_remark,
                        crate::platform::InputKind::Text,
                        110.0,
                    );
                    if ui.add_enabled(!busy, egui::Button::new("设置")).clicked() {
                        let remark = self.accounts_page.batch_remark.trim().to_string();
                        if remark.is_empty() {
                            self.status = "请先填写备注内容".into();
                        } else if !self.accounts.iter().any(|a| a.selected) {
                            self.status = "没有勾选任何账号".into();
                        } else {
                            self.apply_row_action(RowAction::RemarkSelected(remark));
                        }
                    }
                    ui.label("已购");
                    mobile::text_edit(
                        ui,
                        "acct_batch_bought",
                        &mut self.accounts_page.batch_bought,
                        crate::platform::InputKind::Text,
                        55.0,
                    );
                    if ui.add_enabled(!busy, egui::Button::new("加到勾选")).clicked() {
                        match self.accounts_page.batch_bought.trim().parse::<f64>() {
                            Ok(v) if v > 0.0 => {
                                if !self.accounts.iter().any(|a| a.selected) {
                                    self.status = "没有勾选任何账号".into();
                                } else {
                                    self.apply_row_action(RowAction::BoughtAddSelected(v));
                                }
                            }
                            _ => self.status = "请输入大于 0 的数字".into(),
                        }
                    }
                    // 右侧：并发与选项
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.checkbox(&mut self.accounts_page.show_password, "显示密码");
                        let mut value = self.accounts_page.concurrency as i64;
                        let resp = mobile::drag_i64(ui, "acct_concurrency", &mut value, 1..=32, 1.0, "", "");
                        if resp.changed() {
                            self.accounts_page.concurrency = value.clamp(1, 32) as usize;
                            self.config.concurrency = self.accounts_page.concurrency;
                            let _ = model::save_config(&self.config);
                        }
                        ui.colored_label(theme::text_dim(), "并发");
                    });
                });
                ui.add_space(4.0);
                // ── 每账号固定设备平台 ─────────────────────────
                ui.horizontal_wrapped(|ui| {
                    ui.label("设备平台");
                    if ui.add_enabled(!busy, egui::Button::new("苹果")).clicked() {
                        self.apply_row_action(RowAction::SetPlatform("ios".into()));
                    }
                    if ui.add_enabled(!busy, egui::Button::new("安卓")).clicked() {
                        self.apply_row_action(RowAction::SetPlatform("android".into()));
                    }
                    if ui.add_enabled(!busy, egui::Button::new("跟随设备页")).clicked() {
                        self.apply_row_action(RowAction::SetPlatform(String::new()));
                    }
                    let ios = self
                        .accounts
                        .iter()
                        .filter(|a| a.device_platform == "ios")
                        .count();
                    let android = self
                        .accounts
                        .iter()
                        .filter(|a| a.device_platform == "android")
                        .count();
                    let follow = self
                        .accounts
                        .iter()
                        .filter(|a| a.device_platform.is_empty())
                        .count();
                    ui.colored_label(
                        theme::text_dim(),
                        format!("苹果 {ios} / 安卓 {android} / 跟随 {follow}"),
                    );
                    ui.colored_label(theme::text_dim(), "（对勾选账号生效）");
                    ui.separator();
                    if ui
                        .add_enabled(!busy, egui::Button::new("违规自查"))
                        .clicked()
                    {
                        self.start_cheat_check();
                    }
                });
                ui.add_space(4.0);
                // ── 代理（防同 IP 关联）─────────────────────────
                ui.horizontal_wrapped(|ui| {
                    ui.label("代理");
                    mobile::text_edit(
                        ui,
                        "acct_global_proxy",
                        &mut self.config.proxy,
                        crate::platform::InputKind::Text,
                        210.0,
                    );
                    if ui.small_button("保存全局").clicked() {
                        let _ = model::save_config(&self.config);
                        crate::api::client::set_global_proxy(
                            self.proxy_for(self.active_account.and_then(|i| self.accounts.get(i))),
                        );
                        self.status = "全局代理已保存".into();
                    }
                    ui.separator();
                    ui.label("账号代理");
                    mobile::text_edit(
                        ui,
                        "acct_account_proxy",
                        &mut self.accounts_page.batch_proxy,
                        crate::platform::InputKind::Text,
                        210.0,
                    );
                    if ui
                        .add_enabled(!busy, egui::Button::new("设置到勾选"))
                        .clicked()
                    {
                        if !self.accounts.iter().any(|a| a.selected) {
                            self.status = "没有勾选任何账号".into();
                        } else {
                            let px = self.accounts_page.batch_proxy.clone();
                            self.apply_row_action(RowAction::SetProxy(px));
                        }
                    }
                    if ui
                        .add_enabled(!busy, egui::Button::new("清除"))
                        .clicked()
                    {
                        if self.accounts.iter().any(|a| a.selected) {
                            self.apply_row_action(RowAction::SetProxy(String::new()));
                        }
                    }
                    ui.colored_label(
                        theme::text_dim(),
                        "格式 socks5://user:pass@host:port 或 http://host:port",
                    );
                });
                ui.add_space(4.0);
                // ── 定时分散刷跑（降低集中提交特征）─────────────
                ui.horizontal_wrapped(|ui| {
                    ui.checkbox(&mut self.config.auto_run, "定时分散刷跑");
                    if self.config.auto_run {
                        let mut start = self.config.auto_start_hour;
                        let mut end = self.config.auto_end_hour;
                        ui.label("窗口");
                        let r1 = mobile::drag_i64(ui, "auto_start", &mut start, 0..=23, 1.0, "", "时");
                        ui.label("~");
                        let r2 = mobile::drag_i64(ui, "auto_end", &mut end, 1..=24, 1.0, "", "时");
                        ui.label("间隔");
                        let mut lo = self.config.auto_gap_min;
                        let mut hi = self.config.auto_gap_max;
                        let r3 = mobile::drag_i64(ui, "auto_lo", &mut lo, 1..=240, 1.0, "", "分");
                        ui.label("-");
                        let r4 = mobile::drag_i64(ui, "auto_hi", &mut hi, 1..=480, 1.0, "", "分");
                        ui.label("每次");
                        let mut batch = self.config.auto_batch;
                        let r5 = mobile::drag_i64(ui, "auto_batch", &mut batch, 1..=10, 1.0, "", "个");
                        if r1.changed() || r2.changed() || r3.changed() || r4.changed() || r5.changed() {
                            self.config.auto_start_hour = start.clamp(0, 23);
                            self.config.auto_end_hour = end.clamp(1, 24);
                            self.config.auto_gap_min = lo.clamp(1, 240);
                            self.config.auto_gap_max = hi.clamp(1, 480);
                            self.config.auto_batch = batch.clamp(1, 10);
                            self.config.auto_next_ms = 0;
                            let _ = model::save_config(&self.config);
                        }
                        if self.config.auto_next_ms > 0 {
                            let t = chrono::Local
                                .timestamp_millis_opt(self.config.auto_next_ms)
                                .single()
                                .map(|d| d.format("%m-%d %H:%M").to_string())
                                .unwrap_or_default();
                            ui.colored_label(theme::text_dim(), format!("下次 {t}"));
                        }
                    } else {
                        ui.colored_label(
                            theme::text_dim(),
                            "开启后按随机间隔分批跑「勾选且已登录」的账号，每天每号一次",
                        );
                    }
                });
                // 使用提示（固定在操作条底部，随提示折叠）
                ui.add_space(2.0);
                egui::CollapsingHeader::new(
                    RichText::new("使用提示").small().color(theme::text_dim()),
                )
                .default_open(false)
                .show(ui, |ui| {
                    ui.colored_label(
                        theme::text_dim(),
                        "每个账号自动使用专属设备指纹（device_id/MAC/机型和安装时间互不相同）；定位锚点、城市跟随「设备信息」页。",
                    );
                    ui.colored_label(
                        theme::text_dim(),
                        "需要区分苹果/安卓时：勾选账号后点上方「设备平台」按钮固定，之后登录和刷跑都用该平台。",
                    );
                    ui.colored_label(
                        theme::text_dim(),
                        "批量登录/登出/刷跑只作用于勾选账号；筛选生效时仅作用于可见账号。",
                    );
                    ui.colored_label(
                        theme::text_dim(),
                        "防关联：每个账号独立设备指纹；可再给每个账号配独立代理（socks5/http），避免同 IP 集中提交。",
                    );
                    ui.colored_label(
                        theme::text_dim(),
                        "定时分散刷跑：在窗口内按随机间隔分批跑（每天每号一次），削弱集中批量特征。",
                    );
                    ui.colored_label(
                        theme::text_dim(),
                        "违规自查：定期对所有已登录账号查是否被标记，发现异常立即停手。",
                    );
                });
                // 实时运行日志：批量任务时自动展开，逐账号显示详细步骤
                ui.add_space(4.0);
                egui::CollapsingHeader::new(
                    RichText::new(format!("运行日志（{} 行）", self.log.len()))
                        .small()
                        .color(theme::text_dim()),
                )
                .default_open(busy)
                .show(ui, |ui| {
                    egui::Frame::none()
                        .fill(egui::Color32::from_rgb(250, 252, 251))
                        .stroke(egui::Stroke::new(1.0_f32, theme::card_border()))
                        .rounding(egui::Rounding::same(6.0))
                        .inner_margin(egui::Margin::same(6.0))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            self.log.render_tail(ui, 220.0);
                        });
                });
            });
    }

    /// 账号表工具栏：搜索 + 备注筛选 + 已购开关。
    fn draw_accounts_toolbar(&mut self, ui: &mut egui::Ui, busy: bool) {
        let _ = busy;
        ui.horizontal_wrapped(|ui| {
            ui.label("搜索");
            mobile::text_edit(
                ui,
                "acct_search",
                &mut self.accounts_page.search,
                crate::platform::InputKind::Text,
                180.0,
            );
            if !self.accounts_page.search.is_empty() && ui.small_button("✕").clicked() {
                self.accounts_page.search.clear();
            }
        });
        let mut remarks: Vec<String> = self
            .accounts
            .iter()
            .map(|a| a.remark.clone())
            .filter(|r| !r.is_empty())
            .collect();
        remarks.sort();
        remarks.dedup();
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("备注").color(theme::text_dim()));
            if ui
                .selectable_label(self.accounts_page.remark_filter.is_none(), "全部")
                .clicked()
            {
                self.accounts_page.remark_filter = None;
            }
            for r in &remarks {
                let selected = self.accounts_page.remark_filter.as_deref() == Some(r.as_str());
                if ui.selectable_label(selected, r).clicked() {
                    self.accounts_page.remark_filter = Some(r.clone());
                }
            }
            if !remarks.is_empty() {
                let none = self
                    .accounts_page
                    .remark_filter
                    .as_ref()
                    .map(|s| s.is_empty())
                    .unwrap_or(false);
                if ui.selectable_label(none, "无备注").clicked() {
                    self.accounts_page.remark_filter = Some(String::new());
                }
            }
            ui.separator();
            let ob = self.accounts_page.only_bought;
            if ui.selectable_label(ob, "仅看已购").clicked() {
                self.accounts_page.only_bought = !ob;
            }
        });
        ui.add_space(4.0);
    }

    /// 账号表格（自适应宽度）。
    fn draw_accounts_table(&mut self, ui: &mut egui::Ui, busy: bool) {
        let is_filtered = !self.accounts_page.search.trim().is_empty()
            || self.accounts_page.remark_filter.is_some()
            || self.accounts_page.only_bought;
        let visible: Vec<usize> = self
            .filtered_indices()
            .unwrap_or_else(|| (0..self.accounts.len()).collect());
        if visible.is_empty() {
            ui.add_space(8.0);
            ui.colored_label(
                theme::text_dim(),
                format!("没有匹配「{}」的账号", self.accounts_page.search.trim()),
            );
            return;
        }
        let mut action: Option<RowAction> = None;
        let compact = mobile::compact_ui(ui);
        let show_password = self.accounts_page.show_password;
        let active = self.active_account;
        let base_platform = self.identity.platform.clone();
        if compact {
            for &index in &visible {
                let account = &self.accounts[index];
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        let mut selected = account.selected;
                        if ui.checkbox(&mut selected, "").changed() {
                            action = Some(RowAction::Select(index, selected));
                        }
                        ui.label(
                            RichText::new(format!("{}. {}", index + 1, account.username))
                                .strong()
                                .color(theme::text()),
                        );
                        let shown_name = if !account.name.is_empty() {
                            account.name.as_str()
                        } else {
                            account
                                .session
                                .as_ref()
                                .map(|s| s.name.as_str())
                                .unwrap_or("")
                        };
                        if !shown_name.is_empty() {
                            ui.colored_label(theme::text_dim(), shown_name);
                        }
                        let (ptag, pmodel) = platform_label(account, &base_platform);
                        ui.colored_label(
                            theme::text_dim(),
                            if pmodel.is_empty() {
                                ptag.to_string()
                            } else {
                                format!("{ptag}·{pmodel}")
                            },
                        );
                        if active == Some(index) {
                            ui.colored_label(theme::ok(), "当前账号");
                        }
                        ui.colored_label(status_color(account), status_text(account));
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.colored_label(
                            theme::text_dim(),
                            if show_password {
                                account.password.as_str()
                            } else {
                                "••••••"
                            },
                        );
                        let (today_color, today_label) = today_text(account);
                        if today_label != "-" {
                            ui.colored_label(today_color, format!("今日{today_label}"));
                        }
                        if !account.last_run.is_empty() {
                            ui.colored_label(theme::text_dim(), &account.last_run);
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("备注").color(theme::text_dim()));
                        if let Some(remark) = remark_edit(ui, index, account) {
                            action = Some(RowAction::Remark(index, remark));
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
                        if account.semester_checked_ms > 0 {
                            ui.label(RichText::new("公里").color(theme::text_dim()));
                            ui.colored_label(
                                theme::text(),
                                format!("有效 {:.2}", account.semester_valid_km),
                            );
                            if account.goal_abnormal {
                                ui.colored_label(theme::err(), "目标异常→48");
                            } else {
                                ui.colored_label(
                                    theme::text_dim(),
                                    format!("目标 {:.1}", account.semester_goal_km),
                                );
                            }
                            ui.label(RichText::new("已购").color(theme::text_dim()));
                            if let Some(new_bought) = bought_edit(ui, index, account) {
                                action = Some(RowAction::Bought(index, new_bought));
                            }
                            if account.bought_km > 0.0 {
                                ui.colored_label(
                                    theme::warn(),
                                    format!(
                                        "购剩 {:.2}",
                                        (account.bought_km - account.semester_valid_km)
                                            .max(0.0)
                                    ),
                                );
                            }
                        } else {
                            ui.colored_label(theme::text_dim(), "公里未拉取");
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
                        if let Some(picked) = row_buttons(ui, index, account, busy) {
                            action = Some(picked);
                        }
                    });
                });
                ui.add_space(4.0);
            }
        } else {
            // 自适应宽度表格
            TableBuilder::new(ui)
                .id_salt("acct_table")
                .striped(true)
                .vscroll(false)
                .auto_shrink([false, false])
                .column(Column::auto().at_least(24.0))
                .column(Column::auto().at_least(40.0))
                .column(Column::auto().at_least(120.0))
                .column(Column::auto().at_least(140.0))
                .column(Column::auto().at_least(85.0))
                .column(Column::auto().at_least(65.0))
                .column(Column::auto().at_least(85.0))
                .column(Column::auto().at_least(95.0))
                .column(Column::remainder().at_least(140.0))
                .column(Column::auto().at_least(70.0))
                .column(Column::remainder().at_least(130.0))
                .column(Column::auto().at_least(260.0))
                .header(26.0, |mut h| {
                    let all_selected = if !is_filtered {
                        self.accounts.iter().all(|a| a.selected)
                    } else {
                        visible.iter().all(|&i| self.accounts[i].selected)
                    };
                    let mut checkbox = all_selected;
                    h.col(|ui| {
                        if ui.checkbox(&mut checkbox, "").changed() {
                            action = if !is_filtered {
                                Some(RowAction::SelectAll(checkbox))
                            } else {
                                Some(RowAction::SelectVisible(visible.clone(), true))
                            };
                        }
                    });
                    for t in [
                        "序号", "账号", "备注", "密码", "有效", "剩余", "已购", "状态",
                        "今日", "最近刷跑", "操作",
                    ] {
                        h.col(|ui| {
                            ui.label(RichText::new(t).color(theme::text_dim()).small());
                        });
                    }
                })
                .body(|body| {
                    body.rows(76.0, visible.len(), |mut row| {
                        let index = visible[row.index()];
                        let account = &self.accounts[index];
                        row.col(|ui| {
                            let mut selected = account.selected;
                            if ui.checkbox(&mut selected, "").changed() {
                                action = Some(RowAction::Select(index, selected));
                            }
                        });
                        row.col(|ui| {
                            ui.label(RichText::new(format!("{}", index + 1)).color(theme::text_dim()));
                        });
                        row.col(|ui| {
                            ui.vertical(|ui| {
                                // 被平台标记 / 违规自查命中 → 账号名标红
                                let flagged = !account.flagged_reason.is_empty()
                                    || account.cheat_flag == Some(true);
                                ui.label(
                                    RichText::new(&account.username)
                                        .color(if flagged { theme::err() } else { theme::text() }),
                                );
                                let shown_name = if !account.name.is_empty() {
                                    account.name.as_str()
                                } else {
                                    account
                                        .session
                                        .as_ref()
                                        .map(|s| s.name.as_str())
                                        .unwrap_or("")
                                };
                                if !shown_name.is_empty() {
                                    ui.label(
                                        RichText::new(shown_name)
                                            .color(theme::text_dim())
                                            .small(),
                                    );
                                }
                                let (ptag, pmodel) = platform_label(account, &base_platform);
                                ui.label(
                                    RichText::new(if pmodel.is_empty() {
                                        ptag.to_string()
                                    } else {
                                        format!("{ptag}·{pmodel}")
                                    })
                                    .color(theme::text_dim())
                                    .small(),
                                );
                            });
                        });
                        row.col(|ui| {
                            if let Some(remark) = remark_edit(ui, index, account) {
                                action = Some(RowAction::Remark(index, remark));
                            }
                        });
                        row.col(|ui| {
                            ui.label(
                                RichText::new(if show_password {
                                    account.password.as_str()
                                } else {
                                    "••••••"
                                })
                                .color(theme::text_dim()),
                            );
                        });
                        row.col(|ui| {
                            if account.semester_checked_ms <= 0 {
                                ui.label(RichText::new("-").color(theme::text_dim()).small());
                            } else {
                                ui.label(
                                    RichText::new(format!("{:.2}", account.semester_valid_km))
                                        .color(theme::text()),
                                );
                            }
                        });
                        row.col(|ui| {
                            if account.semester_checked_ms <= 0 {
                                ui.label(RichText::new("-").color(theme::text_dim()).small());
                            } else {
                                ui.vertical(|ui| {
                                    ui.label(
                                        RichText::new(format!(
                                            "剩 {:.2}",
                                            (account.semester_goal_km
                                                - account.semester_valid_km)
                                                .max(0.0)
                                        ))
                                        .color(theme::text()),
                                    );
                                    if account.goal_abnormal {
                                        ui.label(
                                            RichText::new("异常→48")
                                                .color(theme::err())
                                                .small(),
                                        );
                                    } else {
                                        ui.label(
                                            RichText::new(format!(
                                                "目标 {:.1}",
                                                account.semester_goal_km
                                            ))
                                            .color(theme::text_dim())
                                            .small(),
                                        );
                                    }
                                });
                            }
                        });
                        row.col(|ui| {
                            ui.vertical(|ui| {
                                if let Some(new_bought) = bought_edit(ui, index, account) {
                                    action = Some(RowAction::Bought(index, new_bought));
                                }
                                if account.bought_km > 0.0 {
                                    let remain =
                                        (account.bought_km - account.semester_valid_km).max(0.0);
                                    ui.label(
                                        RichText::new(format!("购剩 {remain:.2}"))
                                            .color(theme::warn())
                                            .small(),
                                    );
                                }
                            });
                        });
                        row.col(|ui| {
                            ui.label(
                                RichText::new({
                                    let mut text = status_text(account);
                                    if active == Some(index) {
                                        text.push_str("（当前）");
                                    }
                                    text
                                })
                                .color(status_color(account)),
                            );
                        });
                        row.col(|ui| {
                            let (today_color, today_label) = today_text(account);
                            ui.colored_label(today_color, today_label);
                        });
                        row.col(|ui| {
                            ui.label(RichText::new(&account.last_run).color(theme::text_dim()));
                        });
                        row.col(|ui| {
                            ui.horizontal_wrapped(|ui| {
                                if let Some(picked) = row_buttons(ui, index, account, busy) {
                                    action = Some(picked);
                                }
                            });
                        });
                    });
                });
        }
        if let Some(picked) = action {
            self.apply_row_action(picked);
        }
    }
}

/// 行内备注编辑：值一变就返回新备注（立即持久化），避免逐帧重建缓冲区吞掉输入法提交的中文。
fn remark_edit(ui: &mut egui::Ui, index: usize, account: &Account) -> Option<String> {
    let mut remark = account.remark.clone();
    let _resp = mobile::text_edit(
        ui,
        ("acct_remark", index),
        &mut remark,
        crate::platform::InputKind::Text,
        130.0,
    );
    if remark != account.remark {
        Some(remark)
    } else {
        None
    }
}

/// 秒 → mm:ss 简短时长。
fn fmt_duration(sec: i64) -> String {
    let s = sec.max(0);
    format!("{}分{:02}秒", s / 60, s % 60)
}

/// CSV 字段转义：含逗号/引号/换行时加引号包裹。
fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// 姓名中间打码：保留首尾字符，其余用 *。
fn mask_name(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    match chars.len() {
        0 => String::new(),
        1 => chars[0].to_string(),
        2 => format!("{}*", chars[0]),
        n => format!("{}{}{}", chars[0], "*".repeat(n - 2), chars[n - 1]),
    }
}

/// 手机号只保留前三后二，中间打码（按数字提取）。
fn mask_phone(phone: &str) -> String {
    let digits: Vec<char> = phone.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 5 {
        return phone.to_string();
    }
    let head: String = digits[..3].iter().collect();
    let tail: String = digits[digits.len() - 2..].iter().collect();
    format!("{head}{}{tail}", "*".repeat(digits.len() - 5))
}

/// 行内已购公里编辑：值一变立即持久化；清空=0；非数字保持原值。
fn bought_edit(ui: &mut egui::Ui, index: usize, account: &Account) -> Option<f64> {
    let mut buf = if account.bought_km > 0.0 {
        format!("{}", account.bought_km)
    } else {
        String::new()
    };
    let _resp = mobile::text_edit(
        ui,
        ("acct_bought", index),
        &mut buf,
        crate::platform::InputKind::Text,
        60.0,
    );
    let trimmed = buf.trim();
    let new_val = if trimmed.is_empty() {
        0.0
    } else {
        trimmed.parse::<f64>().unwrap_or(account.bought_km)
    };
    if (new_val - account.bought_km).abs() > f64::EPSILON {
        Some(new_val)
    } else {
        None
    }
}

fn row_buttons(
    ui: &mut egui::Ui,
    index: usize,
    account: &Account,
    busy: bool,
) -> Option<RowAction> {
    let logged_in = account.logged_in();
    let mut action = None;
    if ui
        .add_enabled(!busy && !logged_in, egui::Button::new("登录"))
        .clicked()
    {
        action = Some(RowAction::Login(index));
    }
    if ui
        .add_enabled(!busy && logged_in, egui::Button::new("登出"))
        .clicked()
    {
        action = Some(RowAction::Logout(index));
    }
    if ui
        .add_enabled(!busy && logged_in, egui::Button::new("设为当前"))
        .clicked()
    {
        action = Some(RowAction::Activate(index));
    }
    if ui
        .add_enabled(!busy && logged_in, egui::Button::new("记录"))
        .clicked()
    {
        action = Some(RowAction::ViewRecords(index));
    }
    if ui
        .add_enabled(!busy, egui::Button::new("设备"))
        .clicked()
    {
        action = Some(RowAction::ShowDevice(index));
    }
    if ui
        .add_enabled(!busy, egui::Button::new("删除"))
        .clicked()
    {
        action = Some(RowAction::Delete(index));
    }
    action
}

impl App {
    fn apply_row_action(&mut self, action: RowAction) {
        match action {
            RowAction::Select(index, selected) => {
                if let Some(account) = self.accounts.get_mut(index) {
                    account.selected = selected;
                    let _ = self.persist_accounts();
                }
            }
            RowAction::Remark(index, remark) => {
                if let Some(account) = self.accounts.get_mut(index) {
                    account.remark = remark;
                    let _ = self.persist_accounts();
                }
            }
            RowAction::RemarkSelected(remark) => {
                let mut n = 0;
                for account in self.accounts.iter_mut() {
                    if account.selected {
                        account.remark = remark.clone();
                        n += 1;
                    }
                }
                let _ = self.persist_accounts();
                self.status = format!("已给 {n} 个勾选账号设置备注");
            }
            RowAction::Bought(index, value) => {
                if let Some(account) = self.accounts.get_mut(index) {
                    account.bought_km = value;
                    let _ = self.persist_accounts();
                }
            }
            RowAction::BoughtAddSelected(add) => {
                let mut n = 0;
                for account in self.accounts.iter_mut() {
                    if account.selected {
                        account.bought_km += add;
                        n += 1;
                    }
                }
                let _ = self.persist_accounts();
                self.status = format!("已给 {n} 个勾选账号增加 {add} 公里（已购）");
            }
            RowAction::SetProxy(proxy) => {
                let mut n = 0;
                for account in self.accounts.iter_mut() {
                    if account.selected {
                        account.proxy = proxy.trim().to_string();
                        n += 1;
                    }
                }
                let _ = self.persist_accounts();
                self.status = if proxy.trim().is_empty() {
                    format!("已清除 {n} 个勾选账号的专属代理")
                } else {
                    format!("已给 {n} 个勾选账号设置专属代理")
                };
            }
            RowAction::SetPlatform(platform) => {
                let mut n = 0;
                for account in self.accounts.iter_mut() {
                    if account.selected {
                        account.device_platform = platform.clone();
                        // 平台变了就重挑机型与系统版本（清空后按平台自动挑选）
                        account.device_model.clear();
                        account.device_os.clear();
                        n += 1;
                    }
                }
                let _ = self.persist_accounts();
                let label = match platform.as_str() {
                    "android" => "安卓",
                    "ios" => "苹果",
                    _ => "跟随设备页",
                };
                self.status = format!("已把 {n} 个勾选账号固定为{label}设备");
            }
            RowAction::ViewRecords(index) => {
                if self.accounts.get(index).map(|a| a.logged_in()).unwrap_or(false) {
                    self.records_view = Some(index);
                    self.tab = TAB_RECORDS;
                    self.refresh_records();
                }
            }
            RowAction::SelectAll(selected) => {
                for account in self.accounts.iter_mut() {
                    account.selected = selected;
                }
                let _ = self.persist_accounts();
                self.status = if selected {
                    "已勾选全部账号".into()
                } else {
                    "已取消勾选全部账号".into()
                };
            }
            RowAction::SelectVisible(indices, selected) => {
                let mut n = 0;
                for index in indices {
                    if let Some(account) = self.accounts.get_mut(index) {
                        if account.selected != selected {
                            account.selected = selected;
                            n += 1;
                        }
                    }
                }
                let _ = self.persist_accounts();
                self.status = if selected {
                    format!("已勾选筛选可见的 {n} 个账号（跨筛选累计）")
                } else {
                    format!("已取消勾选筛选可见的 {n} 个账号")
                };
            }
            RowAction::Login(index) => self.start_login_index(index),
            RowAction::Logout(index) => self.start_logout_index(index),
            RowAction::Activate(index) => {
                self.set_active_account(index);
                if self.session.is_some() {
                    self.refresh_data_page();
                    self.refresh_records();
                    self.refresh_user_page();
                    self.refresh_ai_list();
                    let name = self
                        .session
                        .as_ref()
                        .map(|s| s.name.clone())
                        .unwrap_or_default();
                    self.status = format!("当前账号已切换为 {name}");
                }
            }
            RowAction::Delete(index) => self.remove_account(index),
            RowAction::ShowDevice(index) => {
                let Some(account) = self.accounts.get(index) else {
                    return;
                };
                let id = identity_for(&self.identity, account);
                let install = chrono::Local
                    .timestamp_millis_opt(id.app_install_time)
                    .single()
                    .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_else(|| "-".into());
                let platform = if id.platform == "android" {
                    "安卓 Android"
                } else {
                    "苹果 iOS"
                };
                let fixed = if account.device_platform.is_empty() {
                    "跟随设备页"
                } else {
                    "已固定"
                };
                let lines = vec![
                    format!("账号：{}", account.username),
                    format!(
                        "名字：{}",
                        if account.name.is_empty() {
                            "-"
                        } else {
                            account.name.as_str()
                        }
                    ),
                    format!("平台：{platform}（{fixed}）"),
                    format!("机型/设备名：{}", id.device_name),
                    format!("系统版本：{}", id.os_version),
                    format!(
                        "制造商：{}",
                        if id.manufacturer.is_empty() {
                            "-"
                        } else {
                            id.manufacturer.as_str()
                        }
                    ),
                    format!("设备 ID：{}", id.device_id),
                    format!("IDFA：{}", id.idfa),
                    format!("MAC：{}", id.mac_address),
                    format!("安装时间：{install}"),
                    format!(
                        "定位锚点：{:.6}, {:.6}（{}）",
                        id.anchor_lat, id.anchor_lon, id.city
                    ),
                    String::new(),
                    "该设备身份随账号固定保存，登录 / 刷跑 / 数据请求全程一致；".into(),
                    "定位锚点与城市跟随「设备信息」页，其余字段账号专属。".into(),
                ];
                self.popup = Some(PopupInfo {
                    title: format!("设备信息 · {}", account.username),
                    lines,
                });
            }
        }
    }

    // ── 列表维护 ───────────────────────────────────────────────

    pub(crate) fn remove_account(&mut self, index: usize) {
        if index >= self.accounts.len() {
            return;
        }
        let removed = self.accounts.remove(index);
        if self.active_account == Some(index) {
            self.active_account = None;
            self.session = None;
            model::clear_session();
        } else if let Some(active) = self.active_account {
            if active > index {
                self.active_account = Some(active - 1);
            }
        }
        let _ = self.persist_accounts();
        self.status = format!("已从列表移除 {}（未调用登出接口）", removed.username);
    }

    pub(crate) fn clear_accounts(&mut self) {
        if self.accounts.is_empty() {
            self.status = "账号列表本来就是空的".into();
            return;
        }
        if self.accounts_busy || self.batch_run_busy {
            self.status = "批量任务进行中，请稍后再试".into();
            return;
        }
        let count = self.accounts.len();
        let global_proxy = self.proxy_for(None);
        let sessions: Vec<(String, model::Session, HeaderIdentity, Option<String>)> = self
            .accounts
            .drain(..)
            .filter_map(|a| {
                let ident = identity_for(&self.identity, &a);
                let proxy = if a.proxy.trim().is_empty() {
                    global_proxy.clone()
                } else {
                    Some(a.proxy.trim().to_string())
                };
                a.session.map(|s| (a.username, s, ident, proxy))
            })
            .collect();
        self.active_account = None;
        self.session = None;
        model::clear_session();
        let _ = self.persist_accounts();
        if !sessions.is_empty() {
            let concurrency = self.accounts_page.concurrency.clamp(1, 32);
            self.spawn_job(move |tx| {
                batch::parallel_map(sessions, concurrency, move |(username, session, identity, proxy), _| {
                    let mut raw_log = App::logger(tx.clone());
                    let mut log = |m: &str| raw_log(&format!("[{username}] {m}"));
                    match crate::api::client::ApiClient::with_proxy(identity, Some(session), proxy) {
                        Ok(mut client) => crate::api::login::logout_request(&mut client, &mut log),
                        Err(e) => log(&format!("× [账号] {username} 登出跳过：{e}")),
                    }
                    log(&format!("√ [账号] {username} 已登出"));
                });
            });
        }
        self.status = format!("已清空 {count} 个账号");
    }

    pub(crate) fn add_account(&mut self) {
        if self.accounts_busy || self.batch_run_busy {
            self.status = "批量任务进行中，请稍后再试".into();
            return;
        }
        let username = self.accounts_page.new_user.trim().to_string();
        let password = self.accounts_page.new_pass.clone();
        if username.is_empty() || password.is_empty() {
            self.status = "请输入账号和密码".into();
            return;
        }
        let index = match self.accounts.iter().position(|a| a.username == username) {
            Some(index) => {
                self.accounts[index].password = password;
                index
            }
            None => {
                if self.accounts.len() >= MAX_ACCOUNTS {
                    self.status = format!("账号数量已达上限 {MAX_ACCOUNTS} 个");
                    return;
                }
                // 添加即生成并固定专属设备身份
                let ident =
                    crate::crypto::header::derive_per_account(&self.identity, &username);
                self.accounts.push(Account {
                    username: username.clone(),
                    password,
                    identity: Some(ident),
                    ..Default::default()
                });
                self.accounts.len() - 1
            }
        };
        self.accounts_page.new_user.clear();
        self.accounts_page.new_pass.clear();
        let _ = self.persist_accounts();
        self.status = format!("已添加 {username}，正在登录…");
        self.start_login_index(index);
    }

    // ── 导入 ───────────────────────────────────────────────────

    pub(crate) fn import_accounts_from_path(&mut self, path: &std::path::Path) {
        match std::fs::read(path) {
            Ok(bytes) => {
                // 账号密码基本是 ASCII：非 UTF-8 文件按有损方式解码，避免直接失败
                let text = String::from_utf8_lossy(&bytes).to_string();
                self.import_accounts_text(&text, &path.display().to_string());
            }
            Err(e) => self.status = format!("× 读取失败：{e}"),
        }
    }

    pub(crate) fn import_accounts_text(&mut self, text: &str, source: &str) {
        let parsed = batch::parse_accounts(text);
        let mut added = 0usize;
        let mut updated = 0usize;
        for (username, password) in &parsed.entries {
            match self.accounts.iter().position(|a| &a.username == username) {
                Some(index) => {
                    if self.accounts[index].password != *password {
                        self.accounts[index].password = password.clone();
                        updated += 1;
                    }
                }
                None => {
                    if self.accounts.len() >= MAX_ACCOUNTS {
                        break;
                    }
                    // 导入即生成并固定该账号的专属设备身份（device_id/MAC/idfa/机型/安装时间）
                    let ident =
                        crate::crypto::header::derive_per_account(&self.identity, username);
                    self.accounts.push(Account {
                        username: username.clone(),
                        password: password.clone(),
                        identity: Some(ident),
                        ..Default::default()
                    });
                    added += 1;
                }
            }
        }
        let _ = self.persist_accounts();
        let mut lines = vec![
            format!("来源：{source}"),
            format!("解析成功：{} 条", parsed.entries.len()),
            format!("新增账号：{added} 个"),
            format!("更新密码：{updated} 个"),
            format!("当前账号：{} / {MAX_ACCOUNTS}", self.accounts.len()),
        ];
        let problems = parsed.problems();
        if !problems.is_empty() {
            lines.push(String::new());
            lines.push("需要注意：".into());
            lines.extend(problems);
        }
        self.popup = Some(PopupInfo {
            title: "账号导入完成".into(),
            lines,
        });
        self.status = format!(
            "导入完成：新增 {added}，更新 {updated}，当前共 {} 个账号",
            self.accounts.len()
        );
        self.tab = TAB_ACCOUNTS;
    }

    pub(crate) fn export_fails(&mut self) {
        let fails = self.accounts_page.last_fails.clone();
        if fails.is_empty() {
            self.status = "当前没有失败记录".into();
            return;
        }
        let text = batch::fail_export_text(&fails);
        let name = format!("失败账号-{}.txt", chrono::Local::now().format("%Y%m%d-%H%M%S"));
        let path = crate::platform::data_dir().join(name);
        match std::fs::write(&path, text) {
            Ok(()) => {
                self.accounts_page.last_export_path = path.display().to_string();
                self.status = format!("√ 失败名单已导出：{}", path.display());
            }
            Err(e) => self.status = format!("× 导出失败：{e}"),
        }
    }

    // ── 导出 / 备份 ─────────────────────────────────────────────

    /// 导出全部账号（账号----密码，可直接重新导入）。
    pub(crate) fn export_accounts_txt(&mut self) {
        if self.accounts.is_empty() {
            self.status = "账号列表为空，没有可导出的内容".into();
            return;
        }
        let mut lines: Vec<String> = Vec::new();
        for a in &self.accounts {
            if !a.username.is_empty() {
                lines.push(format!("{}----{}", a.username, a.password));
            }
        }
        let text = lines.join("\r\n");
        let name = format!("账号导出-{}.txt", chrono::Local::now().format("%Y%m%d-%H%M%S"));
        let path = crate::platform::data_dir().join(&name);
        match std::fs::write(&path, text) {
            Ok(()) => self.status = format!(
                "√ 已导出 {} 个账号（导入格式）：{}",
                self.accounts.len(),
                path.display()
            ),
            Err(e) => self.status = format!("× 导出失败：{e}"),
        }
    }

    /// 导出账号明细 CSV（全字段，带 BOM，Excel 可直接打开对账）。
    pub(crate) fn export_accounts_csv(&mut self) {
        if self.accounts.is_empty() {
            self.status = "账号列表为空，没有可导出的内容".into();
            return;
        }
        let mut out = String::from("\u{feff}");
        out.push_str("序号,账号,密码,名字,备注,平台,机型,系统版本,设备ID,有效公里,剩余公里,已购公里,购剩,状态,今日达标\r\n");
        for (i, a) in self.accounts.iter().enumerate() {
            let valid = if a.semester_checked_ms > 0 {
                format!("{:.2}", a.semester_valid_km)
            } else {
                String::new()
            };
            let remain = if a.semester_checked_ms > 0 {
                format!("{:.2}", (a.semester_goal_km - a.semester_valid_km).max(0.0))
            } else {
                String::new()
            };
            let bought = if a.bought_km > 0.0 {
                format!("{}", a.bought_km)
            } else {
                String::new()
            };
            let bought_remain = if a.bought_km > 0.0 {
                format!("{:.2}", (a.bought_km - a.semester_valid_km).max(0.0))
            } else {
                String::new()
            };
            let fields = [
                (i + 1).to_string(),
                a.username.clone(),
                a.password.clone(),
                a.name.clone(),
                a.remark.clone(),
                if a.device_platform == "android" {
                    "安卓".to_string()
                } else if a.device_platform == "ios" {
                    "苹果".to_string()
                } else {
                    "跟随".to_string()
                },
                a.identity
                    .as_ref()
                    .map(|x| x.device_name.clone())
                    .unwrap_or_default(),
                a.identity
                    .as_ref()
                    .map(|x| x.os_version.clone())
                    .unwrap_or_default(),
                a.identity
                    .as_ref()
                    .map(|x| x.device_id.clone())
                    .unwrap_or_default(),
                valid,
                remain,
                bought,
                bought_remain,
                status_text(a),
                today_text(a).1.to_string(),
            ];
            out.push_str(
                &fields
                    .iter()
                    .map(|f| csv_escape(f))
                    .collect::<Vec<_>>()
                    .join(","),
            );
            out.push_str("\r\n");
        }
        let name = format!("账号明细-{}.csv", chrono::Local::now().format("%Y%m%d-%H%M%S"));
        let path = crate::platform::data_dir().join(&name);
        match std::fs::write(&path, out) {
            Ok(()) => self.status = format!(
                "√ 已导出 {} 个账号明细（Excel 可开）：{}",
                self.accounts.len(),
                path.display()
            ),
            Err(e) => self.status = format!("× 导出失败：{e}"),
        }
    }

    /// 备份数据目录下关键 json（accounts/config/identity）到 backup 子目录，
    /// 每个文件保留最近 30 份；内容与最新备份一致时跳过。
    pub(crate) fn backup_data(&self) {
        let dir = crate::platform::data_dir();
        let backup_dir = dir.join("backup");
        let _ = std::fs::create_dir_all(&backup_dir);
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        for name in ["accounts.json", "config.json", "identity.json"] {
            let src = dir.join(name);
            let Ok(bytes) = std::fs::read(&src) else { continue };
            // 与最新一份备份内容一致则跳过
            let mut newest: Option<(String, Vec<u8>)> = None;
            if let Ok(rd) = std::fs::read_dir(&backup_dir) {
                for e in rd.flatten() {
                    let fname = e.file_name().to_string_lossy().to_string();
                    if fname.starts_with(&format!("{name}.")) {
                        if let Ok(b) = std::fs::read(e.path()) {
                            match &newest {
                                Some((nf, _)) if fname <= *nf => {}
                                _ => newest = Some((fname, b)),
                            }
                        }
                    }
                }
            }
            if let Some((_, nb)) = &newest {
                if *nb == bytes {
                    continue;
                }
            }
            let _ = std::fs::write(backup_dir.join(format!("{name}.{stamp}")), &bytes);
        }
        // 清理：每个文件只保留最近 30 份（文件名时间戳可直接排序）
        if let Ok(rd) = std::fs::read_dir(&backup_dir) {
            let mut files: Vec<std::path::PathBuf> =
                rd.flatten().filter(|e| e.path().is_file()).map(|e| e.path()).collect();
            files.sort();
            let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
            for f in files.iter().rev() {
                let fname = f
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                let key = fname.split('.').next().unwrap_or("").to_string();
                let c = counts.entry(key).or_insert(0);
                *c += 1;
                if *c > 30 {
                    let _ = std::fs::remove_file(f);
                }
            }
        }
    }

    /// 节流备份：距上次备份不足 10 分钟跳过（内容没变的也会被 backup_data 去重）。
    pub(crate) fn backup_throttled(&self) {
        use std::sync::atomic::{AtomicI64, Ordering};
        static LAST: AtomicI64 = AtomicI64::new(0);
        let now = crate::crypto::envelope::now_ms();
        let last = LAST.load(Ordering::SeqCst);
        if now - last < 10 * 60 * 1000 {
            return;
        }
        if LAST
            .compare_exchange(last, now, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        self.backup_data();
    }

    /// 打开备份目录（桌面端资源管理器）。
    pub(crate) fn open_backup_dir(&mut self) {
        let dir = crate::platform::data_dir().join("backup");
        let _ = std::fs::create_dir_all(&dir);
        #[cfg(target_os = "android")]
        {
            let _ = dir;
            self.status = "Android 端暂不支持打开目录".into();
        }
        #[cfg(not(target_os = "android"))]
        {
            match std::process::Command::new("explorer").arg(&dir).spawn() {
                Ok(_) => self.status = format!("已打开备份目录：{}", dir.display()),
                Err(e) => self.status = format!("× 打开目录失败：{e}"),
            }
        }
    }

    // ── 登录 / 登出 ────────────────────────────────────────────

    pub(crate) fn start_login_index(&mut self, index: usize) {
        let Some(account) = self.accounts.get(index) else { return };
        if account.username.is_empty() || account.password.is_empty() {
            self.status = "该账号缺少账号或密码，请先补全".into();
            return;
        }
        let targets = vec![(index, account.username.clone(), account.password.clone())];
        self.accounts_page.last_fails.clear();
        self.run_login_batch(targets, "登录");
    }

    /// 当前筛选（搜索关键字 / 备注筛选 / 已购开关）生效时返回可见账号下标；无筛选返回 None。
    /// 批量登录/登出/刷跑都以「勾选 ∩ 筛选可见」为作用范围，防止隐藏的旧勾选被误带上。
    fn filtered_indices(&self) -> Option<Vec<usize>> {
        let search = self.accounts_page.search.trim().to_lowercase();
        let remark_filter = self.accounts_page.remark_filter.clone();
        let only_bought = self.accounts_page.only_bought;
        if search.is_empty() && remark_filter.is_none() && !only_bought {
            return None;
        }
        let out: Vec<usize> = self
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| {
                let hit_text = search.is_empty()
                    || a.username.to_lowercase().contains(&search)
                    || a.name.to_lowercase().contains(&search)
                    || a.remark.to_lowercase().contains(&search)
                    || a.session
                        .as_ref()
                        .map(|s| s.name.to_lowercase().contains(&search))
                        .unwrap_or(false);
                let hit_remark = match &remark_filter {
                    None => true,
                    Some(r) if r.is_empty() => a.remark.is_empty(),
                    Some(r) => &a.remark == r,
                };
                let hit_bought = !only_bought || a.bought_km > 0.0;
                hit_text && hit_remark && hit_bought
            })
            .map(|(i, _)| i)
            .collect();
        Some(out)
    }

    /// 生成「批量复制」文本：打码名单 + 完成状态 + 时间 + 提示语。
    fn build_accounts_copy_text(&self) -> String {
        let now = chrono::Local::now().format("%m-%d %H:%M").to_string();
        let mut lines = vec![
            "今日校园跑已完成 请上号查询是否今日运动记录有记录如果不存在请私聊发送账号".to_string(),
        ];
        for (i, a) in self.accounts.iter().enumerate() {
            let name = if !a.name.is_empty() {
                mask_name(&a.name)
            } else {
                a.session
                    .as_ref()
                    .map(|s| mask_name(&s.name))
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "未登录".to_string())
            };
            let phone = mask_phone(&a.username);
            let done = match today_text(a).1 {
                "已达标" => "已完成",
                "未达标" => "未完成",
                _ => "未查",
            };
            lines.push(format!(
                "{}. {} {} 今日校园跑:{} 时间:{}",
                i + 1,
                name,
                phone,
                done,
                now
            ));
        }
        lines.join("\n")
    }

    /// 该账号实际使用的代理：账号专属优先，其次全局配置；都为空 = 直连。
    pub(crate) fn proxy_for(&self, account: Option<&Account>) -> Option<String> {
        let own = account
            .map(|a| a.proxy.trim().to_string())
            .filter(|p| !p.is_empty());
        own.or_else(|| {
            let global = self.config.proxy.trim().to_string();
            if global.is_empty() {
                None
            } else {
                Some(global)
            }
        })
    }

    /// 该账号是否在批量操作范围内（勾选 ∩ 当前筛选可见）。
    fn in_batch_scope(&self, filtered: &Option<Vec<usize>>, index: usize) -> bool {
        match filtered {
            None => true,
            Some(v) => v.contains(&index),
        }
    }

    pub(crate) fn start_batch_login(&mut self) {
        let relogin = self.accounts_page.relogin;
        let filtered = self.filtered_indices();
        let targets: Vec<(usize, String, String)> = self
            .accounts
            .iter()
            .enumerate()
            .filter(|(i, a)| a.selected && self.in_batch_scope(&filtered, *i))
            .filter(|(_, a)| !a.username.is_empty() && !a.password.is_empty())
            .filter(|(_, a)| relogin || !a.logged_in())
            .map(|(index, a)| (index, a.username.clone(), a.password.clone()))
            .collect();
        if targets.is_empty() {
            self.status = if self.accounts.is_empty() {
                "账号列表为空，请先导入账号文件".into()
            } else if filtered.is_some()
                && !self
                    .accounts
                    .iter()
                    .enumerate()
                    .any(|(i, a)| a.selected && self.in_batch_scope(&filtered, i))
            {
                "筛选结果里没有勾选的账号（勾选后再批量登录，或点表头全选）".into()
            } else if !self.accounts.iter().any(|a| a.selected) {
                "没有勾选任何账号（勾选后再批量登录，或点表头全选）".into()
            } else {
                "勾选的账号没有需要登录的（都已在登录状态；可勾选“重新登录已登录账号”）".into()
            };
            return;
        }
        self.accounts_page.last_fails.clear();
        let label = if filtered.is_some() {
            "批量登录（筛选）"
        } else {
            "批量登录"
        };
        self.run_login_batch(targets, label);
    }

    fn run_login_batch(&mut self, targets: Vec<(usize, String, String)>, label: &str) {
        if self.accounts_busy || self.batch_run_busy {
            self.status = "已有批量任务进行中".into();
            return;
        }
        let total = targets.len();
        if total == 0 {
            return;
        }
        // GT4 验证码服务对同 IP 并发很敏感；批量登录串行，避免 5 路同时
        // 请求卡在验证码链路上。刷跑仍使用用户配置的并发数。
        let concurrency = 1usize;
        // 每账号独立设备身份 + 独立代理（防同设备 / 同 IP 关联）
        let base_identity = self.identity.clone();
        let global_proxy = self.proxy_for(None);
        let targets: Vec<(usize, String, String, HeaderIdentity, Option<String>)> = targets
            .into_iter()
            .map(|(index, username, password)| {
                let (ident, proxy) = match self.accounts.get(index) {
                    Some(a) => {
                        let px = if a.proxy.trim().is_empty() {
                            global_proxy.clone()
                        } else {
                            Some(a.proxy.trim().to_string())
                        };
                        (identity_for(&base_identity, a), px)
                    }
                    None => (base_identity.clone(), global_proxy.clone()),
                };
                (index, username, password, ident, proxy)
            })
            .collect();
        self.accounts_busy = true;
        self.login_busy = total == 1;
        self.batch_progress = Some(BatchProgress {
            label: "批量登录".into(),
            done: 0,
            total,
        });
        self.status = format!("{label}：{total} 个账号，并发 {concurrency}…");
        self.spawn_job(move |tx| {
            let done_counter = Arc::new(AtomicUsize::new(0));
            let ok_counter = Arc::new(AtomicUsize::new(0));
            let fail_counter = Arc::new(AtomicUsize::new(0));
            let (done_c, ok_c, fail_c) = (
                Arc::clone(&done_counter),
                Arc::clone(&ok_counter),
                Arc::clone(&fail_counter),
            );
            let worker_tx = tx.clone();
            batch::parallel_map(targets, concurrency, move |(index, username, password, identity, proxy), _| {
                let mut raw_log = App::logger(worker_tx.clone());
                let mut log = |m: &str| raw_log(&format!("[{username}] {m}"));
                log(&format!(
                    "[账号] {username} 登录中…（独立设备 {} / {}）",
                    identity.device_id,
                    if proxy.is_some() { "走代理" } else { "直连" }
                ));
                let payload = match batch::login_account(
                    &identity,
                    &username,
                    &password,
                    proxy.clone(),
                    &mut log,
                ) {
                    Ok(session) => {
                        log(&format!(
                            "√ [账号] {username} 登录成功 uid={} name={}",
                            session.uid, session.name
                        ));
                        ok_c.fetch_add(1, Ordering::SeqCst);
                        serde_json::json!({
                            "index": index, "username": username,
                            "ok": true, "session": session, "identity": identity,
                        })
                    }
                    Err(message) => {
                        log(&format!("× [账号] {username} 登录失败：{message}"));
                        fail_c.fetch_add(1, Ordering::SeqCst);
                        serde_json::json!({
                            "index": index, "username": username,
                            "ok": false, "message": message,
                        })
                    }
                };
                let _ = worker_tx.send(format!("{ACCT_LOGIN}{payload}"));
                let done = done_c.fetch_add(1, Ordering::SeqCst) + 1;
                let progress = serde_json::json!({"phase":"login","done":done,"total":total});
                let _ = worker_tx.send(format!("{ACCT_PROG}{progress}"));
            });
            let summary = serde_json::json!({
                "phase": "login",
                "ok": ok_counter.load(Ordering::SeqCst),
                "fail": fail_counter.load(Ordering::SeqCst),
                "total": total,
            });
            let _ = tx.send(format!("{ACCT_LOGIN_DONE}{summary}"));
        });
    }

    pub(crate) fn start_logout_index(&mut self, index: usize) {
        if self.accounts_busy || self.batch_run_busy {
            self.status = "批量任务进行中，请稍后再试".into();
            return;
        }
        let global_proxy = self.proxy_for(None);
        let Some(account) = self.accounts.get_mut(index) else { return };
        let username = account.username.clone();
        let session = account.session.take();
        let identity = identity_for(&self.identity, account);
        let proxy = if account.proxy.trim().is_empty() {
            global_proxy
        } else {
            Some(account.proxy.trim().to_string())
        };
        account.last_error.clear();
        account.last_run.clear();
        if self.active_account == Some(index) {
            self.active_account = None;
            self.session = None;
            model::clear_session();
        }
        let _ = self.persist_accounts();
        match session {
            Some(session) => {
                self.status = format!("已登出 {username}");
                self.spawn_job(move |tx| {
                    let mut raw_log = App::logger(tx.clone());
                    let mut log = |m: &str| raw_log(&format!("[{username}] {m}"));
                    match crate::api::client::ApiClient::with_proxy(identity, Some(session), proxy) {
                        Ok(mut client) => crate::api::login::logout_request(&mut client, &mut log),
                        Err(e) => log(&format!("× [账号] {username} 登出跳过：{e}")),
                    }
                });
            }
            None => self.status = format!("{username} 本来就不在登录状态"),
        }
    }

    pub(crate) fn start_batch_logout(&mut self) {
        if self.accounts_busy || self.batch_run_busy {
            self.status = "批量任务进行中，请稍后再试".into();
            return;
        }
        let filtered = self.filtered_indices();
        let is_filtered = filtered.is_some();
        let visible_set: Option<std::collections::HashSet<usize>> =
            filtered.map(|v| v.into_iter().collect());
        let in_scope = |i: usize| visible_set.as_ref().map(|s| s.contains(&i)).unwrap_or(true);
        if !self
            .accounts
            .iter()
            .enumerate()
            .any(|(i, a)| a.selected && in_scope(i))
        {
            self.status = if is_filtered {
                "筛选结果里没有勾选的账号（勾选后再批量登出，或点表头全选）".into()
            } else {
                "没有勾选任何账号（勾选后再批量登出，或点表头全选）".into()
            };
            return;
        }
        let mut sessions: Vec<(String, model::Session, HeaderIdentity, Option<String>)> = Vec::new();
        let mut selected = 0usize;
        let global_proxy = self.proxy_for(None);
        for (i, account) in self.accounts.iter_mut().enumerate() {
            if !account.selected || !in_scope(i) {
                continue;
            }
            selected += 1;
            if let Some(session) = account.session.take() {
                let ident = identity_for(&self.identity, account);
                let proxy = if account.proxy.trim().is_empty() {
                    global_proxy.clone()
                } else {
                    Some(account.proxy.trim().to_string())
                };
                sessions.push((account.username.clone(), session, ident, proxy));
            }
            account.last_error.clear();
            account.last_run.clear();
        }
        let _ = self.persist_accounts();
        if sessions.is_empty() {
            self.status = format!("勾选的 {selected} 个账号都没有登录会话，无需登出");
            return;
        }
        self.active_account = None;
        self.session = None;
        model::clear_session();
        let count = sessions.len();
        let concurrency = self.accounts_page.concurrency.clamp(1, 32);
        self.spawn_job(move |tx| {
            batch::parallel_map(sessions, concurrency, move |(username, session, identity, proxy), _| {
                let mut raw_log = App::logger(tx.clone());
                    let mut log = |m: &str| raw_log(&format!("[{username}] {m}"));
                match crate::api::client::ApiClient::with_proxy(identity, Some(session), proxy) {
                    Ok(mut client) => crate::api::login::logout(&mut client, &mut log),
                    Err(e) => log(&format!("× [账号] {username} 登出跳过：{e}")),
                }
                log(&format!("√ [账号] {username} 已登出"));
            });
        });
        self.status = format!("已登出 {count} 个勾选账号（本地会话已清空）");
    }

    // ── 批量刷跑 ───────────────────────────────────────────────

    pub(crate) fn start_batch_run(&mut self) {
        self.start_batch_run_with(None);
    }

    /// 批量刷跑。subset 为 Some 时只跑指定下标（定时分散任务使用，
    /// 不受勾选/筛选影响）；None 时按「勾选 ∩ 筛选可见」。
    pub(crate) fn start_batch_run_with(&mut self, subset: Option<Vec<usize>>) {
        if self.accounts_busy || self.batch_run_busy {
            self.status = "已有批量任务进行中".into();
            return;
        }
        if self.logged_account_count() == 0 {
            self.status = "没有已登录账号，请先在账号页完成登录".into();
            return;
        }
        let filtered = self.filtered_indices();
        self.run_page.ensure_plan();
        let altitude_spec = match crate::track::altitude::parse_spec(&self.run_page.manual_altitude) {
            Ok(spec) => spec,
            Err(e) => {
                self.status = e;
                return;
            }
        };
        let (manual_altitude, manual_altitude_range) = match altitude_spec {
            None => (None, None),
            Some(crate::track::altitude::AltitudeSpec::Single(value)) => (Some(value), None),
            Some(crate::track::altitude::AltitudeSpec::Range(range)) => (None, Some(range)),
        };
        let face_check = if self.run_page.face_check { 1 } else { 0 };

        // 逐个账号现抽一份运动量（同一个参数区间，但每个账号各跑各的）；
        // 设备身份与代理同样逐账号独立（与登录时同一台「专属设备」/同一出口）
        let mut targets: Vec<(
            usize,
            String,
            model::Session,
            HeaderIdentity,
            Option<String>,
            crate::api::flow::RunParams,
        )> = Vec::new();
        let base_seed = (crate::crypto::envelope::now_ms() as u64) % 2_147_483_647;
        let global_proxy = self.proxy_for(None);
        let candidate_indices: Vec<usize> = match &subset {
            Some(list) => list.clone(),
            None => (0..self.accounts.len())
                .filter(|i| {
                    self.accounts
                        .get(*i)
                        .map(|a| a.selected && self.in_batch_scope(&filtered, *i))
                        .unwrap_or(false)
                })
                .collect(),
        };
        {
            let page = &self.run_page;
            let base_identity = &self.identity;
            for index in candidate_indices {
                let Some(account) = self.accounts.get(index) else { continue };
                let Some(session) = account.session.clone() else { continue };
                if !session.is_logged_in() {
                    continue;
                }
                let proxy = if account.proxy.trim().is_empty() {
                    global_proxy.clone()
                } else {
                    Some(account.proxy.trim().to_string())
                };
                let plan = page.sample_plan_batched();
                targets.push((
                    index,
                    account.username.clone(),
                    session,
                    identity_for(base_identity, account),
                    proxy,
                    crate::api::flow::RunParams {
                        dist: plan.dist * 1000.0,
                        dur: plan.dur,
                        start_ms: plan.start_ms,
                        face_check,
                        manual_altitude,
                        manual_altitude_range,
                        seed: base_seed
                            .wrapping_mul(31)
                            .wrapping_add(index as u64)
                            .wrapping_add(1),
                        route_engine: match page.route_engine {
                            3 => crate::api::flow::RouteEngine::V4,
                            2 => crate::api::flow::RouteEngine::V3,
                            1 => crate::api::flow::RouteEngine::V2,
                            _ => crate::api::flow::RouteEngine::V1,
                        },
                    },
                ));
            }
        }
        if targets.is_empty() {
            self.status = if subset.is_some() {
                "定时任务：没有可跑账号（未登录或已跳过）".into()
            } else if filtered.is_some() {
                "筛选结果里没有勾选且已登录的账号".into()
            } else {
                "没有勾选且已登录的账号（勾选后再批量刷跑）".into()
            };
            return;
        }
        let total = targets.len();
        let concurrency = self.accounts_page.concurrency.clamp(1, 32);
        self.accounts_page.last_fails.clear();
        self.batch_run_busy = true;
        self.batch_progress = Some(BatchProgress {
            label: "批量刷跑".into(),
            done: 0,
            total,
        });
        self.status = format!("批量刷跑：{total} 个勾选账号，并发 {concurrency}…");
        self.spawn_job(move |tx| {
            let done_counter = Arc::new(AtomicUsize::new(0));
            let ok_counter = Arc::new(AtomicUsize::new(0));
            let fail_counter = Arc::new(AtomicUsize::new(0));
            let (done_c, ok_c, fail_c) = (
                Arc::clone(&done_counter),
                Arc::clone(&ok_counter),
                Arc::clone(&fail_counter),
            );
            let worker_tx = tx.clone();
            batch::parallel_map(targets, concurrency, move |(index, username, session, identity, proxy, params), _| {
                let mut raw_log = App::logger(worker_tx.clone());
                let mut log = |m: &str| raw_log(&format!("[{username}] {m}"));
                // 开跑前先打详细计划：距离 / 用时 / 起点 / 出口
                let start_txt = chrono::Local
                    .timestamp_millis_opt(params.start_ms)
                    .single()
                    .map(|t| t.format("%m-%d %H:%M:%S").to_string())
                    .unwrap_or_default();
                log(&format!(
                    "开始刷跑：{:.0} 米 / {} 秒 / 起点 {} / {}",
                    params.dist,
                    params.dur,
                    start_txt,
                    if proxy.is_some() { "走代理" } else { "直连" }
                ));
                let payload = match batch::run_account(
                    &identity,
                    session,
                    &params,
                    proxy,
                    &mut log,
                ) {
                    Ok(outcome) => {
                        log(&format!(
                            "√ [账号] {username} 刷跑成功 rrid={} obs={}/2 验证={}",
                            outcome.result.rrid, outcome.obs_ok, outcome.detail_ok
                        ));
                        ok_c.fetch_add(1, Ordering::SeqCst);
                        serde_json::json!({
                            "index": index, "username": username, "ok": true,
                            "rrid": outcome.result.rrid,
                            "obs_ok": outcome.obs_ok,
                            "verify": outcome.detail_ok,
                            "dist": outcome.result.total_dis,
                            "dur": outcome.result.total_time,
                        })
                    }
                    Err(message) => {
                        log(&format!("× [账号] {username} 刷跑失败：{message}"));
                        fail_c.fetch_add(1, Ordering::SeqCst);
                        serde_json::json!({
                            "index": index, "username": username,
                            "ok": false, "message": message,
                        })
                    }
                };
                let _ = worker_tx.send(format!("{ACCT_RUN}{payload}"));
                let done = done_c.fetch_add(1, Ordering::SeqCst) + 1;
                let progress = serde_json::json!({"phase":"run","done":done,"total":total});
                let _ = worker_tx.send(format!("{ACCT_PROG}{progress}"));
            });
            let summary = serde_json::json!({
                "phase": "run",
                "ok": ok_counter.load(Ordering::SeqCst),
                "fail": fail_counter.load(Ordering::SeqCst),
                "total": total,
            });
            let _ = tx.send(format!("{ACCT_RUN_DONE}{summary}"));
        });
    }

    // ── 结果归拢（UI 线程）─────────────────────────────────────

    fn resolve_account_index(&self, val: &serde_json::Value, username: &str) -> Option<usize> {
        if let Some(index) = val["index"].as_u64() {
            let index = index as usize;
            if self
                .accounts
                .get(index)
                .map(|a| a.username == username)
                .unwrap_or(false)
            {
                return Some(index);
            }
        }
        self.accounts.iter().position(|a| a.username == username)
    }

    pub(crate) fn apply_account_login(&mut self, val: serde_json::Value) {
        let username = val["username"].as_str().unwrap_or("").to_string();
        let Some(index) = self.resolve_account_index(&val, &username) else {
            return;
        };
        if val["ok"].as_bool().unwrap_or(false) {
            match serde_json::from_value::<model::Session>(val["session"].clone()) {
                Ok(session) => {
                    let uid = session.uid;
                    let name = session.name.clone();
                    // 登录实际使用的设备身份（每账号独立设备）随账号持久化
                    let identity =
                        serde_json::from_value::<HeaderIdentity>(val["identity"].clone()).ok();
                    if let Some(account) = self.accounts.get_mut(index) {
                        if !name.is_empty() {
                            account.name = name.clone();
                        }
                        account.session = Some(session);
                        if let Some(identity) = identity {
                            account.identity = Some(identity);
                        }
                        account.last_error.clear();
                        account.last_login_ms = crate::crypto::envelope::now_ms();
                    }
                    // 还没有当前账号时自动选中第一个登录成功的账号
                    if self.session.is_none() {
                        self.set_active_account(index);
                        self.refresh_data_page();
                        self.refresh_records();
                        self.refresh_user_page();
                        self.refresh_ai_list();
                    }
                    if let Some(account) = self.accounts.get_mut(index) {
                        // 登录成功说明标记已解除
                        account.flagged_reason.clear();
                        account.flagged_until_ms = 0;
                    }
                    self.status = if name.is_empty() {
                        format!("√ {username} 登录成功（uid={uid}）")
                    } else {
                        format!("√ {username} 登录成功：{name}（uid={uid}）")
                    };
                }
                Err(e) => {
                    if let Some(account) = self.accounts.get_mut(index) {
                        account.session = None;
                        account.last_error = format!("登录响应解析失败：{e}");
                    }
                    self.status = format!("× {username} 登录响应解析失败");
                }
            }
        } else {
            let message = val["message"].as_str().unwrap_or("未知错误").to_string();
            let flagged = detect_flag(&message);
            if let Some(account) = self.accounts.get_mut(index) {
                account.session = None;
                account.last_error = message.clone();
                // 命中封禁/风控关键词 → 标红 + 记解冻时间，账号行会显示倒计时
                if let Some((reason, until)) = flagged.clone() {
                    account.flagged_reason = reason;
                    account.flagged_until_ms = until;
                }
            }
            self.accounts_page.last_fails.push(FailItem {
                username: username.clone(),
                password: self
                    .accounts
                    .get(index)
                    .map(|a| a.password.clone())
                    .unwrap_or_default(),
                message: message.clone(),
            });
            self.status = format!("× {username} 登录失败：{message}");
        }
        let _ = self.persist_accounts();
    }

    pub(crate) fn apply_account_run(&mut self, val: serde_json::Value) {
        let username = val["username"].as_str().unwrap_or("").to_string();
        let Some(index) = self.resolve_account_index(&val, &username) else {
            return;
        };
        if val["ok"].as_bool().unwrap_or(false) {
            let dist_m = val["dist"].as_f64().unwrap_or(0.0);
            let dur_s = val["dur"].as_i64().unwrap_or(0);
            let mut summary = format!(
                "刷跑成功 {:.2}km · {} · rrid={} · OBS {}/2 · 验证{}",
                dist_m / 1000.0,
                fmt_duration(dur_s),
                val["rrid"].as_i64().unwrap_or(0),
                val["obs_ok"].as_i64().unwrap_or(0),
                if val["verify"].as_bool().unwrap_or(false) {
                    "通过"
                } else {
                    "未通过"
                }
            );
            if let Some(acct) = self.accounts.get(index) {
                if !acct.remark.is_empty() {
                    summary = format!("[{}] {summary}", acct.remark);
                }
            }
            // 注意：已购公里不在这里扣。购剩 = 已购 - 有效公里，
            // 跑完后「刷新达标与公里」拉取新的有效公里，购剩自然减少；
            // 再手动 -2 等于重复扣账。
            if let Some(account) = self.accounts.get_mut(index) {
                account.last_run = summary.clone();
            }
            self.status = format!("√ {username} {summary}");
        } else {
            let message = val["message"].as_str().unwrap_or("未知错误").to_string();
            if let Some(account) = self.accounts.get_mut(index) {
                account.last_run = format!("刷跑失败：{message}");
            }
            self.accounts_page.last_fails.push(FailItem {
                username: username.clone(),
                password: self
                    .accounts
                    .get(index)
                    .map(|a| a.password.clone())
                    .unwrap_or_default(),
                message: format!("刷跑失败：{message}"),
            });
            self.status = format!("× {username} 刷跑失败：{message}");
        }
        let _ = self.persist_accounts();
    }

    pub(crate) fn finish_batch_login(&mut self, val: &serde_json::Value) {
        self.accounts_busy = false;
        self.login_busy = false;
        self.batch_progress = None;
        let ok = val["ok"].as_u64().unwrap_or(0);
        let fail = val["fail"].as_u64().unwrap_or(0);
        let total = val["total"].as_u64().unwrap_or(0);
        let _ = self.persist_accounts();
        if self.ensure_active_account() {
            self.refresh_data_page();
            self.refresh_records();
            self.refresh_user_page();
            self.refresh_ai_list();
        }
        let fails = self.accounts_page.last_fails.clone();
        let single = total == 1;
        let mut lines = vec![
            format!("账号总数：{total}"),
            format!("登录成功：{ok}"),
            format!("登录失败：{fail}"),
        ];
        if fails.is_empty() {
            lines.push("全部账号登录成功".into());
        } else {
            lines.push(String::new());
            lines.push("失败账号（账号----密码 · 原因）：".into());
            for item in &fails {
                lines.push(format!("{}----{} · {}", item.username, item.password, item.message));
            }
            if !single {
                lines.push(String::new());
                lines.push("可在账号页「导出失败账号」保存到文件。".into());
            }
        }
        self.popup = Some(PopupInfo {
            title: if single {
                format!("登录结果（成功 {ok} / 失败 {fail}）")
            } else {
                format!("批量登录完成（成功 {ok} / 失败 {fail}）")
            },
            lines,
        });
        self.status = format!("批量登录完成：成功 {ok}，失败 {fail}");
        // 登录完自动刷新各账号当日达标状态
        self.start_today_check();
    }

    pub(crate) fn finish_batch_run(&mut self, val: &serde_json::Value) {
        self.batch_run_busy = false;
        self.batch_progress = None;
        let ok = val["ok"].as_u64().unwrap_or(0);
        let fail = val["fail"].as_u64().unwrap_or(0);
        let total = val["total"].as_u64().unwrap_or(0);
        let _ = self.persist_accounts();
        if ok > 0 && self.session.is_some() {
            self.refresh_data_page();
        }
        let fails: Vec<FailItem> = self
            .accounts_page
            .last_fails
            .iter()
            .filter(|f| f.message.starts_with("刷跑失败"))
            .cloned()
            .collect();
        let mut lines = vec![
            format!("参与账号：{total}"),
            format!("刷跑成功：{ok}"),
            format!("刷跑失败：{fail}"),
        ];
        if fails.is_empty() {
            lines.push("全部账号刷跑成功".into());
        } else {
            lines.push(String::new());
            lines.push("失败账号（账号----密码 · 原因）：".into());
            for item in &fails {
                lines.push(format!("{}----{} · {}", item.username, item.password, item.message));
            }
        }
        self.popup = Some(PopupInfo {
            title: if total == 1 {
                format!("刷跑结果（成功 {ok} / 失败 {fail}）")
            } else {
                format!("批量刷跑完成（成功 {ok} / 失败 {fail}）")
            },
            lines,
        });
        self.status = format!("批量刷跑完成：成功 {ok}，失败 {fail}");
        // 跑完自动刷新各账号当日达标状态
        self.start_today_check();
    }

    // ── 达标与公里检查 ─────────────────────────────────────────

    /// 批量查询各已登录账号的跑步记录与学期里程，判断「今天是否有达标记录」
    /// 并刷新「公里」列。手动点「刷新达标与公里」触发；批量登录/刷跑完成后自动触发。
    pub(crate) fn start_today_check(&mut self) {
        if self.accounts_busy || self.batch_run_busy {
            self.status = "批量任务进行中，请稍后再试".into();
            return;
        }
        let base_identity = self.identity.clone();
        let global_proxy = self.proxy_for(None);
        let targets: Vec<(usize, String, model::Session, HeaderIdentity, Option<String>)> = self
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| a.session.as_ref().is_some_and(|s| s.is_logged_in()))
            .map(|(index, a)| {
                let proxy = if a.proxy.trim().is_empty() {
                    global_proxy.clone()
                } else {
                    Some(a.proxy.trim().to_string())
                };
                (
                    index,
                    a.username.clone(),
                    a.session.clone().unwrap_or_default(),
                    identity_for(&base_identity, a),
                    proxy,
                )
            })
            .collect();
        if targets.is_empty() {
            self.status = "没有已登录账号，无法检查达标状态".into();
            return;
        }
        let total = targets.len();
        let concurrency = self.accounts_page.concurrency.clamp(1, 32);
        self.accounts_busy = true;
        self.batch_progress = Some(BatchProgress {
            label: "达标与公里".into(),
            done: 0,
            total,
        });
        self.status = format!("刷新达标与公里：{total} 个账号…");
        self.spawn_job(move |tx| {
            use chrono::TimeZone;
            let today = chrono::Local::now().date_naive();
            let done_counter = Arc::new(AtomicUsize::new(0));
            let worker_tx = tx.clone();
            batch::parallel_map(targets, concurrency, move |(index, username, session, identity, proxy), _| {
                let mut raw_log = App::logger(worker_tx.clone());
                let mut log = |m: &str| raw_log(&format!("[{username}] {m}"));
                let mut client = match crate::api::client::ApiClient::with_proxy(
                    identity,
                    Some(session),
                    proxy,
                ) {
                    Ok(c) => c,
                    Err(e) => {
                        log(&format!("× [账号] {username} 代理无效：{e}"));
                        let payload = serde_json::json!({
                            "index": index, "username": username,
                            "ok": serde_json::Value::Null,
                        });
                        let _ = worker_tx.send(format!("{ACCT_TODAY}{payload}"));
                        let done = done_counter.fetch_add(1, Ordering::SeqCst) + 1;
                        let progress = serde_json::json!({"phase":"today","done":done,"total":total});
                        let _ = worker_tx.send(format!("{ACCT_PROG}{progress}"));
                        return;
                    }
                };
                // 查询失败不影响既有展示：ok 传 null，UI 侧跳过更新
                let rows = crate::api::records::fetch_records(&mut client);
                let ok = match &rows {
                    Ok(rows) => serde_json::Value::Bool(rows.iter().any(|r| {
                        r.complete
                            && chrono::Local
                                .timestamp_millis_opt(r.start_time)
                                .single()
                                .map(|t| t.date_naive())
                                == Some(today)
                    })),
                    Err(_) => serde_json::Value::Null,
                };
                // 学期公里（有效/目标），失败不阻塞达标展示；完成状态接口需要最近 rrid
                let latest_rrid = rows
                    .as_ref()
                    .ok()
                    .and_then(|r| r.first().map(|x| x.rrid));
                let (valid_km, goal_km) =
                    crate::api::semester::query_km(&mut client, &mut log, latest_rrid);
                let payload = serde_json::json!({
                    "index": index, "username": username, "ok": ok,
                    "valid_km": valid_km, "goal_km": goal_km,
                });
                let _ = worker_tx.send(format!("{ACCT_TODAY}{payload}"));
                let done = done_counter.fetch_add(1, Ordering::SeqCst) + 1;
                let progress = serde_json::json!({"phase":"today","done":done,"total":total});
                let _ = worker_tx.send(format!("{ACCT_PROG}{progress}"));
            });
            let _ = tx.send(ACCT_TODAY_DONE.to_string());
        });
    }

    pub(crate) fn apply_today_status(&mut self, val: serde_json::Value) {
        let username = val["username"].as_str().unwrap_or("").to_string();
        let Some(index) = self.resolve_account_index(&val, &username) else {
            return;
        };
        if let Some(account) = self.accounts.get_mut(index) {
            if let Some(ok) = val["ok"].as_bool() {
                account.today_ok = Some(ok);
                account.today_checked_ms = crate::crypto::envelope::now_ms();
            }
            if let Some(v) = val["valid_km"].as_f64() {
                account.semester_valid_km = v;
                account.semester_checked_ms = crate::crypto::envelope::now_ms();
            }
            if let Some(g) = val["goal_km"].as_f64() {
                // 48 公里规则：超出 48 按 48；原始值 > 64000 米（64 公里）标异常
                if g > 64.0 {
                    account.goal_abnormal = true;
                    account.semester_goal_km = 48.0;
                } else if g > 0.0 {
                    account.goal_abnormal = false;
                    account.semester_goal_km = g.min(48.0);
                } else {
                    account.goal_abnormal = false;
                    account.semester_goal_km = 48.0;
                }
                account.semester_checked_ms = crate::crypto::envelope::now_ms();
            }
        }
    }

    pub(crate) fn finish_today_check(&mut self) {
        self.accounts_busy = false;
        self.batch_progress = None;
        let _ = self.persist_accounts();
        let done = self
            .accounts
            .iter()
            .filter(|a| a.today_ok == Some(true))
            .count();
        let failed = self
            .accounts
            .iter()
            .filter(|a| a.today_ok == Some(false))
            .count();
        let abnormal = self.accounts.iter().filter(|a| a.goal_abnormal).count();
        if abnormal > 0 {
            self.status = format!(
                "达标检查完成：已达标 {done}，未达标 {failed}；目标异常 {abnormal} 个（已按 48 公里处理）"
            );
        } else {
            self.status = format!("达标检查完成：已达标 {done}，未达标 {failed}");
        }
    }

    // ── 定时分散刷跑 ───────────────────────────────────────────

    /// 定时窗口内按随机间隔分批跑，降低「集中批量提交」特征。
    /// 每帧调用，内部判断开关/窗口/间隔/当日去重。
    pub(crate) fn tick_auto_run(&mut self) {
        if !self.config.auto_run
            || self.accounts_busy
            || self.batch_run_busy
            || self.accounts.is_empty()
        {
            return;
        }
        use chrono::Timelike;
        let now = chrono::Local::now();
        let now_ms = crate::crypto::envelope::now_ms();
        let hour = now.hour() as i64;
        let start_h = self.config.auto_start_hour.clamp(0, 23);
        let end_h = self.config.auto_end_hour.clamp(1, 24);
        if hour < start_h || hour >= end_h {
            return;
        }
        if self.config.auto_next_ms > 0 && now_ms < self.config.auto_next_ms {
            return;
        }
        let today = now.format("%Y-%m-%d").to_string();
        let pending: Vec<usize> = self
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| a.selected && a.logged_in() && a.auto_done_date != today)
            .map(|(i, _)| i)
            .collect();
        if pending.is_empty() {
            // 今天都跑完了 → 明天窗口；一个可跑账号都没有 → 10 分钟后再看
            let any_candidate = self.accounts.iter().any(|a| a.selected && a.logged_in());
            self.config.auto_next_ms = if any_candidate {
                self.next_window_start_ms()
            } else {
                now_ms + 10 * 60_000
            };
            let _ = model::save_config(&self.config);
            return;
        }
        let batch = self.config.auto_batch.clamp(1, 10) as usize;
        let picked: Vec<usize> = pending.into_iter().take(batch).collect();
        for &i in &picked {
            if let Some(a) = self.accounts.get_mut(i) {
                a.auto_done_date = today.clone();
            }
        }
        let _ = self.persist_accounts();
        let lo = self.config.auto_gap_min.clamp(1, 240);
        let hi = self.config.auto_gap_max.max(lo).clamp(lo, 480);
        let span = (hi - lo + 1) as u64;
        let gap_min = lo + (rand::random::<u64>() % span) as i64;
        self.config.auto_next_ms = now_ms + gap_min * 60_000;
        let _ = model::save_config(&self.config);
        self.status = format!(
            "定时刷跑：开始 {} 个账号，下次约 {} 分钟后",
            picked.len(),
            gap_min
        );
        self.start_batch_run_with(Some(picked));
    }

    /// 下一个定时窗口开始时间（毫秒）。
    fn next_window_start_ms(&self) -> i64 {
        use chrono::TimeZone;
        let now = chrono::Local::now();
        let start_h = self.config.auto_start_hour.clamp(0, 23) as u32;
        let as_ms = |date: chrono::NaiveDate| -> i64 {
            date.and_hms_opt(start_h, 0, 0)
                .and_then(|n| chrono::Local.from_local_datetime(&n).single())
                .map(|d| d.timestamp_millis())
                .unwrap_or(0)
        };
        let today = now.date_naive();
        let today_ms = as_ms(today);
        if now.timestamp_millis() < today_ms {
            today_ms
        } else {
            as_ms(today + chrono::Days::new(1))
        }
    }

    // ── 违规自查（全部账号）────────────────────────────────────

    pub(crate) fn start_cheat_check(&mut self) {
        if self.accounts_busy || self.batch_run_busy {
            self.status = "批量任务进行中，请稍后再试".into();
            return;
        }
        let base_identity = self.identity.clone();
        let global_proxy = self.proxy_for(None);
        let targets: Vec<(usize, String, model::Session, HeaderIdentity, Option<String>)> = self
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| a.session.as_ref().is_some_and(|s| s.is_logged_in()))
            .map(|(index, a)| {
                let proxy = if a.proxy.trim().is_empty() {
                    global_proxy.clone()
                } else {
                    Some(a.proxy.trim().to_string())
                };
                (
                    index,
                    a.username.clone(),
                    a.session.clone().unwrap_or_default(),
                    identity_for(&base_identity, a),
                    proxy,
                )
            })
            .collect();
        if targets.is_empty() {
            self.status = "没有已登录账号，无法自查".into();
            return;
        }
        let total = targets.len();
        let concurrency = self.accounts_page.concurrency.clamp(1, 32);
        self.accounts_busy = true;
        self.batch_progress = Some(BatchProgress {
            label: "违规自查".into(),
            done: 0,
            total,
        });
        self.status = format!("违规自查：{total} 个账号…");
        self.spawn_job(move |tx| {
            let done_counter = Arc::new(AtomicUsize::new(0));
            let worker_tx = tx.clone();
            batch::parallel_map(targets, concurrency, move |(index, username, session, identity, proxy), _| {
                let mut raw_log = App::logger(worker_tx.clone());
                let mut log = |m: &str| raw_log(&format!("[{username}] {m}"));
                let result = match crate::api::client::ApiClient::with_proxy(
                    identity,
                    Some(session),
                    proxy,
                ) {
                    Ok(mut client) => match crate::api::cheat::query(&mut client, 1, &mut log) {
                        Ok(report) => serde_json::json!({
                            "clean": report.is_clean(),
                            "reason": report.self_brief(),
                        }),
                        Err(e) => {
                            log(&format!("× [自查] {username} 失败：{e}"));
                            serde_json::Value::Null
                        }
                    },
                    Err(e) => {
                        log(&format!("× [自查] {username} 代理无效：{e}"));
                        serde_json::Value::Null
                    }
                };
                let payload = serde_json::json!({
                    "index": index, "username": username, "result": result,
                });
                let _ = worker_tx.send(format!("{ACCT_CHEAT}{payload}"));
                let done = done_counter.fetch_add(1, Ordering::SeqCst) + 1;
                let progress = serde_json::json!({"phase":"cheat","done":done,"total":total});
                let _ = worker_tx.send(format!("{ACCT_PROG}{progress}"));
            });
            let _ = tx.send(ACCT_CHEAT_DONE.to_string());
        });
    }

    pub(crate) fn apply_cheat_result(&mut self, val: serde_json::Value) {
        let username = val["username"].as_str().unwrap_or("").to_string();
        let Some(index) = self.resolve_account_index(&val, &username) else {
            return;
        };
        let result = &val["result"];
        if result.is_null() {
            return;
        }
        if let Some(account) = self.accounts.get_mut(index) {
            account.cheat_flag = result["clean"].as_bool();
            account.cheat_reason = result["reason"].as_str().unwrap_or("").to_string();
            account.cheat_ms = crate::crypto::envelope::now_ms();
        }
    }

    pub(crate) fn finish_cheat_check(&mut self) {
        self.accounts_busy = false;
        self.batch_progress = None;
        let _ = self.persist_accounts();
        let flagged: Vec<String> = self
            .accounts
            .iter()
            .filter(|a| a.cheat_flag == Some(false))
            .map(|a| format!("{}：{}", a.username, a.cheat_reason))
            .collect();
        let checked = self.accounts.iter().filter(|a| a.cheat_ms > 0).count();
        let mut lines = vec![format!("已自查：{checked} 个账号")];
        if flagged.is_empty() {
            lines.push("全部干净（未在违规名单中）".into());
        } else {
            lines.push(format!("⚠ 已被标记：{} 个", flagged.len()));
            lines.extend(flagged);
        }
        self.popup = Some(PopupInfo {
            title: "违规自查结果".into(),
            lines,
        });
        self.status = format!(
            "违规自查完成：已查 {checked}，标记 {} 个",
            self.accounts.iter().filter(|a| a.cheat_flag == Some(false)).count()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归：设备页保存的锚点/城市/平台必须实时生效，不被登录时的账号身份快照锁死；
    /// 设备指纹（device_id 等）保持每账号专属不变。
    #[test]
    fn identity_for_geo_follows_base_but_fingerprint_is_per_account() {
        let mut base = HeaderIdentity {
            city: "旧城市".into(),
            anchor_lat: 30.0,
            anchor_lon: 120.0,
            ..Default::default()
        };
        let mut account = Account {
            username: "13800000001".into(),
            ..Default::default()
        };
        // 模拟登录时持久化的旧坐标快照
        let mut stored = crate::crypto::header::derive_per_account(&base, &account.username);
        stored.city = "旧城市".into();
        stored.anchor_lat = 30.0;
        stored.anchor_lon = 120.0;
        account.identity = Some(stored.clone());

        // 设备页改成新坐标并保存（base 更新）
        base.city = "新城市".into();
        base.anchor_lat = 39.9;
        base.anchor_lon = 116.4;
        base.os_version = "28.0".into();

        let merged = identity_for(&base, &account);
        // 地理/平台字段实时跟随 base
        assert_eq!(merged.city, "新城市");
        assert_eq!(merged.anchor_lat, 39.9);
        assert_eq!(merged.anchor_lon, 116.4);
        assert_eq!(merged.os_version, "28.0");
        // 设备指纹保持账号专属
        assert_eq!(merged.device_id, stored.device_id);
        assert_eq!(merged.mac_address, stored.mac_address);

        // 未持久化身份的账号：派生指纹 + 同样跟随 base 地理
        let fresh = identity_for(
            &base,
            &Account { username: "13800000002".into(), ..Default::default() },
        );
        assert_eq!(fresh.city, "新城市");
        assert_eq!(fresh.anchor_lat, 39.9);
        assert_ne!(fresh.device_id, merged.device_id);
    }
}
