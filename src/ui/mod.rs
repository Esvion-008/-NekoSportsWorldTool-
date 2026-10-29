//! UI 层：App 状态机 + 顶栏 + 标签页 + 底部状态栏。
//!
//! 后台任务与消息处理在独立 impl App 块中。
//!
//! 线程模型：所有网络/长任务 spawn thread + mpsc channel<String> 回 UI，
//! 带 200ms 轮询重绘。带内消息协议：
//! __IP_DONE__ / __RUN_DONE__ / __AI_DONE__ / __RECORDS__ / __AI_LIST__
//! __SEMESTER__ / __CHEAT__ / __RANK__ / __USER__ / __AI_RECORDS__
//! __ACCT_LOGIN__ / __ACCT_LOGIN_DONE__ / __ACCT_RUN__ / __ACCT_RUN_DONE__ / __ACCT_PROG__

pub mod accounts;
pub mod ai;
pub mod campus;
pub mod data;
pub mod device;
pub mod fonts;
pub mod jobs;
pub mod msgs;
pub mod log;
pub mod mobile;
pub mod records;
pub mod run;
pub mod user;
pub mod theme;

use crate::api::model::{self, Config, HeaderIdentity, Session};
use eframe::egui;
use std::sync::mpsc::{Receiver, Sender};

pub const IP: &str = "__IP_DONE__";
pub const RUN_DONE: &str = "__RUN_DONE__";
pub const AI_DONE: &str = "__AI_DONE__";
pub const RECORDS: &str = "__RECORDS__";
pub const AI_LIST: &str = "__AI_LIST__";
pub const AI_DETAIL: &str = "__AI_DETAIL__";
pub const SEMESTER: &str = "__SEMESTER__";
pub const CHEAT: &str = "__CHEAT__";
pub const RANK: &str = "__RANK__";
pub const USER: &str = "__USER__";
pub const AI_RECORDS: &str = "__AI_RECORDS__";
pub const RUN_DETAIL: &str = "__RUN_DETAIL__";
/// 单个账号登录结果（批量登录 / 单个登录共用）。
pub const ACCT_LOGIN: &str = "__ACCT_LOGIN__";
/// 批量登录收尾（ok/fail 计数）。
pub const ACCT_LOGIN_DONE: &str = "__ACCT_LOGIN_DONE__";
/// 单个账号刷跑结果。
pub const ACCT_RUN: &str = "__ACCT_RUN__";
/// 批量刷跑收尾。
pub const ACCT_RUN_DONE: &str = "__ACCT_RUN_DONE__";
/// 批量任务进度（done/total）。
pub const ACCT_PROG: &str = "__ACCT_PROG__";
/// 单个账号当日达标检查结果。
pub const ACCT_TODAY: &str = "__ACCT_TODAY__";
/// 当日达标检查收尾。
pub const ACCT_TODAY_DONE: &str = "__ACCT_TODAY_DONE__";
/// 单个账号违规自查结果。
pub const ACCT_CHEAT: &str = "__ACCT_CHEAT__";
/// 违规自查收尾。
pub const ACCT_CHEAT_DONE: &str = "__ACCT_CHEAT_DONE__";
/// 两天计划单个账号进度。
pub const TWO_DAY_PROG: &str = "__TWO_DAY_PROG__";
/// 两天计划收尾。
pub const TWO_DAY_DONE: &str = "__TWO_DAY_DONE__";

/// 版本标识（本版不接自动更新，桌面端手动替换 exe 升级）。
pub const VERSION_TEXT: &str = "格陵兰岛 v1.0";

/// 标签页下标（mobile::TABS 与之对应）。
pub const TAB_ACCOUNTS: usize = 0;
pub const TAB_RUN: usize = 1;
pub const TAB_RECORDS: usize = 3;
pub const TAB_DEVICE: usize = 6;
pub const TAB_CAMPUS: usize = 7;

/// 提交结果弹窗。
pub struct PopupInfo {
    pub title: String,
    pub lines: Vec<String>,
}

/// 顶层 App。
pub struct App {
    pub tx: Sender<String>,
    pub rx: Receiver<String>,
    pub log: log::LogStore,
    pub font_loaded: Option<String>,
    /// 应用内 logo（定制图标，顶栏左侧展示）。
    pub logo: Option<egui::TextureHandle>,

    pub ip: String,
    pub identity: HeaderIdentity,
    pub device_buf: HeaderIdentity,
    pub session: Option<Session>,
    pub config: Config,

    /// 批量账号列表（accounts.json）。
    pub accounts: Vec<model::Account>,
    /// 当前账号在 accounts 中的下标；其会话镜像到 `session` 供其余页面使用。
    pub active_account: Option<usize>,
    /// 运动记录页正在查看的账号下标（None = 跟随当前账号）。
    pub records_view: Option<usize>,
    /// 批量登录进行中。
    pub accounts_busy: bool,
    /// 批量刷跑进行中。
    pub batch_run_busy: bool,
    /// 批量任务进度。
    pub batch_progress: Option<accounts::BatchProgress>,
    /// V2 两天批量计划（阶段 6）。
    pub two_day_plan: Option<run::TwoDayPlan>,
    pub two_day_busy: bool,
    pub two_day_pause: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub two_day_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,

    pub login_busy: bool,
    pub run_busy: bool,
    pub ai_busy: bool,
    pub records_busy: bool,
    pub data_busy: bool,
    pub user_busy: bool,
    pub status: String,
    pub popup: Option<PopupInfo>,
    pub ai_confirm: Option<ai::AiBatchPlan>,

    pub tab: usize,
    pub run_page: run::RunPage,
    pub campus_page: campus::CampusPage,
    pub ai_page: ai::AiPage,
    pub records_page: records::RecordsPage,
    pub data_page: data::DataPage,
    pub user_page: user::UserPage,
    pub device_page: device::DevicePage,
    pub accounts_page: accounts::AccountsPage,
}

impl eframe::App for App {
    #[cfg(target_os = "android")]
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, input: &mut egui::RawInput) {
        crate::platform::apply_safe_area(ctx, input);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_messages();
        // 定时分散刷跑：到点触发（内部有窗口/间隔/每日去重判断）
        self.tick_auto_run();
        #[cfg(target_os = "android")]
        self.poll_device_info();
        crate::platform::set_keep_screen_on(
            self.run_busy || self.ai_busy || self.login_busy || self.accounts_busy || self.batch_run_busy,
        );
        // 自适应重绘：任务进行中 100ms 轮询后台消息；空闲时降到 500ms 省电
        // （鼠标/键盘输入仍会即时触发重绘，不影响操作流畅度）
        let busy = self.run_busy
            || self.ai_busy
            || self.login_busy
            || self.records_busy
            || self.data_busy
            || self.user_busy
            || self.accounts_busy
            || self.batch_run_busy;
        ctx.request_repaint_after(if busy {
            std::time::Duration::from_millis(100)
        } else {
            std::time::Duration::from_millis(500)
        });

        // ── 顶栏 ────────────────────────────────────────────────
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.add_space(6.0);
            let compact = mobile::compact_ui(ui);
            let logged = self.logged_account_count();
            let total = self.accounts.len();
            let ip = self.ip.clone();
            let current = match &self.session {
                Some(s) => format!("● {}（uid={}）", s.name, s.uid),
                None => "● 未选账号".to_string(),
            };
            let device = self.identity.device_id.clone();
            let busy = self.accounts_busy || self.batch_run_busy;
            let logo = self.logo.clone();
            let draw_connection = |ui: &mut egui::Ui| {
                if let Some(tex) = &logo {
                    ui.add(
                        egui::Image::new(egui::load::SizedTexture::from_handle(tex))
                            .fit_to_exact_size(egui::vec2(22.0, 22.0)),
                    );
                }
                ui.label("IP：");
                if ip.is_empty() {
                    ui.colored_label(theme::warn(), "获取中…");
                } else if ip == "失败" {
                    ui.colored_label(theme::err(), "失败");
                } else {
                    ui.monospace(&ip);
                }
                let color = if logged > 0 { theme::ok() } else { theme::err() };
                ui.colored_label(color, &current);
            };
            let mut open_accounts = false;
            if compact {
                ui.horizontal_wrapped(draw_connection);
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("设备：{device}"));
                    ui.separator();
                    ui.colored_label(
                        if logged > 0 { theme::ok() } else { theme::text_dim() },
                        format!("已登录账号 {logged} / {total}"),
                    );
                });
                ui.horizontal_wrapped(|ui| {
                    open_accounts = ui
                        .add_enabled(!busy, theme::primary_btn("账号管理"))
                        .clicked();
                    if busy {
                        ui.label("批量任务进行中…");
                    }
                });
            } else {
                egui::Grid::new("top_row1")
                    .num_columns(6)
                    .spacing([10.0, 3.0])
                    .show(ui, |ui| {
                        draw_connection(ui);
                        ui.label("设备：");
                        ui.monospace(&device);
                        ui.end_row();
                    });
                ui.add_space(3.0);
                egui::Grid::new("top_row2")
                    .num_columns(5)
                    .spacing([10.0, 3.0])
                    .show(ui, |ui| {
                        ui.label("账号：");
                        ui.colored_label(
                            if logged > 0 { theme::ok() } else { theme::text_dim() },
                            format!("已登录 {logged} 个 / 共 {total} 个"),
                        );
                        ui.separator();
                        open_accounts = ui
                            .add_enabled(!busy, theme::primary_btn("账号管理"))
                            .clicked();
                        if busy {
                            ui.colored_label(theme::warn(), "批量任务进行中…");
                        }
                        ui.end_row();
                    });
            }
            if open_accounts {
                self.tab = TAB_ACCOUNTS;
            }
            ui.add_space(4.0);
        });

        // ── 底部状态栏 ──────────────────────────────────────────
        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("日志 {} 行", self.log.len()));
                ui.separator();
                if let Some(s) = &self.data_page.semester {
                    ui.label(format!(
                        "学期 {}：有效 {}/{} 次",
                        s.sname, s.semester_valid_count, s.semester_count
                    ));
                }
                if let Some(c) = &self.data_page.cheat {
                    if c.is_clean() {
                        ui.colored_label(theme::ok(), "自查正常");
                    } else {
                        ui.colored_label(theme::err(), format!("已标记：{}", c.self_brief()));
                    }
                }
                ui.separator();
                ui.label(&self.status);
                if let Some(p) = &self.batch_progress {
                    ui.separator();
                    ui.colored_label(
                        theme::warn(),
                        format!("{} {}/{}", p.label, p.done, p.total),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(VERSION_TEXT)
                            .small()
                            .color(theme::text_dim()),
                    );
                });
            });
            ui.add_space(2.0);
        });

        // ── 标签页 ──────────────────────────────────────────────
        egui::CentralPanel::default().show(ctx, |ui| {
            mobile::tab_bar(ui, &mut self.tab);
            ui.separator();
            match self.tab {
                TAB_ACCOUNTS => self.draw_accounts(ui),
                TAB_RUN => self.draw_run(ui),
                2 => self.draw_ai(ui),
                3 => self.draw_records(ui),
                4 => self.draw_data(ui),
                5 => self.draw_user(ui),
                TAB_DEVICE => self.draw_device(ui),
                TAB_CAMPUS => self.draw_campus(ui),
                _ => self.log.render(ui),
            }
        });

        if let Some(p) = &self.popup {
            let mut open = true;
            let mut close = false;
            let mut copy = false;
            let width = (ctx.screen_rect().width() - 32.0).min(560.0);
            let height = (ctx.screen_rect().height() - 220.0).max(160.0);
            egui::Window::new(egui::RichText::new(&p.title).strong())
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    ui.set_max_width(width);
                    egui::ScrollArea::vertical()
                        .id_salt("popup_scroll")
                        .auto_shrink([false, false])
                        .max_height(height)
                        .show(ui, |ui| {
                            ui.set_min_width(width - 24.0);
                            for line in &p.lines {
                                ui.label(line);
                            }
                        });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("复制内容").clicked() {
                            copy = true;
                        }
                        if ui.button("关闭").clicked() {
                            close = true;
                        }
                    });
                });
            if copy {
                if let Some(p) = &self.popup {
                    let text = p.lines.join("\n");
                    ctx.output_mut(|o| o.copied_text = text);
                    self.status = "已复制到剪贴板".into();
                }
            }
            if !open || close {
                self.popup = None;
            }
        }
        crate::platform::sync_clipboard(ctx);
    }
}

/// 解码内置 PNG 图标为 egui 纹理（软件内图标，顶栏展示）。
fn load_logo(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let bytes = include_bytes!("../../assets/icon.png");
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    let color =
        egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &img.into_raw());
    Some(ctx.load_texture("app-logo", color, egui::TextureOptions::LINEAR))
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let font_loaded = fonts::install(&cc.egui_ctx);
        let logo = load_logo(&cc.egui_ctx);
        theme::apply(&cc.egui_ctx);
        let identity = model::load_identity();
        let device_buf = identity.clone();
        let stored_session = model::load_session();
        let config = model::load_config();
        let (accounts, active_account) = load_accounts_with_fallback(&stored_session);
        let session = active_account
            .and_then(|index| accounts.get(index))
            .and_then(|a| a.session.clone());

        let mut app = Self {
            tx: tx.clone(),
            rx,
            log: log::LogStore::default(),
            font_loaded,
            logo,
            ip: String::new(),
            identity,
            device_buf,
            session,
            config: config.clone(),
            accounts,
            active_account,
            records_view: None,
            accounts_busy: false,
            batch_run_busy: false,
            batch_progress: None,
            two_day_plan: None,
            two_day_busy: false,
            two_day_pause: None,
            two_day_cancel: None,
            login_busy: false,
            run_busy: false,
            ai_busy: false,
            records_busy: false,
            data_busy: false,
            user_busy: false,
            status: "就绪".into(),
            popup: None,
            ai_confirm: None,
            tab: 0,
            campus_page: campus::CampusPage::load(),
            run_page: run::RunPage {
                dist_min: config.dist_min,
                dist_max: config.dist_max,
                pace_min: config.pace_min,
                pace_max: config.pace_max,
                manual_altitude: config.manual_altitude_range
                    .map(|r| format!("{}-{}", r.min_m, r.max_m))
                    .or_else(|| config.manual_altitude.map(|v| v.to_string()))
                    .unwrap_or_default(),
                start_mode: 0,
                days_ago: 0,
                hour: 12,
                minute: 0,
                face_check: config.face_check,
                route_engine: match config.route_engine.as_str() {
                    "v4" => 3,
                    "v3" => 2,
                    "v2" => 1,
                    _ => 3,
                },
                v2_candidates: Vec::new(),
                v2_campus: None,
                v2_selected: 0,
                v2_status: String::new(),
                v3_candidates: Vec::new(),
                v3_selected: 0,
                v3_status: String::new(),
                v3_checkpoints: Vec::new(),
                v3_campus: None,
                plan: None,
            },
            ai_page: ai::AiPage { days: 1, per_day: 1, ..Default::default() },
            records_page: records::RecordsPage::default(),
            data_page: data::DataPage::default(),
            user_page: user::UserPage::default(),
            device_page: device::DevicePage::default(),
            accounts_page: accounts::AccountsPage::new(config.concurrency),
        };
        if app.font_loaded.is_none() {
            app.log.push("未找到中文字体（msyh/simhei/simsun），界面中文可能显示为方块");
        }
        // 为每个账号补齐并固定专属设备身份（device_id / idfa / MAC / 机型 / 安装时间）。
        // 导入或添加时已生成；这里兜底旧数据文件中缺失 identity 的账号。
        {
            let base = app.identity.clone();
            let mut changed = 0usize;
            for account in app.accounts.iter_mut() {
                if account.identity.is_none() && !account.username.is_empty() {
                    account.identity = Some(crate::crypto::header::derive_per_account(
                        &base,
                        &account.username,
                    ));
                    changed += 1;
                }
            }
            if changed > 0 {
                let _ = app.persist_accounts();
                app.log
                    .push(&format!("为 {changed} 个账号生成并固定专属设备身份"));
            }
        }
        // 启动时做一次安全备份（内容与上次一致会自动跳过）
        app.backup_data();
        // 单账号任务沿用当前账号/全局代理
        crate::api::client::set_global_proxy(
            app.proxy_for(app.active_account.and_then(|i| app.accounts.get(i))),
        );
        app.fetch_ip();
        #[cfg(target_os = "android")]
        crate::android::request_device_info(true);
        // AI 项目列表先上缓存，网络刷新后覆盖
        app.ai_page.list = crate::api::model::load_ai_sports().unwrap_or_default();
        if app.session.is_some() {
            app.refresh_data_page();
            app.refresh_records();
            app.refresh_user_page();
            app.refresh_ai_list();
        }
        app
    }

    /// 后台任务（线程 + 日志 channel）。
    pub fn spawn_job<F>(&self, f: F)
    where
        F: FnOnce(Sender<String>) + Send + 'static,
    {
        let tx = self.tx.clone();
        std::thread::spawn(move || f(tx));
    }

    pub fn logger(tx: Sender<String>) -> impl FnMut(&str) {
        move |s: &str| {
            tx.send(s.to_string()).ok();
        }
    }

    // ── 批量账号：状态维护 ─────────────────────────────────────

    pub fn logged_account_count(&self) -> usize {
        self.accounts.iter().filter(|a| a.logged_in()).count()
    }

    pub fn persist_accounts(&self) -> Result<(), String> {
        let result = model::save_accounts(&self.accounts);
        if let Err(error) = &result {
            // 保存失败绝不能静默：账号全在内存里，一关软件就没了。
            // 写一份显眼的日志到数据目录，用户能直接看到。
            let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
            let line = format!(
                "{stamp}  保存账号失败：{error}
  账号数：{}  文件：{}
",
                self.accounts.len(),
                crate::platform::data_dir().join("accounts.json").display()
            );
            let path = crate::platform::data_dir().join("persist_error.log");
            let mut text = std::fs::read_to_string(&path).unwrap_or_default();
            text.push_str(&line);
            let _ = std::fs::write(&path, text);
            eprintln!("{line}");
        }
        // 自动安全备份（10 分钟节流 + 内容去重）
        self.backup_throttled();
        result
    }

    /// 当前账号会话实际使用的设备身份（账号专属身份优先，单账号模式回落本机身份）。
    pub fn session_identity(&self) -> HeaderIdentity {
        if let Some(index) = self.active_account {
            if let Some(account) = self.accounts.get(index) {
                return accounts::identity_for(&self.identity, account);
            }
        }
        self.identity.clone()
    }

    /// 把 accounts[index] 设为「当前账号」：会话镜像到 self.session 并落盘。
    pub fn set_active_account(&mut self, index: usize) {
        let Some(account) = self.accounts.get(index) else { return };
        if account.session.is_none() {
            self.status = "该账号尚未登录，无法设为当前账号".into();
            return;
        }
        self.active_account = Some(index);
        self.session = account.session.clone();
        // 单账号任务跟随当前账号的代理
        crate::api::client::set_global_proxy(self.proxy_for(self.accounts.get(index)));
        match &self.session {
            Some(session) => {
                if let Err(e) = model::save_session(session) {
                    self.status = format!("× 会话保存失败：{e}");
                }
            }
            None => model::clear_session(),
        }
    }

    /// 批量任务结束后，若还没有当前账号，则自动选第一个已登录账号。
    pub fn ensure_active_account(&mut self) -> bool {
        if self.session.is_some() {
            return false;
        }
        match self.accounts.iter().position(|a| a.logged_in()) {
            Some(index) => {
                self.set_active_account(index);
                true
            }
            None => false,
        }
    }
}

/// 载入账号列表：accounts.json 为空时用 session.json 兜底回填一条。
fn load_accounts_with_fallback(stored: &Session) -> (Vec<model::Account>, Option<usize>) {
    let mut accounts = model::load_accounts();
    let mut active = accounts.iter().position(|a| a.logged_in());
    if active.is_none() && stored.is_logged_in() {
        match accounts.iter().position(|a| a.username == stored.username) {
            Some(index) => {
                accounts[index].session = Some(stored.clone());
                if accounts[index].name.is_empty() {
                    accounts[index].name = stored.name.clone();
                }
            }
            None => accounts.push(model::Account {
                username: stored.username.clone(),
                password: String::new(),
                session: Some(stored.clone()),
                name: stored.name.clone(),
                ..Default::default()
            }),
        }
        active = accounts.iter().position(|a| a.logged_in());
        let _ = model::save_accounts(&accounts);
    }
    (accounts, active)
}
