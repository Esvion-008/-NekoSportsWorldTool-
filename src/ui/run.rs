//! 跑步页：恒 sportType=1；距离/配速范围 + 开始时间（随机或指定，最多前 3 天）。
//!
//! 开始时间：「日期」下拉（今天 / 1-3 天前）两种模式共用；随机模式在该日 7:00-20:00
//! 内抽样，并保证「开始 + 用时」不越过当前时刻；指定模式完全按用户填写的时/分
//! （尚未到达时按当前时刻），「换一版」只重掷运动量，不动时刻。

use super::{mobile, theme, App};
use crate::api::batch;
use crate::api::model::HeaderIdentity;
use chrono::{Datelike, Duration, Local, TimeZone, Timelike};
use eframe::egui;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// 随机时刻窗口（含端点）：7:00-20:00。
const RAND_HOUR_LO: u32 = 7;
const RAND_HOUR_HI: u32 = 20;
/// flow.rs 提交时会给开始时间加 0-4s 抖动，随机模式抽样时预留，避免「开始 + 用时」越过当前时刻。
const JITTER_MARGIN_MS: i64 = 5_000;

/// 指定日期（days_ago 天前）的 7:00-20:00 内均匀抽样一个时刻，且不晚于 latest_ms。
///
/// latest_ms = 当前时刻 - 用时 - 抖动余量。若该日窗口整体晚于 latest_ms（例如凌晨选「今天」），
/// 退化为 latest_ms，宁可贴近当前时间，也不产生「未来开跑」。
fn random_time_ago(days_ago: i64, latest_ms: i64) -> i64 {
    let base = Local::now() - Duration::days(days_ago.clamp(0, 3));
    let lo = Local.with_ymd_and_hms(base.year(), base.month(), base.day(), RAND_HOUR_LO, 0, 0).single();
    let hi = Local.with_ymd_and_hms(base.year(), base.month(), base.day(), RAND_HOUR_HI, 0, 0).single();
    let (lo_ms, hi_ms) = match (lo, hi) {
        (Some(l), Some(h)) => (l.timestamp_millis(), h.timestamp_millis()),
        _ => return latest_ms,
    };
    let hi_ms = hi_ms.min(latest_ms);
    let lo_ms = lo_ms.min(hi_ms);
    let span = (hi_ms - lo_ms).max(0) as f64;
    lo_ms + (rand::random::<f64>() * span) as i64
}

/// 指定时刻：days_ago(0=今天) + 时/分；超出 3 天或在未来时做钳制。
fn specified_time(days_ago: i64, hour: i64, minute: i64) -> i64 {
    let now = Local::now();
    let base = now - Duration::days(days_ago.clamp(0, 3));
    let h = hour.clamp(0, 23);
    let m = minute.clamp(0, 59);
    let t = Local
        .with_ymd_and_hms(base.year(), base.month(), base.day(), h as u32, m as u32, 0)
        .single()
        .map(|x| x.timestamp_millis())
        .unwrap_or_else(crate::crypto::envelope::now_ms);
    t.min(now.timestamp_millis())
}

/// 日期下拉的显示文本。
fn days_ago_label(days_ago: i64) -> String {
    match days_ago {
        0 => "今天".into(),
        1 => "昨天".into(),
        n => format!("{n} 天前"),
    }
}

#[derive(Default)]
pub struct RunPage {
    pub dist_min: f32,
    pub dist_max: f32,
    pub pace_min: f32,
    pub pace_max: f32,
    /// 手动绝对海拔（米）；空白表示使用生成器默认海拔。
    pub manual_altitude: String,
    /// 0=随机时刻 1=指定时刻
    pub start_mode: usize,
    pub days_ago: i64,
    pub hour: i64,
    pub minute: i64,
    pub face_check: bool,
    /// 0=v1 拟合环（默认） 1=v2 校园路网。
    pub route_engine: usize,
    /// V2 预览候选与选中项（阶段 5）。
    pub v2_candidates: Vec<crate::route::RouteCandidate>,
    pub v2_campus: Option<crate::campus::Campus>,
    pub v2_selected: usize,
    pub v2_status: String,
    /// V3 预览候选
    pub v3_candidates: Vec<crate::route::v3::V3Candidate>,
    pub v3_selected: usize,
    pub v3_status: String,
    pub v3_checkpoints: Vec<crate::campus::LngLat>,
    pub v3_campus: Option<crate::campus::Campus>,
    /// 预计算方案：参数变更时重抽样，提交直接使用
    pub plan: Option<RunPlan>,
}

/// 一次性提交的确定方案（进入页面/参数变更时抽样生成）。
#[derive(Debug, Clone)]
pub struct RunPlan {
    pub dist_min: f32,
    pub dist_max: f32,
    pub pace_min: f32,
    pub pace_max: f32,
    pub start_mode: usize,
    pub days_ago: i64,
    pub hour: i64,
    pub minute: i64,
    /// 公里
    pub dist: f64,
    /// 秒/km
    pub pace: f32,
    /// 秒
    pub dur: i64,
    pub start_ms: i64,
}

/// 两天计划中的单个账号状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TwoDayStatus {
    Pending,
    Running,
    Ok,
    Fail,
}

/// 单个账号在两天窗口内的一次执行。
#[derive(Clone, Debug)]
pub struct TwoDayJob {
    pub index: usize,
    pub username: String,
    pub due_ms: i64,
    pub day: usize,
    pub status: TwoDayStatus,
    pub message: String,
}

/// 两天批量计划（阶段 6）。
#[derive(Clone, Debug, Default)]
pub struct TwoDayPlan {
    pub jobs: Vec<TwoDayJob>,
    pub created_ms: i64,
    pub note: String,
}

/// 每个执行任务的不可变快照（交给后台线程）。
struct TwoDayRun {
    index: usize,
    username: String,
    due_ms: i64,
    day: usize,
    session: crate::api::model::Session,
    identity: HeaderIdentity,
    proxy: Option<String>,
    dist: f64,
    dur: i64,
    face_check: i64,
    manual_altitude: Option<f64>,
    manual_altitude_range: Option<crate::track::altitude::AltitudeRange>,
    seed: u64,
    route_engine: crate::api::flow::RouteEngine,
}

impl RunPage {

    /// 距离/配速是否与方案一致（不一致需重掷运动参数）。
    fn shape_matches(&self, p: &RunPlan) -> bool {
        p.dist_min == self.dist_min
            && p.dist_max == self.dist_max
            && p.pace_min == self.pace_min
            && p.pace_max == self.pace_max
    }

    /// 开始时间控件是否与方案一致。
    ///
    /// 随机模式的时刻是抽样结果（不是输入），只比对日期；指定模式的时/分是用户输入，需回比。
    fn time_matches(&self, p: &RunPlan) -> bool {
        if p.start_mode != self.start_mode || p.days_ago != self.days_ago {
            return false;
        }
        self.start_mode == 0 || (p.hour == self.hour && p.minute == self.minute)
    }

    /// 开跑时刻上限：当前时刻 - 用时 - 抖动余量（flow.rs 提交时还会再加 0-4s）。
    fn latest_start_ms(dur: i64) -> i64 {
        crate::crypto::envelope::now_ms() - dur * 1000 - JITTER_MARGIN_MS
    }

    /// 按当前参数抽样运动量（距离 / 配速 / 用时）。
    fn sample_shape(&self) -> (f64, f32, i64) {
        let (lo, hi) = (self.dist_min.min(self.dist_max), self.dist_min.max(self.dist_max));
        let (plo, phi) = (self.pace_min.min(self.pace_max), self.pace_min.max(self.pace_max));
        let pace = plo + (phi - plo) * rand::random::<f32>();
        let dist = (lo + (hi - lo) * rand::random::<f32>()) as f64;
        let dur = (dist * pace as f64).round() as i64;
        (dist, pace, dur)
    }

    /// 按当前模式与日期取一个开始时刻（随机模式抽样，指定模式采纳输入框）。
    fn pick_time(&self) -> i64 {
        if self.start_mode == 0 {
            random_time_ago(self.days_ago, Self::latest_start_ms(self.dur_hint()))
        } else {
            specified_time(self.days_ago, self.hour, self.minute)
        }
    }

    /// 用于随机抽样的时长上限：已抽样的方案优先，否则按当前配速区间上界估一个。
    fn dur_hint(&self) -> i64 {
        match &self.plan {
            Some(p) => p.dur,
            None => {
                let hi = self.dist_min.max(self.dist_max) as f64;
                let phi = self.pace_min.max(self.pace_max) as f64;
                (hi * phi).round() as i64
            }
        }
    }

    /// 运动参数变更时重抽距离/配速/用时，保留已选开始时刻（指定模式的时刻是用户输入，不动）。
    fn regen_shape(&mut self) {
        let (dist, pace, dur) = self.sample_shape();
        let start_ms = match (&self.plan, self.start_mode) {
            // 随机模式沿用已抽样的时刻（仅按新用时收紧上限），避免拖动距离时时刻乱跳
            (Some(p), 0) => p.start_ms.min(Self::latest_start_ms(dur)),
            _ => self.pick_time(),
        };
        self.write_plan(dist, pace, dur, start_ms);
    }

    /// 只重算开始时刻，保留已抽样的运动量（日期 / 模式变更时用）。
    fn resync_time(&mut self) {
        let start_ms = self.pick_time();
        if let Some(p) = self.plan.as_mut() {
            p.start_mode = self.start_mode;
            p.days_ago = self.days_ago;
            p.hour = self.hour;
            p.minute = self.minute;
            p.start_ms = start_ms;
        }
    }

    /// 把当前参数与已算好的量写成方案。
    fn write_plan(&mut self, dist: f64, pace: f32, dur: i64, start_ms: i64) {
        self.plan = Some(RunPlan {
            dist_min: self.dist_min,
            dist_max: self.dist_max,
            pace_min: self.pace_min,
            pace_max: self.pace_max,
            start_mode: self.start_mode,
            days_ago: self.days_ago,
            hour: self.hour,
            minute: self.minute,
            dist,
            pace,
            dur,
            start_ms,
        });
    }

    /// 「换一版」：重掷运动量；随机模式下同时换一个开始时刻。
    ///
    /// 指定模式的时刻由用户填写，按钮不碰它（用户填什么就是什么）。
    pub fn regen_plan(&mut self) {
        let (dist, pace, dur) = self.sample_shape();
        let start_ms = if self.start_mode == 0 {
            random_time_ago(self.days_ago, Self::latest_start_ms(dur))
        } else {
            specified_time(self.days_ago, self.hour, self.minute)
        };
        self.write_plan(dist, pace, dur, start_ms);
    }

    /// 只读抽样一份完整方案（批量刷跑逐账号调用；不修改页面状态）。
    pub fn sample_plan(&self) -> RunPlan {
        let (dist, pace, dur) = self.sample_shape();
        let start_ms = if self.start_mode == 0 {
            random_time_ago(self.days_ago, Self::latest_start_ms(dur))
        } else {
            specified_time(self.days_ago, self.hour, self.minute)
        };
        RunPlan {
            dist_min: self.dist_min,
            dist_max: self.dist_max,
            pace_min: self.pace_min,
            pace_max: self.pace_max,
            start_mode: self.start_mode,
            days_ago: self.days_ago,
            hour: self.hour,
            minute: self.minute,
            dist,
            pace,
            dur,
            start_ms,
        }
    }

    /// 批量刷跑专用：在 [`sample_plan`] 基础上把开始时间往前错开 0–25 分钟。
    ///
    /// 几十个账号共用一个「指定时刻」时，排行榜上会变成「同一分钟几十个人在同一个
    /// 操场跑步」，一看就是刷的。随机模式本身已经随机，这里只对指定时刻生效。
    pub fn sample_plan_batched(&self) -> RunPlan {
        let mut plan = self.sample_plan();
        if self.start_mode != 0 {
            let jitter_ms = rand::random::<i64>().rem_euclid(25 * 60 * 1000);
            plan.start_ms = plan.start_ms.saturating_sub(jitter_ms);
        }
        plan
    }

    /// 确保方案与当前参数一致（参数变更补齐 / 提交前兜底）。
    ///
    /// 两个一致性判定都在改写方案「之前」求值：否则 regen_shape 会把当前参数写进方案，
    /// 同帧内「距离 + 日期」一起改时日期变化将被漏掉。
    pub fn ensure_plan(&mut self) {
        let shape_ok = self.plan.as_ref().map(|p| self.shape_matches(p)) == Some(true);
        let time_ok = self.plan.as_ref().map(|p| self.time_matches(p)) == Some(true);
        if !shape_ok {
            self.regen_shape();
        }
        if !time_ok {
            self.resync_time();
        }
    }

    /// 随机模式的抽样结果是否落在所选日期的 7:00-20:00 窗口内。
    ///
    /// 凌晨选「今天」时窗口尚未到达，`random_time_ago` 会退化到贴近当前时刻，
    /// 此时界面需要说明，避免用户以为刻意选了凌晨。
    fn random_window_ok(&self) -> bool {
        let Some(p) = &self.plan else { return true };
        let Some(t) = Local.timestamp_millis_opt(p.start_ms).single() else {
            return true;
        };
        let day = (Local::now() - Duration::days(self.days_ago.clamp(0, 3))).date_naive();
        t.date_naive() == day && (RAND_HOUR_LO..=RAND_HOUR_HI).contains(&t.hour())
    }
}

impl App {
    pub fn draw_run(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.draw_run_content(ui);
            });
    }

    fn draw_run_content(&mut self, ui: &mut egui::Ui) {
        {
            let page = &mut self.run_page;
            let compact = mobile::compact_ui(ui);
            let mut draw_distance_inputs = |ui: &mut egui::Ui| {
                mobile::drag_f32(ui, "run_dist_min", &mut page.dist_min, 0.5..=20.0, 0.05, 2, " km");
                ui.label("至");
                mobile::drag_f32(ui, "run_dist_max", &mut page.dist_max, 0.5..=20.0, 0.05, 2, " km");
            };
            if compact {
                ui.label("距离范围（km）：");
                ui.horizontal(draw_distance_inputs);
            } else {
                ui.horizontal(|ui| {
                    ui.label("距离范围（km）：");
                    draw_distance_inputs(ui);
                });
            }
            let mut draw_pace_inputs = |ui: &mut egui::Ui| {
                mobile::drag_f32(ui, "run_pace_min", &mut page.pace_min, 180.0..=520.0, 5.0, 0, "");
                ui.label("至");
                mobile::drag_f32(ui, "run_pace_max", &mut page.pace_max, 180.0..=520.0, 5.0, 0, "");
            };
            if compact {
                ui.label("配速范围（秒/km）：");
                ui.horizontal(draw_pace_inputs);
            } else {
                ui.horizontal(|ui| {
                    ui.label("配速范围（秒/km）：");
                    draw_pace_inputs(ui);
                });
            }
            mobile::row(ui, |ui| {
                ui.label("手动海拔（米/区间）：");
                mobile::text_edit(
                    ui,
                    "run_manual_altitude",
                    &mut page.manual_altitude,
                    crate::platform::InputKind::Text,
                    120.0,
                );
                ui.label("留空自动；可填 17.2 或 11.6-22.8");
            });
            mobile::row(ui, |ui| {
                ui.label("开始时间：");
                ui.radio_value(&mut page.start_mode, 0, "随机时刻");
                ui.radio_value(&mut page.start_mode, 1, "指定时刻");
                ui.label("日期：");
                egui::ComboBox::from_id_salt("run_days_ago")
                    .width(96.0)
                    .selected_text(days_ago_label(page.days_ago))
                    .show_ui(ui, |ui| {
                        for d in 0..=3 {
                            ui.selectable_value(&mut page.days_ago, d, days_ago_label(d));
                        }
                    });
                if page.start_mode == 0 {
                    ui.label(format!("（{RAND_HOUR_LO}:00-{RAND_HOUR_HI}:00 内随机）"));
                } else {
                    ui.label("时刻：");
                    mobile::drag_i64(ui, "run_hour", &mut page.hour, 0..=23, 1.0, "", " 点");
                    ui.label(":");
                    mobile::drag_i64(ui, "run_minute", &mut page.minute, 0..=59, 1.0, "", "");
                }
            });
            if page.start_mode == 1 && page.days_ago == 0 {
                // 今天 + 指定时刻：提示是否落在未来
                let now = Local::now();
                let spec = Local
                    .with_ymd_and_hms(now.year(), now.month(), now.day(), page.hour as u32, page.minute as u32, 0)
                    .single();
                if let Some(t) = spec {
                    if t.timestamp_millis() > crate::crypto::envelope::now_ms() {
                        ui.colored_label(theme::warn(), "指定时刻在今天且尚未到达，将按当前时间提交");
                    }
                }
            }
            mobile::row(ui, |ui| {
                ui.label("人脸校验标记：");
                ui.checkbox(&mut page.face_check, "faceCheck=1");
            });
            let route_engine_before = page.route_engine;
            mobile::row(ui, |ui| {
                ui.label("路线引擎：");
                ui.radio_value(&mut page.route_engine, 3, "v4 无敌算法（默认 · 操场走标准跑道分道）");
                ui.radio_value(&mut page.route_engine, 0, "v1 拟合环");
                ui.radio_value(&mut page.route_engine, 1, "v2 校园路网");
                ui.radio_value(&mut page.route_engine, 2, "v3 道路优先（严格顺序）");
                if page.route_engine == 1 {
                    ui.colored_label(
                        theme::warn(),
                        "v2 目标固定 2000±5m；需先在「校园地图」导入 campus.json 与路网",
                    );
                }
            });
            if page.route_engine != route_engine_before {
                self.config.route_engine = match page.route_engine {
                    3 => "v4",
                    2 => "v3",
                    1 => "v2",
                    _ => "v1",
                }
                .into();
                let _ = crate::api::model::save_config(&self.config);
            }
        }
        if self.run_page.route_engine == 1 {
            self.draw_v2_preview(ui);
        }
        if self.run_page.route_engine == 2 {
            self.draw_v3_preview(ui);
        }

        ui.add_space(8.0);
        let fmt_pace = |s: f32| format!("{}:{:02}", (s / 60.0) as i64, (s as i64) % 60);
        let fmt_dur = |s: i64| {
            if s >= 3600 {
                format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
            } else {
                format!("{}:{:02}", s / 60, s % 60)
            }
        };
        // 参数变更时补方案；显示本次提交的确定方案
        self.run_page.ensure_plan();
        self.draw_run_warnings(ui);
        let plan_label = match &self.run_page.plan {
            Some(p) => {
                let start = chrono::Local
                    .timestamp_millis_opt(p.start_ms)
                    .single()
                    .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_default();
                let (dist, pace, dur) = (p.dist, p.pace, p.dur);
                format!(
                    "本次方案：距离 {dist:.2} km · 配速 {}/km · 用时 {} · 开始 {start}",
                    fmt_pace(pace),
                    fmt_dur(dur)
                )
            }
            None => String::new(),
        };
        mobile::row(ui, |ui| {
            ui.colored_label(theme::plain(), plan_label);
            if ui.small_button("换一版").clicked() {
                self.run_page.regen_plan();
            }
        });

        ui.add_space(8.0);
        let enabled = !self.run_busy && self.session.is_some();
        let btn = if self.run_busy { theme::primary_btn("提交中…") } else { theme::primary_btn("开始跑步") };
        mobile::row(ui, |ui| {
            if ui.add_enabled(enabled, btn).clicked() {
                self.start_run();
            }
            if self.session.is_none() {
                ui.label("（请先登录）");
            }
        });

        // ── 批量刷跑：账号页里所有已登录账号同时刷 ──────────────
        ui.add_space(6.0);
        ui.separator();
        let logged_accounts = self.logged_account_count();
        let batch_busy = self.batch_run_busy || self.accounts_busy;
        let batch_btn = if self.batch_run_busy {
            theme::primary_btn("批量刷跑中…")
        } else {
            theme::primary_btn(&format!("批量刷跑（{logged_accounts} 个已登录账号）"))
        };
        mobile::row(ui, |ui| {
            if ui
                .add_enabled(!batch_busy && logged_accounts > 0, batch_btn)
                .clicked()
            {
                self.start_batch_run();
            }
            if logged_accounts == 0 {
                ui.label("（先在「账号」页登录账号）");
            } else {
                ui.label("对账号页所有已登录账号同时刷跑，距离/配速/时间参数同上");
            }
        });
        if let Some(p) = &self.batch_progress {
            let fraction = if p.total > 0 { p.done as f32 / p.total as f32 } else { 0.0 };
            ui.add(egui::ProgressBar::new(fraction).text(format!("{} {}/{}", p.label, p.done, p.total)));
        }
        if self.run_page.route_engine == 1 || self.run_page.route_engine == 2 {
            self.draw_two_day_plan(ui);
        }
    }

    /// 开始时间相关的提示。须在 `ensure_plan()` 之后调用，才对得上本次提交时刻。
    fn draw_run_warnings(&self, ui: &mut egui::Ui) {
        let page = &self.run_page;
        if page.start_mode == 0 {
            if !page.random_window_ok() {
                ui.colored_label(
                    theme::warn(),
                    format!(
                        "所选日期的 {RAND_HOUR_LO}:00-{RAND_HOUR_HI}:00 尚未到达，本次只能贴近当前时刻",
                    ),
                );
            }
            return;
        }
        // 今天 + 指定时刻：提示是否落在未来（与原有行为一致）
        if page.days_ago != 0 {
            return;
        }
        let now = Local::now();
        if let Some(t) = Local
            .with_ymd_and_hms(
                now.year(),
                now.month(),
                now.day(),
                page.hour.clamp(0, 23) as u32,
                page.minute.clamp(0, 59) as u32,
                0,
            )
            .single()
        {
            if t.timestamp_millis() > crate::crypto::envelope::now_ms() {
                ui.colored_label(theme::warn(), "指定时刻在今天且尚未到达，将按当前时间提交");
            }
        }
    }

    /// 阶段 6：生成两天批量计划（每账号两天各一次，窗口 7:00–20:00）。
    pub(crate) fn plan_two_day(&mut self) {
        if self.two_day_busy || self.batch_run_busy {
            self.status = "已有批量任务进行中".into();
            return;
        }
        let accounts: Vec<(usize, String)> = self
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, account)| account.logged_in() && account.selected)
            .map(|(index, account)| (index, account.username.clone()))
            .collect();
        if accounts.is_empty() {
            self.status = "没有勾选且已登录的账号，无法生成两天计划".into();
            return;
        }
        let today = Local::now().date_naive();
        let day1 = today + Duration::days(1);
        let day2 = day1 + Duration::days(1);
        let wave1 = 7 * 3600 + (rand::random::<f64>() * 5.0 * 3600.0) as i64; // 07:00–12:00
        let wave2 = 19 * 3600 + (rand::random::<f64>() * 3600.0) as i64; // 19:00–20:00
        let base1 = Local
            .with_ymd_and_hms(day1.year(), day1.month(), day1.day(), 0, 0, 0)
            .single()
            .map(|time| time.timestamp_millis())
            .unwrap_or_else(crate::crypto::envelope::now_ms);
        let base2 = Local
            .with_ymd_and_hms(day2.year(), day2.month(), day2.day(), 0, 0, 0)
            .single()
            .map(|time| time.timestamp_millis())
            .unwrap_or(base1 + 86_400_000);
        let mut jobs = Vec::with_capacity(accounts.len() * 2);
        for (index, username) in accounts {
            let jitter1 = rand::random::<i64>().rem_euclid(60_000);
            let jitter2 = rand::random::<i64>().rem_euclid(60_000);
            jobs.push(TwoDayJob {
                index,
                username: username.clone(),
                due_ms: base1 + wave1 * 1000 + jitter1,
                day: 1,
                status: TwoDayStatus::Pending,
                message: String::new(),
            });
            jobs.push(TwoDayJob {
                index,
                username,
                due_ms: base2 + wave2 * 1000 + jitter2,
                day: 2,
                status: TwoDayStatus::Pending,
                message: String::new(),
            });
        }
        jobs.sort_by_key(|job| job.due_ms);
        let note = format!(
            "{} 个账号 × 2 天：第 1 天 07:00–12:00、第 2 天 19:00–20:00 随机波次；同批共享点位请求，间隔 ≥12h",
            jobs.len() / 2
        );
        self.status = format!("两天计划已生成：{} 个执行任务", jobs.len());
        self.two_day_plan = Some(TwoDayPlan {
            jobs,
            created_ms: crate::crypto::envelope::now_ms(),
            note,
        });
    }

    /// 启动两天计划：后台按计划时间等待并逐账号执行。
    pub(crate) fn start_two_day_plan(&mut self) {
        if self.two_day_busy || self.batch_run_busy {
            self.status = "已有批量任务进行中".into();
            return;
        }
        let Some(plan) = self.two_day_plan.clone() else {
            self.status = "请先生成两天计划".into();
            return;
        };
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
        let route_engine = match self.run_page.route_engine {
            3 => crate::api::flow::RouteEngine::V4,
            2 => crate::api::flow::RouteEngine::V3,
            1 => crate::api::flow::RouteEngine::V2,
            _ => crate::api::flow::RouteEngine::V1,
        };
        let page = &self.run_page;
        let mut runs: Vec<TwoDayRun> = Vec::new();
        for job in plan.jobs {
            if job.status != TwoDayStatus::Pending {
                continue;
            }
            let Some(account) = self.accounts.get(job.index) else {
                continue;
            };
            let Some(session) = account.session.clone() else {
                continue;
            };
            let run_plan = match page.plan.clone() {
                Some(plan) => plan,
                None => page.sample_plan(),
            };
            runs.push(TwoDayRun {
                index: job.index,
                username: job.username,
                due_ms: job.due_ms,
                day: job.day,
                session,
                identity: super::accounts::identity_for(&self.identity, account),
                proxy: self.proxy_for(Some(account)),
                dist: run_plan.dist * 1000.0,
                dur: run_plan.dur,
                face_check,
                manual_altitude,
                manual_altitude_range,
                seed: (crate::crypto::envelope::now_ms() as u64)
                    .wrapping_mul(31)
                    .wrapping_add(job.index as u64),
                route_engine,
            });
        }
        if runs.is_empty() {
            self.status = "两天计划里没有可执行的已登录账号".into();
            return;
        }
        runs.sort_by_key(|run| run.due_ms);
        let total = runs.len();
        let pause = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        self.two_day_pause = Some(Arc::clone(&pause));
        self.two_day_cancel = Some(Arc::clone(&cancel));
        self.two_day_busy = true;
        self.batch_run_busy = true;
        self.batch_progress = Some(super::accounts::BatchProgress {
            label: "两天计划".into(),
            done: 0,
            total,
        });
        self.status = format!("两天计划执行中：{total} 个任务（每账号两天各一次）");
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut done = 0usize;
            let mut ok = 0usize;
            let mut fail = 0usize;
            for run in runs {
                loop {
                    if cancel.load(Ordering::SeqCst) {
                        let _ = tx.send(format!(
                            "{}{}",
                            super::TWO_DAY_DONE,
                            serde_json::json!({
                                "ok": ok, "fail": fail, "total": total, "cancelled": true
                            })
                        ));
                        return;
                    }
                    if pause.load(Ordering::SeqCst) {
                        std::thread::sleep(std::time::Duration::from_secs(1));
                        continue;
                    }
                    let now = crate::crypto::envelope::now_ms();
                    if now >= run.due_ms {
                        break;
                    }
                    let wait = (run.due_ms - now).clamp(200, 20_000) as u64;
                    std::thread::sleep(std::time::Duration::from_millis(wait));
                }
                let due = run.due_ms;
                let _ = tx.send(format!(
                    "{}{}",
                    super::TWO_DAY_PROG,
                    serde_json::json!({
                        "username": run.username, "due_ms": due, "status": "running",
                        "done": done, "total": total
                    })
                ));
                let start_ms = crate::crypto::envelope::now_ms() - run.dur * 1000 - 5_000;
                let params = crate::api::flow::RunParams {
                    dist: run.dist,
                    dur: run.dur,
                    start_ms,
                    face_check: run.face_check,
                    manual_altitude: run.manual_altitude,
                    manual_altitude_range: run.manual_altitude_range,
                    seed: run.seed,
                    route_engine: run.route_engine,
                };
                let mut raw = App::logger(tx.clone());
                let mut log = |message: &str| {
                    raw(&format!("[{}][第{}天] {message}", run.username, run.day));
                };
                log("开始执行两天计划任务");
                let payload = match batch::run_account(
                    &run.identity,
                    run.session.clone(),
                    &params,
                    run.proxy.clone(),
                    &mut log,
                ) {
                    Ok(outcome) => {
                        ok += 1;
                        log(&format!(
                            "√ 两天计划任务成功 rrid={} obs={}/2 verify={}",
                            outcome.result.rrid, outcome.obs_ok, outcome.detail_ok
                        ));
                        serde_json::json!({
                            "index": run.index, "username": run.username, "ok": true,
                            "rrid": outcome.result.rrid, "obs_ok": outcome.obs_ok,
                            "verify": outcome.detail_ok,
                            "dist": outcome.result.total_dis, "dur": outcome.result.total_time,
                        })
                    }
                    Err(message) => {
                        fail += 1;
                        log(&format!("× 两天计划任务失败：{message}"));
                        serde_json::json!({
                            "index": run.index, "username": run.username,
                            "ok": false, "message": message,
                        })
                    }
                };
                let _ = tx.send(format!("{}{}", super::ACCT_RUN, payload));
                done += 1;
                let succeeded = payload["ok"].as_bool().unwrap_or(false);
                let status = if succeeded { "ok" } else { "fail" };
                let _ = tx.send(format!(
                    "{}{}",
                    super::TWO_DAY_PROG,
                    serde_json::json!({
                        "username": run.username, "due_ms": due, "status": status,
                        "done": done, "total": total,
                        "message": payload["message"].as_str().unwrap_or("")
                    })
                ));
            }
            let _ = tx.send(format!(
                "{}{}",
                super::TWO_DAY_DONE,
                serde_json::json!({
                    "ok": ok, "fail": fail, "total": total, "cancelled": false
                })
            ));
        });
    }

    pub(crate) fn retry_two_day_failures(&mut self) {
        if self.two_day_busy || self.batch_run_busy {
            self.status = "当前有任务在执行，稍后再重试".into();
            return;
        }
        let Some(plan) = self.two_day_plan.clone() else {
            return;
        };
        let mut jobs = Vec::new();
        let now = crate::crypto::envelope::now_ms() + 5_000;
        for job in plan.jobs {
            if job.status == TwoDayStatus::Fail {
                jobs.push(TwoDayJob {
                    index: job.index,
                    username: job.username,
                    due_ms: now,
                    day: job.day,
                    status: TwoDayStatus::Pending,
                    message: String::new(),
                });
            }
        }
        if jobs.is_empty() {
            self.status = "没有失败任务需要重试".into();
            return;
        }
        self.two_day_plan = Some(TwoDayPlan {
            jobs,
            created_ms: crate::crypto::envelope::now_ms(),
            note: "失败任务重试（立即执行）".into(),
        });
        self.start_two_day_plan();
    }

    pub(crate) fn apply_two_day_progress(&mut self, value: &serde_json::Value) {
        let username = value["username"].as_str().unwrap_or("");
        let due = value["due_ms"].as_i64().unwrap_or(0);
        let status = match value["status"].as_str().unwrap_or("") {
            "running" => TwoDayStatus::Running,
            "ok" => TwoDayStatus::Ok,
            "fail" => TwoDayStatus::Fail,
            _ => TwoDayStatus::Pending,
        };
        if let Some(plan) = self.two_day_plan.as_mut() {
            if let Some(job) = plan
                .jobs
                .iter_mut()
                .find(|job| job.username == username && job.due_ms == due)
            {
                job.status = status;
                job.message = value["message"].as_str().unwrap_or("").to_string();
            }
        }
        if let Some(done) = value["done"].as_u64() {
            if let Some(progress) = self.batch_progress.as_mut() {
                progress.done = done as usize;
            }
        }
    }

    pub(crate) fn finish_two_day(&mut self, value: &serde_json::Value) {
        self.two_day_busy = false;
        self.batch_run_busy = false;
        self.batch_progress = None;
        self.two_day_pause = None;
        self.two_day_cancel = None;
        let ok = value["ok"].as_u64().unwrap_or(0);
        let fail = value["fail"].as_u64().unwrap_or(0);
        let total = value["total"].as_u64().unwrap_or(0);
        let cancelled = value["cancelled"].as_bool().unwrap_or(false);
        self.status = if cancelled {
            format!("两天计划已取消：成功 {ok} / 失败 {fail}")
        } else {
            format!("两天计划完成：成功 {ok} / 失败 {fail} / 共 {total}")
        };
        if ok > 0 && self.session.is_some() {
            self.refresh_data_page();
        }
    }

    /// 阶段 6：两天计划界面。
    fn draw_two_day_plan(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.separator();
        ui.strong("V2 两天批量计划");
        let paused = self
            .two_day_pause
            .as_ref()
            .map(|flag| flag.load(Ordering::SeqCst))
            .unwrap_or(false);
        mobile::row(ui, |ui| {
            if ui
                .add_enabled(!self.two_day_busy, egui::Button::new("生成两天计划"))
                .clicked()
            {
                self.plan_two_day();
            }
            if ui
                .add_enabled(
                    !self.two_day_busy && self.two_day_plan.is_some(),
                    theme::primary_btn("开始执行"),
                )
                .clicked()
            {
                self.start_two_day_plan();
            }
            if let Some(pause) = &self.two_day_pause {
                if ui.button(if paused { "继续" } else { "暂停" }).clicked() {
                    pause.store(!paused, Ordering::SeqCst);
                }
            }
            if let Some(cancel) = &self.two_day_cancel {
                if ui.button("取消").clicked() {
                    cancel.store(true, Ordering::SeqCst);
                    self.status = "正在取消两天计划…".into();
                }
            }
            if ui
                .add_enabled(!self.two_day_busy, egui::Button::new("重试失败"))
                .clicked()
            {
                self.retry_two_day_failures();
            }
            if self.two_day_busy {
                ui.colored_label(theme::warn(), "两天计划执行中（需保持应用运行）");
            }
        });
        let Some(plan) = self.two_day_plan.clone() else {
            return;
        };
        ui.label(
            egui::RichText::new(&plan.note)
                .small()
                .color(theme::text_dim()),
        );
        let created = Local
            .timestamp_millis_opt(plan.created_ms)
            .single()
            .map(|time| time.format("%m-%d %H:%M:%S").to_string())
            .unwrap_or_default();
        ui.label(
            egui::RichText::new(format!("计划生成：{created}"))
                .small()
                .color(theme::text_dim()),
        );
        egui::ScrollArea::vertical()
            .id_salt("two_day_plan")
            .max_height(180.0)
            .show(ui, |ui| {
                egui::Grid::new("two_day_jobs")
                    .num_columns(4)
                    .striped(true)
                    .show(ui, |ui| {
                        ui.strong("账号");
                        ui.strong("计划时间");
                        ui.strong("天");
                        ui.strong("状态");
                        ui.end_row();
                        for job in &plan.jobs {
                            ui.label(&job.username);
                            let at = Local
                                .timestamp_millis_opt(job.due_ms)
                                .single()
                                .map(|time| time.format("%m-%d %H:%M").to_string())
                                .unwrap_or_else(|| "-".into());
                            ui.monospace(at);
                            ui.monospace(format!("D{}", job.day));
                            let (text, color) = match job.status {
                                TwoDayStatus::Pending => ("等待".to_string(), theme::text_dim()),
                                TwoDayStatus::Running => ("执行中".to_string(), theme::warn()),
                                TwoDayStatus::Ok => ("成功".to_string(), theme::ok()),
                                TwoDayStatus::Fail => (
                                    if job.message.is_empty() {
                                        "失败".to_string()
                                    } else {
                                        format!("失败：{}", job.message)
                                    },
                                    theme::err(),
                                ),
                            };
                            ui.colored_label(color, text);
                            ui.end_row();
                        }
                    });
            });
    }

    /// 阶段 5：用点位缓存（无缓存时用校园 POI）生成候选并预览。
    fn generate_v2_preview(&mut self) {
        let campus = match crate::campus::store::load() {
            Ok(Some(campus)) => campus,
            Ok(None) => {
                self.run_page.v2_status = "尚未导入 campus.json，请先到「校园地图」导入".into();
                return;
            }
            Err(e) => {
                self.run_page.v2_status = format!("读取 campus.json 失败：{e}");
                return;
            }
        };
        let mut checkpoints: Vec<crate::campus::LngLat> =
            crate::api::model::load_points_cache_any()
                .unwrap_or_default()
                .iter()
                .filter_map(|point| {
                    let lat = point.get("lat")?.as_f64()?;
                    let lng = point.get("lon")?.as_f64()?;
                    Some([lng, lat])
                })
                .collect();
        // 点位缓存可能来自另一座城市/校区，和当前 campus.json 不匹配时回退到校园 POI 演示。
        let projection = crate::campus::geo::Projection::for_polygon(&campus.area);
        let cache_matches_campus = checkpoints.len() >= 2
            && checkpoints
                .iter()
                .all(|point| crate::campus::geo::point_in_polygon(*point, &campus.area, &projection));
        let source = if cache_matches_campus {
            "点位缓存"
        } else {
            checkpoints = campus
                .pois
                .all()
                .map(|(_, poi)| poi.pos)
                .collect::<Vec<_>>();
            "校园 POI（点位缓存与当前校园区域不匹配）"
        };
        if checkpoints.len() < 2 {
            self.run_page.v2_status =
                "打卡点不足 2 个：请先拉取实时点位，或在校园地图标注至少 2 个 POI".into();
            return;
        }
        let seed = (crate::crypto::envelope::now_ms() as u64) % 2_147_483_647;
        match crate::route::generate(
            &campus,
            &checkpoints,
            crate::route::RouteOptions {
                target_m: 2000.0,
                max_candidates: 12,
                seed,
            },
        ) {
            Ok(candidates) => {
                self.run_page.v2_status = format!(
                    "已生成 {} 条候选（来源：{}，目标 2000±5m）",
                    candidates.len(),
                    source
                );
                self.run_page.v2_candidates = candidates;
                self.run_page.v2_campus = Some(campus);
                self.run_page.v2_selected = 0;
            }
            Err(e) => {
                self.run_page.v2_status = format!("生成失败：{e}");
                self.run_page.v2_candidates.clear();
                self.run_page.v2_campus = Some(campus);
            }
        }
    }

    fn ensure_v3_preview_data(&mut self) {
        if self.run_page.v3_campus.is_none() {
            if let Ok(Some(campus)) = crate::campus::store::load() {
                self.run_page.v3_campus = Some(campus);
            }
        }
        let Some(_campus) = self.run_page.v3_campus.as_ref() else {
            return;
        };
        if !self.run_page.v3_checkpoints.is_empty() {
            return;
        }
        let points: Vec<crate::campus::LngLat> = crate::api::model::load_points_cache_any()
            .unwrap_or_default()
            .iter()
            .filter_map(|point| {
                let lat = point.get("lat")?.as_f64()?;
                let lng = point.get("lon")?.as_f64()?;
                Some([lng, lat])
            })
            .collect();
        if points.len() < 2 {
            self.run_page.v3_status =
                "没有服务器下发的打卡点缓存：请先登录并拉取服务器点位".into();
            self.run_page.v3_checkpoints.clear();
            return;
        }
        self.run_page.v3_checkpoints = points;
    }

    /// V3 道路优先预览：严格顺序，固定使用当前点位缓存/校园 POI。
    fn generate_v3_preview(&mut self) {
        let campus = match crate::campus::store::load() {
            Ok(Some(campus)) => campus,
            Ok(None) => {
                self.run_page.v3_status = "尚未导入 campus.json，请先到「校园地图」导入".into();
                return;
            }
            Err(e) => {
                self.run_page.v3_status = format!("读取 campus.json 失败：{e}");
                return;
            }
        };
        let checkpoints: Vec<crate::campus::LngLat> =
            crate::api::model::load_points_cache_any()
                .unwrap_or_default()
                .iter()
                .filter_map(|point| {
                    let lat = point.get("lat")?.as_f64()?;
                    let lng = point.get("lon")?.as_f64()?;
                    Some([lng, lat])
                })
                .collect();
        let projection = crate::campus::geo::Projection::for_polygon(&campus.area);
        let points_in_campus = checkpoints
            .iter()
            .all(|point| crate::campus::geo::point_in_polygon(*point, &campus.area, &projection));
        if checkpoints.len() < 2 || !points_in_campus {
            self.run_page.v3_status =
                "没有可用的服务器打卡点缓存（点位为空或不在当前校园区域内）；V3 不使用校园 POI 代替".into();
            self.run_page.v3_checkpoints = checkpoints;
            return;
        }
        if crate::route::v3::is_playground_in_campus(&campus, &checkpoints, 180.0) {
            self.run_page.v3_checkpoints = checkpoints;
            self.run_page.v3_candidates.clear();
            self.run_page.v3_status =
                "检测到操场点位：正式运行使用绕圈模式（v4 = 标准跑道几何 + 分道换道 + GPS 噪声；v3 = 过点椭圆）".into();
            return;
        }
        let seed = (crate::crypto::envelope::now_ms() as u64) % 2_147_483_647;
        self.run_page.v3_checkpoints = checkpoints.clone();
        self.run_page.v3_campus = Some(campus.clone());
        match crate::route::v3::generate(
            &campus,
            &checkpoints,
            crate::route::v3::V3Rules {
                seed,
                ..Default::default()
            },
        ) {
            Ok(candidates) => {
                self.run_page.v3_status = format!(
                    "V3 已生成 {} 条候选（严格顺序，种子 {}）",
                    candidates.len(),
                    seed
                );
                self.run_page.v3_candidates = candidates;
                self.run_page.v3_selected = 0;
            }
            Err(e) => {
                self.run_page.v3_status = format!("V3 生成失败：{e}");
                self.run_page.v3_candidates.clear();
            }
        }
    }

    fn draw_v3_preview(&mut self, ui: &mut egui::Ui) {
        self.ensure_v3_preview_data();
        ui.add_space(6.0);
        ui.separator();
        ui.strong("V3 道路优先预览（严格 P1→P5）");
        mobile::row(ui, |ui| {
            if ui.add(theme::primary_btn("运行 V3 算法")).clicked() {
                self.generate_v3_preview();
            }
            if !self.run_page.v3_status.is_empty() {
                ui.colored_label(theme::text_dim(), &self.run_page.v3_status);
            }
        });
        if let Some(campus) = self.run_page.v3_campus.as_ref() {
            let route = self
                .run_page
                .v3_candidates
                .get(self.run_page.v3_selected)
                .map(|candidate| candidate.points.as_slice());
            draw_v3_map(ui, campus, &self.run_page.v3_checkpoints, route);
        }
        if self.run_page.v3_candidates.is_empty() {
            ui.label("V3 使用道路图 + 任务接入段；无 corridor 数据时不会用自由空间补距。");
            return;
        }
        let rows: Vec<(usize, f64, f64, f64, usize, usize, f64)> = self
            .run_page
            .v3_candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                (
                    index,
                    candidate.distance_m,
                    candidate.p5_distance_m,
                    candidate.road_coverage * 100.0,
                    candidate.turns,
                    candidate.immediate_backtracks,
                    candidate.score,
                )
            })
            .collect();
        let mut select = None;
        egui::Grid::new("run_v3_candidates")
            .num_columns(7)
            .spacing([10.0, 4.0])
            .show(ui, |ui| {
                ui.strong("#");
                ui.strong("总长");
                ui.strong("P5");
                ui.strong("道路%");
                ui.strong("转向");
                ui.strong("掉头");
                ui.strong("评分");
                ui.end_row();
                for (index, total, p5, road, turns, back, score) in &rows {
                    if ui
                        .selectable_label(self.run_page.v3_selected == *index, format!("{}", index + 1))
                        .clicked()
                    {
                        select = Some(*index);
                    }
                    ui.label(format!("{total:.0}m"));
                    ui.label(format!("{p5:.0}m"));
                    ui.label(format!("{road:.1}"));
                    ui.label(format!("{turns}"));
                    ui.label(format!("{back}"));
                    ui.label(format!("{score:.1}"));
                    ui.end_row();
                }
            });
        if let Some(index) = select {
            self.run_page.v3_selected = index;
        }
        mobile::row(ui, |ui| {
            if ui.button("确认使用 v3 引擎").clicked() {
                self.run_page.route_engine = 2;
                self.config.route_engine = "v3".into();
                let _ = crate::api::model::save_config(&self.config);
                self.run_page.v3_status = "已切到 v3：单跑/批量按严格顺序生成".into();
            }
            ui.label("V3 与 V2 共用 campus.json；V1 仍可回退。");
        });
        if let Some(candidate) = self.run_page.v3_candidates.get(self.run_page.v3_selected) {
            for diagnostic in candidate.diagnostics.iter().take(3) {
                ui.label(diagnostic);
            }
        }
    }

    fn draw_v2_preview(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.separator();
        ui.strong("V2 路线预览");
        mobile::row(ui, |ui| {
            if ui.add(theme::primary_btn("生成候选路线")).clicked() {
                self.generate_v2_preview();
            }
            if !self.run_page.v2_status.is_empty() {
                ui.colored_label(theme::text_dim(), &self.run_page.v2_status);
            }
        });
        if self.run_page.v2_candidates.is_empty() {
            ui.label("预览使用最近一次点位缓存；正式提交时会用实时点位重新生成。");
            return;
        }
        let rows: Vec<(usize, f64, f64, String, usize, f64)> = self
            .run_page
            .v2_candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                (
                    index,
                    candidate.total_m,
                    candidate.tour_m,
                    candidate
                        .end_poi
                        .clone()
                        .unwrap_or_else(|| "末打卡点".into()),
                    candidate.corners,
                    candidate.curve_ratio,
                )
            })
            .collect();
        let mut select = None;
        egui::Grid::new("run_v2_candidates")
            .num_columns(6)
            .spacing([10.0, 4.0])
            .striped(true)
            .show(ui, |ui| {
                ui.strong("#");
                ui.strong("总长");
                ui.strong("巡回路");
                ui.strong("终点");
                ui.strong("转角");
                ui.strong("曲线占比");
                ui.end_row();
                for (index, total, tour, end, corners, curve) in &rows {
                    if ui
                        .selectable_label(self.run_page.v2_selected == *index, format!("{}", index + 1))
                        .clicked()
                    {
                        select = Some(*index);
                    }
                    ui.monospace(format!("{total:.1} m"));
                    ui.monospace(format!("{tour:.1} m"));
                    ui.label(end);
                    ui.monospace(corners.to_string());
                    ui.monospace(format!("{:.0}%", curve * 100.0));
                    ui.end_row();
                }
            });
        if let Some(index) = select {
            self.run_page.v2_selected = index;
        }
        mobile::row(ui, |ui| {
            if ui.button("确认使用 v2 引擎").clicked() {
                self.run_page.route_engine = 1;
                self.config.route_engine = "v2".into();
                let _ = crate::api::model::save_config(&self.config);
                self.run_page.v2_status =
                    "已切到 v2 引擎：单跑与批量都会用实时点位重新生成路网路线".into();
            }
            ui.label("确认后单跑/批量使用 v2；可随时切回 v1 回退。");
        });
        let Some(campus) = self.run_page.v2_campus.as_ref() else {
            return;
        };
        if let Some(candidate) = self.run_page.v2_candidates.get(self.run_page.v2_selected) {
            draw_route_preview(ui, campus, candidate);
        }
    }

    fn start_run(&mut self) {        // 参数变更时补齐方案；提交直接使用预计算值
        self.run_page.ensure_plan();
        let page = &mut self.run_page;
        let plan = match page.plan.clone() {
            Some(p) => p,
            None => return,
        };
        let altitude_spec = match crate::track::altitude::parse_spec(&page.manual_altitude) {
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
        let (dist, dur) = (plan.dist * 1000.0, plan.dur); // 米
        let start_ms = plan.start_ms;
        let face_check = if page.face_check { 1 } else { 0 };
        let route_engine = match page.route_engine {
            3 => crate::api::flow::RouteEngine::V4,
            2 => crate::api::flow::RouteEngine::V3,
            1 => crate::api::flow::RouteEngine::V2,
            _ => crate::api::flow::RouteEngine::V1,
        };
        self.config.dist_min = page.dist_min;
        self.config.dist_max = page.dist_max;
        self.config.pace_min = page.pace_min;
        self.config.pace_max = page.pace_max;
        self.config.face_check = page.face_check;
        self.config.route_engine = match page.route_engine {
            3 => "v4",
            2 => "v3",
            1 => "v2",
            _ => "v1",
        }
        .into();
        self.config.manual_altitude = manual_altitude;
        self.config.manual_altitude_range = manual_altitude_range;
        let _ = crate::api::model::save_config(&self.config);

        let identity = self.session_identity();
        let session = match self.session.clone() {
            Some(s) => s,
            None => {
                self.status = "请先登录".into();
                return;
            }
        };
        self.run_busy = true;
        self.status = "跑步提交中…".into();
        self.spawn_job(move |tx| {
            let mut log = App::logger(tx.clone());
            let seed = (crate::crypto::envelope::now_ms() % 2_147_483_647) as u64;
            let mut client = crate::api::client::ApiClient::new(identity, Some(session));
            let params = crate::api::flow::RunParams { dist, dur, start_ms, face_check, manual_altitude, manual_altitude_range, seed, route_engine };
            let payload = match crate::api::flow::run_full_flow(&mut client, &params, &mut log) {
                Ok(out) => {
                    log(&format!(
                        "全链完成 rrid={} obs={}/2 verify={} uuid={}",
                        out.result.rrid, out.obs_ok, out.detail_ok, out.result.uuid
                    ));
                    serde_json::json!({
                        "ok": true, "rrid": out.result.rrid,
                        "obs_ok": out.obs_ok, "verify": out.detail_ok,
                        "uuid": out.result.uuid,
                        "dist": out.result.total_dis, "dur": out.result.total_time,
                        "steps": out.result.total_steps, "avg_step_freq": out.result.avg_step_freq,
                        "calorie": out.result.calorie, "avg_power": out.result.avg_power,
                        "sel_distance": out.result.sel_distance, "start": out.result.start_ms,
                    })
                }
                Err(e) => {
                    log(&format!("跑步提交失败: {e}"));
                    serde_json::json!({ "ok": false, "message": e })
                }
            };
            tx.send(format!("__RUN_DONE__{payload}")).ok();
        });
    }
}

fn draw_v3_map(
    ui: &mut egui::Ui,
    campus: &crate::campus::Campus,
    checkpoints: &[crate::campus::LngLat],
    route: Option<&[crate::campus::LngLat]>,
) {
    let height = if mobile::compact_ui(ui) { 220.0 } else { 300.0 };
    let (response, painter) = ui.allocate_painter(
        egui::vec2(ui.available_width().max(160.0), height),
        egui::Sense::hover(),
    );
    let rect = response.rect;
    painter.rect_filled(rect, egui::Rounding::same(6.0), egui::Color32::from_rgb(250, 252, 251));
    let projection = crate::campus::geo::Projection::for_polygon(&campus.area);
    let mut min = [f64::INFINITY; 2];
    let mut max = [f64::NEG_INFINITY; 2];
    let mut push = |point: crate::campus::LngLat| {
        let xy = projection.xy(point);
        min[0] = min[0].min(xy[0]);
        min[1] = min[1].min(xy[1]);
        max[0] = max[0].max(xy[0]);
        max[1] = max[1].max(xy[1]);
    };
    for ring in campus.area.rings() {
        for point in ring {
            push(*point);
        }
    }
    for point in checkpoints {
        push(*point);
    }
    if let Some(route) = route {
        for point in route {
            push(*point);
        }
    }
    if !(min[0].is_finite() && max[0] > min[0] && max[1] > min[1]) {
        return;
    }
    let pad = 18.0_f32;
    let scale = ((rect.width() - pad * 2.0) / (max[0] - min[0]) as f32)
        .min((rect.height() - pad * 2.0) / (max[1] - min[1]) as f32)
        .max(0.0001);
    let center = [(min[0] + max[0]) / 2.0, (min[1] + max[1]) / 2.0];
    let to_screen = |point: crate::campus::LngLat| -> egui::Pos2 {
        let xy = projection.xy(point);
        egui::Pos2::new(
            rect.center().x + (xy[0] - center[0]) as f32 * scale,
            rect.center().y - (xy[1] - center[1]) as f32 * scale,
        )
    };
    let area_points: Vec<egui::Pos2> = campus.area.outer.iter().map(|p| to_screen(*p)).collect();
    painter.add(egui::Shape::closed_line(
        area_points,
        egui::Stroke::new(1.4_f32, egui::Color32::from_rgb(13, 148, 136)),
    ));
    for blocked in &campus.blocked {
        let points: Vec<egui::Pos2> = blocked.polygon.outer.iter().map(|p| to_screen(*p)).collect();
        painter.add(egui::Shape::closed_line(
            points,
            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(196, 48, 48)),
        ));
    }
    for road in &campus.roads {
        let points: Vec<egui::Pos2> = road.path.iter().map(|p| to_screen(*p)).collect();
        painter.add(egui::Shape::line(
            points,
            egui::Stroke::new(1.2_f32, egui::Color32::from_rgb(150, 150, 150)),
        ));
    }
    for (index, checkpoint) in checkpoints.iter().enumerate() {
        let center = to_screen(*checkpoint);
        let radius = (10.0 * scale).max(4.0);
        painter.circle_stroke(
            center,
            radius,
            egui::Stroke::new(1.4_f32, egui::Color32::from_rgb(20, 110, 180)),
        );
        painter.text(
            center + egui::vec2(6.0, -16.0),
            egui::Align2::LEFT_TOP,
            format!("P{}", index + 1),
            egui::FontId::proportional(12.0),
            egui::Color32::from_rgb(0, 80, 150),
        );
    }
    if let Some(route) = route {
        let points: Vec<egui::Pos2> = route.iter().map(|p| to_screen(*p)).collect();
        painter.add(egui::Shape::line(
            points.clone(),
            egui::Stroke::new(2.2_f32, egui::Color32::from_rgb(37, 99, 235)),
        ));
        if let Some(first) = points.first() {
            painter.circle_filled(*first, 5.0, egui::Color32::from_rgb(22, 130, 90));
        }
        if let Some(last) = points.last() {
            painter.circle_filled(*last, 5.0, egui::Color32::from_rgb(196, 48, 48));
        }
    }
    painter.text(
        rect.left_top() + egui::vec2(8.0, 8.0),
        egui::Align2::LEFT_TOP,
        "V3：P1–P5 到达圆 / 道路 / 候选路线",
        egui::FontId::proportional(12.0),
        egui::Color32::from_rgb(80, 90, 90),
    );
}

/// 阶段 5 预览画布：校园区域/禁行区 + 候选路线。
fn draw_route_preview(
    ui: &mut egui::Ui,
    campus: &crate::campus::Campus,
    candidate: &crate::route::RouteCandidate,
) {
    let height = if mobile::compact_ui(ui) { 220.0 } else { 300.0 };
    let (response, painter) = ui.allocate_painter(
        egui::vec2(ui.available_width().max(160.0), height),
        egui::Sense::hover(),
    );
    let rect = response.rect;
    painter.rect_filled(rect, egui::Rounding::same(6.0), egui::Color32::from_rgb(250, 252, 251));
    let projection = crate::campus::geo::Projection::for_polygon(&campus.area);
    let mut min = [f64::INFINITY; 2];
    let mut max = [f64::NEG_INFINITY; 2];
    let mut push = |point: [f64; 2]| {
        min[0] = min[0].min(point[0]);
        min[1] = min[1].min(point[1]);
        max[0] = max[0].max(point[0]);
        max[1] = max[1].max(point[1]);
    };
    for ring in campus.area.rings() {
        for point in ring {
            push(projection.xy(*point));
        }
    }
    for point in &candidate.points {
        push(projection.xy(*point));
    }
    if !(min[0].is_finite() && max[0] > min[0] && max[1] > min[1]) {
        return;
    }
    let pad = 18.0_f32;
    let scale = ((rect.width() - pad * 2.0) / (max[0] - min[0]) as f32)
        .min((rect.height() - pad * 2.0) / (max[1] - min[1]) as f32)
        .max(0.0001);
    let center = [(min[0] + max[0]) / 2.0, (min[1] + max[1]) / 2.0];
    let to_screen = |point: crate::campus::LngLat| -> egui::Pos2 {
        let xy = projection.xy(point);
        egui::Pos2::new(
            rect.center().x + (xy[0] - center[0]) as f32 * scale,
            rect.center().y - (xy[1] - center[1]) as f32 * scale,
        )
    };
    let area_color = egui::Color32::from_rgb(13, 148, 136);
    let area_points: Vec<egui::Pos2> = campus.area.outer.iter().map(|p| to_screen(*p)).collect();
    painter.add(egui::Shape::closed_line(
        area_points,
        egui::Stroke::new(1.4_f32, area_color),
    ));
    for blocked in &campus.blocked {
        let points: Vec<egui::Pos2> = blocked.polygon.outer.iter().map(|p| to_screen(*p)).collect();
        painter.add(egui::Shape::closed_line(
            points,
            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(196, 48, 48)),
        ));
    }
    // 预览中始终标出打卡点，避免只看路线时误以为点位消失。
    for (index, checkpoint) in candidate.checkpoints.iter().enumerate() {
        let center = to_screen(*checkpoint);
        painter.circle_stroke(
            center,
            (10.0 * scale).max(4.0),
            egui::Stroke::new(1.4_f32, egui::Color32::from_rgb(20, 110, 180)),
        );
        painter.text(
            center + egui::vec2(6.0, -16.0),
            egui::Align2::LEFT_TOP,
            format!("P{}", index + 1),
            egui::FontId::proportional(12.0),
            egui::Color32::from_rgb(0, 80, 150),
        );
    }
    let route_points: Vec<egui::Pos2> = candidate.points.iter().map(|p| to_screen(*p)).collect();
    painter.add(egui::Shape::line(
        route_points.clone(),
        egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(37, 99, 235)),
    ));
    if let Some(first) = route_points.first() {
        painter.circle_filled(*first, 5.0, egui::Color32::from_rgb(22, 130, 90));
    }
    if let Some(last) = route_points.last() {
        painter.circle_filled(*last, 5.0, egui::Color32::from_rgb(196, 48, 48));
    }
    painter.text(
        rect.left_top() + egui::vec2(8.0, 8.0),
        egui::Align2::LEFT_TOP,
        format!(
            "{:.0} m · 巡回路 {:.0} m · 终点 {} · 转角 {} · 直/曲 {:.0}/{:.0}%",
            candidate.total_m,
            candidate.tour_m,
            candidate.end_poi.as_deref().unwrap_or("末打卡点"),
            candidate.corners,
            candidate.straight_ratio * 100.0,
            candidate.curve_ratio * 100.0
        ),
        egui::FontId::proportional(11.0),
        theme::plain(),
    );
}

#[cfg(test)]
mod tests {    use super::*;

    /// 一份参数合理的页面状态（配速 6:00/km，距离 2 km 附近）。
    fn page(start_mode: usize, days_ago: i64) -> RunPage {
        RunPage {
            dist_min: 2.0,
            dist_max: 2.2,
            pace_min: 350.0,
            pace_max: 370.0,
            manual_altitude: String::new(),
            start_mode,
            days_ago,
            hour: 12,
            minute: 0,
            face_check: false,
            route_engine: 3,
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
        }
    }

    /// 开跑时刻本身不得落在未来（两种模式都成立）。
    fn assert_start_not_future(p: &RunPlan) {
        let now = crate::crypto::envelope::now_ms();
        assert!(p.start_ms <= now, "开始时刻 {} 落在未来", p.start_ms);
    }

    /// 随机模式：整段跑步（含 flow.rs 的 0-4s 抖动）都不得越过当前时刻。
    ///
    /// 指定模式不适用：用户指定今天 12:00 而此刻已 12:05 时，原行为就是照常提交。
    fn assert_random_not_future(p: &RunPlan) {
        let now = crate::crypto::envelope::now_ms();
        assert!(
            p.start_ms + p.dur * 1000 + JITTER_MARGIN_MS <= now,
            "随机时刻 {} + 用时 {}s 越过当前时刻",
            p.start_ms,
            p.dur
        );
    }

    /// 指定模式下「换一版」只换运动量，用户填的时刻必须原样保留。
    #[test]
    fn shuffle_keeps_user_time_in_specified_mode() {
        let mut p = page(1, 1);
        p.hour = 9;
        p.minute = 15;
        p.ensure_plan();
        for _ in 0..40 {
            p.regen_plan();
            let plan = p.plan.clone().unwrap();
            assert_eq!((p.hour, p.minute), (9, 15), "「换一版」改动了用户填的时刻");
            assert_eq!((plan.hour, plan.minute), (9, 15), "方案时刻与输入框不一致");
        }
    }

    /// 指定模式下「换一版」仍应换掉距离/配速（否则按钮看起来没反应）。
    #[test]
    fn shuffle_changes_shape_in_specified_mode() {
        let mut p = page(1, 1);
        p.hour = 9;
        p.minute = 15;
        p.ensure_plan();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..40 {
            p.regen_plan();
            seen.insert(p.plan.as_ref().unwrap().dist.to_bits());
        }
        assert!(seen.len() > 1, "「换一版」40 次仍未改变距离");
    }

    /// 日期由用户决定，「换一版」不应改动它。
    #[test]
    fn shuffle_keeps_days_ago_in_specified_mode() {
        let mut p = page(1, 2);
        for _ in 0..20 {
            p.regen_plan();
            assert_eq!(p.days_ago, 2);
            assert_eq!(p.plan.as_ref().unwrap().days_ago, 2);
        }
    }

    /// 随机模式：时刻应落在该日 7:00-20:00 内，且「换一版」每次都换新的。
    #[test]
    fn random_mode_within_window_and_varies() {
        let mut p = page(0, 1);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..40 {
            p.regen_plan();
            let plan = p.plan.clone().unwrap();
            let t = Local.timestamp_millis_opt(plan.start_ms).single().unwrap();
            assert!(
                (RAND_HOUR_LO..=RAND_HOUR_HI).contains(&t.hour()),
                "随机时刻 {}:{} 越出 {RAND_HOUR_LO}:00-{RAND_HOUR_HI}:00",
                t.hour(),
                t.minute()
            );
            seen.insert((t.hour(), t.minute()));
        }
        assert!(seen.len() > 1, "随机模式 40 次仍未改变时刻");
    }

    /// 任何模式下开始时刻都不得落在未来；随机模式还要求整段跑步不越过当前时刻。
    #[test]
    fn never_schedules_future_start() {
        for mode in [0, 1] {
            for days_ago in 0..=3 {
                let mut p = page(mode, days_ago);
                for _ in 0..25 {
                    p.regen_plan();
                    assert_start_not_future(p.plan.as_ref().unwrap());
                    if mode == 0 {
                        assert_random_not_future(p.plan.as_ref().unwrap());
                    }
                }
                // 改参数走 ensure_plan 的路径同样成立
                p.dist_max = 5.0;
                p.pace_max = 500.0;
                p.ensure_plan();
                let plan = p.plan.as_ref().unwrap();
                assert_start_not_future(plan);
                if mode == 0 {
                    assert_random_not_future(plan);
                }
            }
        }
    }

    /// 今天 + 已过去的指定时刻：按原时刻提交，不动它。
    #[test]
    fn specified_today_past_hour_is_kept() {
        let mut p = page(1, 0);
        p.regen_plan();
        // 用户在界面上填时刻（regen_plan 之后填，才是用户输入而非抽样结果）
        p.hour = 0;
        p.minute = 30;
        p.ensure_plan();
        let plan = p.plan.clone().unwrap();
        let today = Local::now().date_naive();
        let want = Local
            .with_ymd_and_hms(today.year(), today.month(), today.day(), 0, 30, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        // 凌晨 0:30 尚未到达时会被钳制，跳过断言
        if want <= crate::crypto::envelope::now_ms() {
            assert_eq!(plan.start_ms, want, "已过去的指定时刻不应被改动");
        }
        assert_start_not_future(&plan);
    }

    /// 未来的指定时刻被钳制到当前时刻；已过去的时刻原样保留。
    ///
    /// 断言直接对齐契约 `min(填入时刻, 现在)`，与运行时刻无关（23:59 跑也不会误报）。
    #[test]
    fn specified_time_is_clamped_to_now() {
        let now = Local::now();
        let mut p = page(1, 0);
        p.hour = 23;
        p.minute = 59;
        p.regen_plan();
        let plan = p.plan.clone().unwrap();

        let today = now.date_naive();
        let want = Local
            .with_ymd_and_hms(today.year(), today.month(), today.day(), 23, 59, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        let now_ms = crate::crypto::envelope::now_ms();
        assert_eq!(plan.start_ms, want.min(now_ms), "指定时刻未按 min(填入, 现在) 处理");
        assert_start_not_future(&plan);
    }

    /// 改距离/配速只重抽运动量，不应打乱用户指定的时刻。
    #[test]
    fn shape_change_keeps_specified_time() {
        let mut p = page(1, 1);
        p.regen_plan();
        // 用户在界面上填时刻
        p.hour = 9;
        p.minute = 15;
        p.ensure_plan();
        let before = p.plan.clone().unwrap();
        assert_eq!((before.hour, before.minute), (9, 15));

        p.dist_min = 3.0;
        p.dist_max = 3.5;
        p.ensure_plan();
        let after = p.plan.clone().unwrap();
        assert_ne!((after.dist, after.pace), (before.dist, before.pace), "运动量未重抽");
        assert_eq!(after.start_ms, before.start_ms, "改距离不应改动指定时刻");
        assert_eq!((p.hour, p.minute), (9, 15));
    }

    /// 改小时/分钟应立刻反映到方案，且不重抽运动量。
    #[test]
    fn editing_hhmm_resyncs_plan() {
        let mut p = page(1, 1);
        p.regen_plan();
        let before = p.plan.clone().unwrap();

        p.hour = 8;
        p.minute = 45;
        p.ensure_plan();
        let after = p.plan.clone().unwrap();
        assert_eq!((after.hour, after.minute), (8, 45));
        assert_eq!((after.dist, after.pace, after.dur), (before.dist, before.pace, before.dur));
        assert_start_not_future(&after);
    }

    /// 切换模式应立即换掉时刻，且不重抽运动量。
    #[test]
    fn mode_switch_resyncs_time_only() {
        let mut p = page(1, 1);
        p.regen_plan();
        let before = p.plan.clone().unwrap();

        p.start_mode = 0;
        p.ensure_plan();
        let after = p.plan.clone().unwrap();
        assert_eq!(after.start_mode, 0);
        assert_eq!((after.dist, after.pace, after.dur), (before.dist, before.pace, before.dur));
        assert_random_not_future(&after);
    }

    /// 日期下拉作用于两种模式。
    #[test]
    fn date_change_applies_to_both_modes() {
        for mode in [0, 1] {
            let mut p = page(mode, 0);
            p.regen_plan();
            p.days_ago = 3;
            p.ensure_plan();
            let plan = p.plan.clone().unwrap();
            assert_eq!(plan.days_ago, 3);
            let want = (Local::now() - Duration::days(3)).date_naive();
            let t = Local.timestamp_millis_opt(plan.start_ms).single().unwrap();
            assert_eq!(t.date_naive(), want, "mode={mode} 未落到 3 天前");
        }
    }

    /// 日期下拉文本。
    #[test]
    fn days_ago_labels() {
        assert_eq!(days_ago_label(0), "今天");
        assert_eq!(days_ago_label(1), "昨天");
        assert_eq!(days_ago_label(3), "3 天前");
    }

    /// 同一帧内「距离 + 日期」一起改：两处变化都不能漏。
    #[test]
    fn simultaneous_shape_and_date_change() {
        let mut p = page(0, 0);
        p.regen_plan();
        p.dist_min = 4.0;
        p.dist_max = 4.5;
        p.days_ago = 2;
        p.ensure_plan();
        let plan = p.plan.clone().unwrap();
        assert_eq!(plan.days_ago, 2, "日期变化被漏掉");
        assert!(plan.dist >= 4.0, "距离变化被漏掉");
        let want = (Local::now() - Duration::days(2)).date_naive();
        let t = Local.timestamp_millis_opt(plan.start_ms).single().unwrap();
        assert_eq!(t.date_naive(), want);
        assert_random_not_future(&plan);
    }

    /// 同一帧内「时刻 + 距离」一起改。
    #[test]
    fn simultaneous_shape_and_hhmm_change() {
        let mut p = page(1, 1);
        p.regen_plan();
        p.dist_min = 4.0;
        p.dist_max = 4.5;
        p.hour = 8;
        p.minute = 20;
        p.ensure_plan();
        let plan = p.plan.clone().unwrap();
        assert_eq!((plan.hour, plan.minute), (8, 20), "时刻变化被漏掉");
        assert!(plan.dist >= 4.0, "距离变化被漏掉");
        assert_eq!(plan.start_ms, specified_time(1, 8, 20));
        assert_start_not_future(&plan);
    }

    /// 今天 + 指定时刻且该时刻已过：照常提交，不再回退（用户指定的时刻说了算）。
    #[test]
    fn specified_past_time_is_committed_as_is() {
        let now = Local::now();
        // 取一个今天已过去足够久的整点，保证整段用时都已结束
        let h = now.hour().saturating_sub(3) as i64;
        let mut p = page(1, 0);
        p.hour = h;
        p.minute = 0;
        p.regen_plan();
        let plan = p.plan.clone().unwrap();
        let today = now.date_naive();
        let want = Local
            .with_ymd_and_hms(today.year(), today.month(), today.day(), h as u32, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        assert_eq!(plan.start_ms, want, "用户填的过去时刻被改动了");
        assert_start_not_future(&plan);
    }

    /// 明天（未来）的指定时刻必然被钳制，且界面可据此提示。
    ///
    /// 用「未来某天」构造，避免依赖当前钟点（23:xx 跑也不会失效）。
    #[test]
    fn future_specified_time_is_clamped_and_detectable() {
        // days_ago 取负即未来，但会被 clamp 到 0；这里直接构造今天最后一刻
        let mut p = page(1, 0);
        p.hour = 23;
        p.minute = 59;
        p.ensure_plan();
        let plan = p.plan.clone().unwrap();

        let now = Local::now();
        let today = now.date_naive();
        let typed = Local
            .with_ymd_and_hms(today.year(), today.month(), today.day(), 23, 59, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        let now_ms = crate::crypto::envelope::now_ms();
        assert_eq!(plan.start_ms, typed.min(now_ms));
        assert_start_not_future(&plan);
        // 界面提示条件：填入时刻晚于现在（23:59 总成立，除非正好在那一分钟）
        if typed > now_ms {
            assert!(plan.start_ms < typed, "未来时刻未被钳制");
        }
    }

    /// 反复调用 ensure_plan 应当是幂等的（不改动任何已定值）。
    #[test]
    fn ensure_plan_is_idempotent() {
        for mode in [0, 1] {
            let mut p = page(mode, 1);
            p.regen_plan();
            let first = p.plan.clone().unwrap();
            for _ in 0..5 {
                p.ensure_plan();
            }
            let again = p.plan.clone().unwrap();
            assert_eq!(
                (again.dist, again.pace, again.dur, again.start_ms),
                (first.dist, first.pace, first.dur, first.start_ms),
                "mode={mode} ensure_plan 不稳定"
            );
        }
    }

    /// 随机模式抽样结果应落在所选日期的窗口内；凌晨选「今天」时窗口未到，
    /// 此时允许退化，但必须能被 random_window_ok 识别出来（界面据此提示）。
    #[test]
    fn random_window_detection() {
        let mut p = page(0, 1);
        p.regen_plan();
        assert!(p.random_window_ok(), "昨天的随机时刻应当落在窗口内");

        // 构造一个「今天但早于 7:00」的方案，窗口判定应为 false
        let now = Local::now();
        let early = Local
            .with_ymd_and_hms(now.year(), now.month(), now.day(), 3, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        p.days_ago = 0;
        if let Some(plan) = p.plan.as_mut() {
            plan.days_ago = 0;
            plan.start_ms = early;
        }
        assert!(!p.random_window_ok(), "今天 03:00 不应被判为在 7:00-20:00 窗口内");
    }
}
