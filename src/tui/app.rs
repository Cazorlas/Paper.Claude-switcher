use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::DefaultTerminal;
use tokio::sync::Semaphore;

use crate::auth;
use crate::cache;
use crate::claude_usage::AccountInfo;
use crate::output::format_local_timestamp;
use crate::profile::{
    cmd_delete, list_profiles, profile_auth_path, read_current, rename_profile,
    switch_profile, sync_current_from_live, validate_alias,
};
use crate::usage::{
    Refresh, UsageError, UsageInfo, fetch_usage_retried,
    fetch_usage_retried_force, fetch_usage_retried_unattended,
};

async fn with_usage_limiter<T>(limiter: &Semaphore, operation: impl Future<Output = T>) -> T {
    let _permit = limiter
        .acquire()
        .await
        .expect("TUI usage limiter remains open for the app lifetime");
    operation.await
}

#[derive(Debug, Clone)]
pub struct AccountEntry {
    pub alias: String,
    pub info: AccountInfo,
    pub usage: UsageStatus,
    pub is_current: bool,
}

#[derive(Debug, Clone)]
pub enum UsageStatus {
    Idle,
    Loading,
    Loaded(Box<UsageInfo>),
    Error(UsageError),
}

#[cfg(test)]
fn retained_usage_by_alias(accounts: Vec<AccountEntry>) -> HashMap<String, UsageStatus> {
    accounts
        .into_iter()
        .map(|account| (account.alias, account.usage))
        .collect()
}

fn refresh_fetches_loaded_usage(refresh: Refresh) -> bool {
    !matches!(refresh, Refresh::Cached)
}

fn refresh_priority(refresh: Refresh) -> u8 {
    match refresh {
        Refresh::Cached => 0,
        Refresh::Unattended => 1,
        Refresh::Forced => 2,
    }
}

#[derive(Debug)]
enum SwitchCompletion {
    Succeeded {
        alias: String,
        current: String,
        last_used_error: Option<String>,
    },
    Failed {
        alias: String,
        error: String,
    },
}

fn wrap_account_detail_line(line: String) -> Vec<String> {
    const MAX_WIDTH: usize = 68;
    if line.chars().count() <= MAX_WIDTH {
        return vec![line];
    }
    let indent = "    ";
    let mut remaining = line.as_str();
    let mut wrapped = Vec::new();
    while remaining.chars().count() > MAX_WIDTH {
        let split = remaining
            .char_indices()
            .take(MAX_WIDTH + 1)
            .filter(|(_, ch)| ch.is_whitespace() || matches!(ch, '·' | ','))
            .map(|(index, _)| index)
            .last()
            .unwrap_or_else(|| {
                remaining
                    .char_indices()
                    .nth(MAX_WIDTH)
                    .map(|(index, _)| index)
                    .unwrap_or(remaining.len())
            });
        let (head, tail) = remaining.split_at(split);
        wrapped.push(head.trim_end().to_string());
        remaining = tail.trim_start_matches(|ch: char| ch.is_whitespace() || ch == '·');
    }
    if !remaining.is_empty() {
        wrapped.push(format!("{indent}{remaining}"));
    }
    wrapped
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Name,
    Quota,
    Status,
}

impl SortMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            SortMode::Name => "name",
            SortMode::Quota => "quota",
            SortMode::Status => "status",
        }
    }
}

pub enum ConfirmAction {
    DiscardSettings,
    Delete(String),
    BatchDelete(Vec<String>),
    /// Use one reset of the grant `label`; `resets_left` and `ends_at` are shown in the prompt.
    UseReset {
        alias: String,
        label: String,
        resets_left: u32,
        ends_at: Option<String>,
    },
}

/// Starts a claim for (alias, request id). Injectable so tests never reach the real endpoint.
pub type ResetClaimer = Arc<dyn Fn(String, String) -> ClaimFuture + Send + Sync>;

pub type ClaimFuture = std::pin::Pin<
    Box<
        dyn Future<
                Output = Result<crate::claude_api::ResetClaim, crate::claude_api::ClaimError>,
            > + Send,
    >,
>;

fn default_reset_claimer() -> ResetClaimer {
    Arc::new(|alias: String, request_id: String| -> ClaimFuture {
        Box::pin(async move { crate::claude_usage::claim_reset(&alias, &request_id).await })
    })
}

pub struct RenameState {
    pub old_alias: String,
    pub input: String,
    pub cursor: usize,
}

#[derive(Debug, Clone)]
pub struct SearchState {
    pub query: String,
    pub cursor: usize,
}

/// Which top-level TUI tab is active. Accounts, Settings (`config.toml`) and
/// Logs stay isolated so their key bindings never mix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Accounts,
    Settings,
    Logs,
}

pub struct App {
    pub log_writer: crate::logging::TuiLogWriter,
    pub log_scroll: u16,
    pub log_render_revision: Option<u64>,
    pub log_render_width: u16,
    pub log_visual_lines: Vec<String>,
    pub accounts: Vec<AccountEntry>,
    /// Live editor for `config.toml` (Settings tab).
    pub settings: super::settings::SettingsState,
    /// Active top-level tab.
    pub active_tab: Tab,
    pub selected: usize,
    pub search: Option<SearchState>,
    pub search_active: bool,
    pub sort_mode: SortMode,
    pub view_indices: Vec<usize>,
    pub marked: BTreeSet<String>,
    pub status_msg: Option<String>,
    pub status_is_error: bool,
    pub status_expiry: Option<Instant>,
    /// Persistent load diagnostics remain visible until that data domain loads cleanly.
    pub profile_load_error: Option<String>,
    pub refreshing_requests: HashMap<String, (u64, Refresh)>,
    pub pending_usage_refreshes: HashMap<String, Refresh>,
    pub usage_next_id: u64,
    pub pending_results: tokio::sync::mpsc::Receiver<(String, u64, Result<UsageInfo, UsageError>)>,
    pub result_sender: tokio::sync::mpsc::Sender<(String, u64, Result<UsageInfo, UsageError>)>,
    pub confirm: Option<ConfirmAction>,
    pub rename: Option<RenameState>,
    pub usage_limiter: Arc<Semaphore>,
    pub update_available: Option<String>,
    pub update_rx: Option<tokio::sync::oneshot::Receiver<String>>,
    pub auto_refresh_enabled: bool,
    pub auto_refresh_interval: Duration,
    pub next_auto_refresh: Option<Instant>,
    pub detail_visible: bool,
    pub help_popup: Option<super::popup::PopupState>,
    pub menu: Option<super::menu::MenuState>,
    /// Last list-row press used to recognize a bounded double-click.
    last_list_click: Option<(Tab, String, Instant)>,
    /// Regions from the last drawn frame, used for mouse hit-testing.
    pub hitmap: super::hitmap::HitMap,
    pending_switches: tokio::sync::mpsc::Receiver<SwitchCompletion>,
    switch_sender: tokio::sync::mpsc::Sender<SwitchCompletion>,
    switching_alias: Option<String>,
    /// Starts a reset claim; replaced by a fake in every test.
    pub claimer: ResetClaimer,
    /// Accounts with a reset claim on its way.
    pub reset_in_flight: BTreeSet<String>,
    /// Accounts whose last claim may or may not have used a reset; no new claim
    /// until a fresh usage reading of the account arrives.
    reset_unknown: BTreeSet<String>,
    /// Request id of each claim without a definite outcome. A retry reuses it,
    /// so the server cannot spend two resets for one decision.
    reset_request_ids: HashMap<String, String>,
    pending_resets: tokio::sync::mpsc::Receiver<ResetClaimDone>,
    reset_sender: tokio::sync::mpsc::Sender<ResetClaimDone>,
}

type ResetClaimDone = (
    String,
    Result<crate::claude_api::ResetClaim, crate::claude_api::ClaimError>,
);

impl App {
    pub fn new() -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(128);
        let (switch_tx, switch_rx) = tokio::sync::mpsc::channel(4);
        let (reset_tx, reset_rx) = tokio::sync::mpsc::channel(16);
        let cfg = crate::config::get();
        App {
            log_writer: crate::logging::tui_log_writer(),
            log_scroll: 0,
            log_render_revision: None,
            log_render_width: 0,
            log_visual_lines: vec!["No logs in this session.".to_string()],
            accounts: vec![],
            settings: super::settings::SettingsState::from_config(cfg.clone()),
            active_tab: Tab::default(),
            selected: 0,
            search: None,
            search_active: false,
            sort_mode: SortMode::Name,
            view_indices: vec![],
            marked: BTreeSet::new(),
            status_msg: None,
            status_is_error: false,
            status_expiry: None,
            profile_load_error: None,
            refreshing_requests: HashMap::new(),
            pending_usage_refreshes: HashMap::new(),
            usage_next_id: 0,
            pending_results: rx,
            result_sender: tx,
            confirm: None,
            rename: None,
            usage_limiter: Arc::new(Semaphore::new(cfg.network.max_concurrent)),
            update_available: None,
            update_rx: None,
            auto_refresh_enabled: false,
            auto_refresh_interval: Duration::from_secs(cfg.tui.auto_refresh_interval_secs),
            next_auto_refresh: None,
            detail_visible: true,
            help_popup: None,
            menu: None,
            last_list_click: None,
            hitmap: super::hitmap::HitMap::default(),
            pending_switches: switch_rx,
            switch_sender: switch_tx,
            switching_alias: None,
            claimer: default_reset_claimer(),
            reset_in_flight: BTreeSet::new(),
            reset_unknown: BTreeSet::new(),
            reset_request_ids: HashMap::new(),
            pending_resets: reset_rx,
            reset_sender: reset_tx,
        }
    }

    pub fn open_help(&mut self) {
        self.help_popup = Some(super::popup::PopupState::new());
    }

    pub fn close_help(&mut self) {
        self.help_popup = None;
    }

    pub fn open_account_menu(&mut self) {
        let Some(account_idx) = self.selected_account_idx() else {
            return;
        };
        let entry = &self.accounts[account_idx];
        let loaded_usage = match &entry.usage {
            UsageStatus::Loaded(u) => Some(u.as_ref()),
            _ => None,
        };
        let plan = loaded_usage
            .and_then(|u| u.plan_type.as_deref())
            .or(entry.info.plan_type.as_deref());
        let usage_meta: Vec<String> = loaded_usage
            .map(|usage| {
                let mut items = Vec::new();
                if usage.account_limited || usage.rate_limit_reached_type.is_some() {
                    let reason = usage
                        .rate_limit_reached_type
                        .as_deref()
                        .map(|value| format!(" · {}", value.replace(['_', '-'], " ")))
                        .unwrap_or_default();
                    items.push(format!("  Status  limited{reason}"));
                }

                if let Some(limit) = &usage.individual_limit {
                    let mut parts = vec!["  Monthly API".to_string()];
                    if let Some(value) = &limit.limit {
                        parts.push(format!("{value} total"));
                    }
                    if let Some(value) = &limit.used {
                        parts.push(format!("{value} used"));
                    }
                    if let Some(value) = &limit.remaining {
                        parts.push(format!("{value} remaining"));
                    }
                    if let Some(value) = limit.remaining_percent {
                        parts.push(format!("{value:.0}% left"));
                    }
                    if let Some(value) = limit.resets_at {
                        parts.push(format!("resets {}", format_local_timestamp(value)));
                    }
                    if parts.len() > 1 {
                        items.push(parts.join(" · "));
                    }
                }
                items
            })
            .unwrap_or_default()
            .into_iter()
            .flat_map(wrap_account_detail_line)
            .collect();
        let auth_expiries = profile_auth_path(&entry.alias)
            .ok()
            .and_then(|path| auth::read_auth(&path).ok())
            .map(|credentials| {
                let mut expiries = Vec::new();
                if let Some(millis) = credentials["claudeAiOauth"]["expiresAt"].as_i64() {
                    let expiry = crate::output::format_token_expiry(millis / 1000);
                    expiries.push(format!("Access token · {expiry}"));
                }
                expiries
            })
            .unwrap_or_default();
        self.menu = Some(super::menu::MenuState::account(
            super::menu::AccountMenuInfo {
                alias: entry.alias.clone(),
                email: entry.info.email.clone(),
                account_id: entry.info.account_id.clone(),
                plan_label: entry.info.plan_label_with(plan),
                plan_type: plan.map(str::to_string),
                is_current: entry.is_current,
                auth_expiries,
                usage: loaded_usage.cloned().map(Box::new),
                usage_meta,
            },
        ));
    }

    pub fn open_batch_menu(&mut self) {
        let count = self.marked.len();
        if count == 0 {
            return;
        }
        self.menu = Some(super::menu::MenuState::batch(count));
    }

    pub fn open_add_menu(&mut self) {
        if self.defer_while_switching("adding an account") {
            return;
        }
        self.menu = Some(super::menu::MenuState::add());
    }

    /// Cycle Accounts → Providers → Settings → Logs → Accounts (`Tab`), or reverse (`BackTab`).
    /// Entering Settings reloads `config.toml` from disk unless the form has
    /// unsaved edits.
    pub fn cycle_tab(&mut self, forward: bool) {
        let next = if forward {
            match self.active_tab {
                Tab::Accounts => Tab::Settings,
                Tab::Settings => Tab::Logs,
                Tab::Logs => Tab::Accounts,
            }
        } else {
            match self.active_tab {
                Tab::Accounts => Tab::Logs,
                Tab::Settings => Tab::Accounts,
                Tab::Logs => Tab::Settings,
            }
        };
        self.select_tab(next);
    }

    /// Switch to `tab` (no-op if already there). Reloads Settings from disk
    /// when entering that tab without unsaved edits.
    pub fn select_tab(&mut self, tab: Tab) {
        if self.active_tab == tab {
            return;
        }
        self.active_tab = tab;
        self.status_msg = None;
        if self.active_tab == Tab::Settings && !self.settings.is_dirty() {
            let cfg = crate::config::load_current().unwrap_or_else(|_| crate::config::get());
            self.settings = super::settings::SettingsState::from_config(cfg);
        }
    }

    /// Handle a mouse event against the last frame's hit map.
    ///
    /// Scope: wheel scroll on logs/help/menus/settings/modal lists; left-click
    /// tabs, list rows, settings fields, and modal form controls. Click outside
    /// dismissible overlays closes them. Modal overlays do not click through.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> Option<KeyCode> {
        use super::hitmap::{HitMap, OverlayHit};

        let col = mouse.column;
        let row = mouse.row;

        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                self.last_list_click = None;
                let down = matches!(mouse.kind, MouseEventKind::ScrollDown);
                match self.hitmap.overlay {
                    OverlayHit::Modal => {
                        if let Some(panel) = self.hitmap.overlay_panel
                            && !HitMap::contains(panel, col, row)
                        {
                            return None;
                        }

                    }
                    OverlayHit::Dismissible { panel } => {
                        if !HitMap::contains(panel, col, row) {
                            return None;
                        }
                        if self.help_popup.is_some() {
                            if let Some(state) = self.help_popup.as_mut() {
                                if down {
                                    state.scroll_down(u16::MAX);
                                } else {
                                    state.scroll_up();
                                }
                            }
                        } else if let Some(menu) = self.menu.as_mut() {
                            menu.handle_key(if down { KeyCode::Down } else { KeyCode::Up });
                        }
                    }
                    OverlayHit::None => {
                        if self.active_tab == Tab::Logs
                            && self
                                .hitmap
                                .logs
                                .is_some_and(|area| HitMap::contains(area, col, row))
                        {
                            if down {
                                self.log_scroll = self.log_scroll.saturating_sub(1);
                            } else {
                                self.log_scroll = self.log_scroll.saturating_add(1);
                            }
                        } else if self.active_tab == Tab::Settings
                            && self
                                .hitmap
                                .settings_body
                                .is_some_and(|area| HitMap::contains(area, col, row))
                        {
                            self.settings.handle_wheel(down);
                        }
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Left) => match self.hitmap.overlay {
                OverlayHit::Modal => {
                    self.last_list_click = None;
                    if let Some(click) = self.hitmap.overlay_click_at(col, row) {
                        return self.apply_overlay_click(click);
                    }
                    if self.settings.is_editing()
                        && let Some(index) = self.hitmap.settings_field_at(col, row)
                    {
                        self.settings.click_field(index);
                    }
                }
                OverlayHit::Dismissible { panel } => {
                    self.last_list_click = None;
                    if self.menu.is_some()
                        && let Some(key) = self.hitmap.menu_action_at(col, row)
                    {
                        return Some(key);
                    }
                    if !HitMap::contains(panel, col, row) {
                        if self.help_popup.is_some() {
                            self.close_help();
                        } else if self.menu.is_some() {
                            self.close_menu();
                        }
                    }
                }
                OverlayHit::None => {
                    if let Some(code) = self.hitmap.footer_action_at(col, row) {
                        self.last_list_click = None;
                        return Some(code);
                    }
                    if let Some(tab) = self.hitmap.tab_at(col, row) {
                        self.last_list_click = None;
                        self.select_tab(tab);
                        return None;
                    }
                    match self.active_tab {
                        Tab::Accounts => {
                            if let Some(list) = self.hitmap.account_list.as_ref()
                                && let Some(idx) = HitMap::list_index_at(list, col, row)
                                && let Some(account_idx) = self.view_indices.get(idx).copied()
                                && let Some(alias) = self
                                    .accounts
                                    .get(account_idx)
                                    .map(|account| account.alias.clone())
                            {
                                self.selected = idx;
                                let now = Instant::now();
                                let double = self.last_list_click.as_ref().is_some_and(
                                    |(tab, previous, at)| {
                                        *tab == Tab::Accounts
                                            && previous == &alias
                                            && now.duration_since(*at) <= Duration::from_millis(500)
                                    },
                                );
                                self.last_list_click =
                                    (!double).then_some((Tab::Accounts, alias, now));
                                if double {
                                    self.open_account_menu();
                                }
                            } else {
                                self.last_list_click = None;
                            }
                        }

                        Tab::Settings => {
                            if let Some(index) = self.hitmap.settings_field_at(col, row) {
                                self.settings.click_field(index);
                            }
                            self.last_list_click = None;
                        }
                        Tab::Logs => self.last_list_click = None,
                    }
                }
            },
            _ => {}
        }
        None
    }

    fn apply_overlay_click(&mut self, click: super::hitmap::OverlayClick) -> Option<KeyCode> {
        match click { super::hitmap::OverlayClick::Key(code) => Some(code) }
    }

    fn rebuild_open_account_menu(&mut self) {
        let scroll = match self.menu.as_ref() {
            Some(super::menu::MenuState::Account { popup, .. }) => popup.scroll,
            _ => return,
        };
        self.open_account_menu();
        if let Some(super::menu::MenuState::Account { popup, .. }) = self.menu.as_mut() {
            popup.scroll = scroll;
        }
    }

    /// Handle synchronous Accounts-list keys. Returns a selected alias when
    /// the caller must perform the terminal-backed launch action.
    pub fn handle_accounts_key(&mut self, code: KeyCode) -> Option<String> {
        self.last_list_click = None;
        match code {
            KeyCode::Esc => {
                if self.search.is_some() {
                    self.search = None;
                    self.update_view();
                } else if !self.marked.is_empty() {
                    self.clear_marks();
                }
            }
            KeyCode::Down | KeyCode::Char('j') if self.selected + 1 < self.view_indices.len() => {
                self.selected += 1;
            }
            KeyCode::Up | KeyCode::Char('k') if self.selected > 0 => {
                self.selected -= 1;
            }
            KeyCode::Enter => {
                if self.marked.is_empty() {
                    self.open_account_menu();
                } else {
                    self.open_batch_menu();
                }
            }
            KeyCode::Char('a') => self.open_add_menu(),
            KeyCode::Char('o') if self.marked.is_empty() => {
                if self.defer_while_switching("launching Claude Code") {
                    return None;
                }
                return self.selected_account_idx().map(|idx| self.accounts[idx].alias.clone());
            }
            KeyCode::Char('u') if self.marked.is_empty() => self.switch_selected(),
            KeyCode::Char('r') => self.refresh(Refresh::Forced),
            KeyCode::Char('t') => self.toggle_auto_refresh(),
            KeyCode::Char('i') => self.toggle_detail_panel(),
            KeyCode::Char('s') => self.cycle_sort(),
            KeyCode::Char(' ') => self.toggle_mark(),
            KeyCode::Char('/') => {
                if let Some(search) = &mut self.search {
                    search.cursor = search.query.chars().count();
                } else {
                    self.search = Some(SearchState {
                        query: String::new(),
                        cursor: 0,
                    });
                    self.update_view();
                }
                self.search_active = true;
            }
            _ => {}
        }
        None
    }

    pub fn handle_settings_key(&mut self, code: KeyCode) {
        match self.settings.handle_key(code) {
            super::settings::SettingsOutcome::Continue => {}
            super::settings::SettingsOutcome::Saved { message } => {
                self.apply_saved_settings();
                self.set_status(message, 8);
            }
        }
    }

    fn apply_saved_settings(&mut self) {
        let cfg = crate::config::get();
        self.auto_refresh_interval = Duration::from_secs(cfg.tui.auto_refresh_interval_secs.max(1));
        self.usage_limiter = Arc::new(Semaphore::new(cfg.network.max_concurrent.max(1)));
    }

    pub fn open_relogin_flow_menu(&mut self, alias: String, email: Option<String>) {
        if self.defer_while_switching("re-logging in") {
            return;
        }
        self.menu = Some(super::menu::MenuState::relogin_flow(alias, email));
    }

    pub fn close_menu(&mut self) {
        self.menu = None;
    }

    /// Request delete confirmation for a specific alias (called from menu).
    pub fn request_delete_alias(&mut self, alias: &str) {
        if self.defer_while_switching("deleting an account") {
            return;
        }
        let Some(entry) = self.accounts.iter().find(|a| a.alias == alias) else {
            return;
        };
        if entry.is_current {
            self.set_status_error("Cannot delete the active profile".to_string(), 3);
            return;
        }
        self.confirm = Some(ConfirmAction::Delete(entry.alias.clone()));
    }

    /// Begin rename for a specific alias (called from menu).
    pub fn start_rename_alias(&mut self, alias: &str) {
        if self.defer_while_switching("renaming an account") {
            return;
        }
        let Some(entry) = self.accounts.iter().find(|a| a.alias == alias) else {
            return;
        };
        let old = entry.alias.clone();
        let len = old.len();
        self.rename = Some(RenameState {
            old_alias: old.clone(),
            input: old,
            cursor: len,
        });
    }

    pub fn load_profiles(&mut self) -> bool {
        let mut account_problems = Vec::new();
        let previous_selected_alias = self
            .selected_account_idx()
            .and_then(|idx| self.accounts.get(idx))
            .map(|account| account.alias.clone());
        let new_accounts = match list_profiles() {
            Err(error) => {
                account_problems.push(format!(
                    "Could not load saved accounts; showing the stale account list: {error:#}"
                ));
                None
            }
            Ok(profiles) => {
                let current = sync_current_from_live().unwrap_or_else(read_current);
                let mut complete = true;
                let mut retained_usage: HashMap<_, _> = self
                    .accounts
                    .iter()
                    .map(|account| (account.alias.clone(), account.usage.clone()))
                    .collect();
                let mut accounts = Vec::with_capacity(profiles.len());
                for alias in profiles {
                    let path = match profile_auth_path(&alias) {
                        Ok(path) => path,
                        Err(error) => {
                            complete = false;
                            account_problems.push(format!(
                                "Could not load account '{}'; keeping its last loaded data: {error:#}",
                                alias
                            ));
                            continue;
                        }
                    };
                    let _ = path;
                    accounts.push(AccountEntry {
                        info: crate::claude_usage::read_profile(&alias)
                            .map(|profile| profile.info)
                            .unwrap_or_default(),
                        usage: retained_usage.remove(&alias).unwrap_or(UsageStatus::Idle),
                        is_current: alias == current,
                        alias,
                    });
                }
                if complete { Some(accounts) } else { None }
            }
        };

        if let Some(accounts) = new_accounts {
            self.accounts = accounts;
            self.marked
                .retain(|alias| self.accounts.iter().any(|account| &account.alias == alias));
            // A successful account-list read can follow credential replacement
            // for an existing alias. Invalidate late results only after commit.
            self.refreshing_requests.clear();
            self.pending_usage_refreshes.clear();
            self.selected = 0;
            self.view_indices.clear();
            self.update_view();
            let selected_alias = if account_problems.is_empty() {
                None
            } else {
                previous_selected_alias.as_deref()
            };
            let selected_idx = selected_alias
                .and_then(|alias| {
                    self.accounts
                        .iter()
                        .position(|account| account.alias == alias)
                })
                .or_else(|| self.accounts.iter().position(|account| account.is_current));
            if let Some(account_idx) = selected_idx
                && let Some(view_idx) = self.view_indices.iter().position(|&idx| idx == account_idx)
            {
                self.selected = view_idx;
            }
        }

        for problem in account_problems.iter() {
            tracing::warn!("{problem}");
        }
        self.profile_load_error =
            (!account_problems.is_empty()).then(|| account_problems.join("; "));
        let all_ok = self.profile_load_error.is_none();
        if let Some(summary) = self.profile_load_error.clone() { self.set_status_error(summary, 10); }
        all_ok
    }

    pub fn load_profiles_preserving_selection(&mut self) -> bool {
        let selected_alias = self
            .selected_account_idx()
            .and_then(|idx| self.accounts.get(idx))
            .map(|entry| entry.alias.clone());

        let loaded = self.load_profiles();

        if let Some(alias) = selected_alias
            && let Some(account_idx) = self.accounts.iter().position(|a| a.alias == alias)
            && let Some(view_idx) = self.view_indices.iter().position(|&idx| idx == account_idx)
        {
            self.selected = view_idx;
        }
        loaded
    }

    /// Recompute `view_indices` based on the current search query.
    pub fn update_view(&mut self) {
        let selected_account_idx = self.selected_account_idx();

        self.view_indices = match &self.search {
            None => (0..self.accounts.len()).collect(),
            Some(s) if s.query.is_empty() => (0..self.accounts.len()).collect(),
            Some(s) => {
                let q = s.query.to_lowercase();
                self.accounts
                    .iter()
                    .enumerate()
                    .filter(|(_, entry)| {
                        entry.alias.to_lowercase().contains(&q)
                            || entry
                                .info
                                .email
                                .as_deref()
                                .unwrap_or("")
                                .to_lowercase()
                                .contains(&q)
                            || entry
                                .info
                                .plan_type
                                .as_deref()
                                .unwrap_or("")
                                .to_lowercase()
                                .contains(&q)
                    })
                    .map(|(i, _)| i)
                    .collect()
            }
        };

        match self.sort_mode {
            SortMode::Name => {}
            SortMode::Quota => {
                let quotas: Vec<f64> = (0..self.accounts.len())
                    .map(|idx| self.get_5h_used_pct(idx))
                    .collect();
                self.view_indices.sort_by(|&a, &b| {
                    quotas[a]
                        .partial_cmp(&quotas[b])
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            SortMode::Status => {
                let statuses: Vec<u8> = (0..self.accounts.len())
                    .map(|idx| self.status_order(idx))
                    .collect();
                self.view_indices
                    .sort_by(|&a, &b| statuses[a].cmp(&statuses[b]));
            }
        }

        if let Some(account_idx) = selected_account_idx
            && let Some(view_idx) = self.view_indices.iter().position(|&idx| idx == account_idx)
        {
            self.selected = view_idx;
            return;
        }

        if self.view_indices.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.view_indices.len() {
            self.selected = self.view_indices.len() - 1;
        }
    }

    /// Get the selected index in `accounts`.
    pub fn selected_account_idx(&self) -> Option<usize> {
        self.view_indices.get(self.selected).copied()
    }

    pub fn loading_count(&self) -> usize {
        self.refreshing_requests.len()
    }

    pub fn is_refreshing(&self, alias: &str) -> bool {
        self.refreshing_requests.contains_key(alias)
    }

    pub fn cycle_sort(&mut self) {
        self.sort_mode = match self.sort_mode {
            SortMode::Name => SortMode::Quota,
            SortMode::Quota => SortMode::Status,
            SortMode::Status => SortMode::Name,
        };
        self.update_view();
    }

    pub fn toggle_mark(&mut self) {
        if let Some(idx) = self.selected_account_idx() {
            let alias = self.accounts[idx].alias.clone();
            if !self.marked.remove(&alias) {
                self.marked.insert(alias);
            }
        }

        if self.selected + 1 < self.view_indices.len() {
            self.selected += 1;
        }
    }

    pub fn clear_marks(&mut self) {
        self.marked.clear();
    }

    pub fn refresh_one(&mut self, alias: &str) {
        let Some(idx) = self
            .accounts
            .iter()
            .position(|account| account.alias == alias)
        else {
            return;
        };
        self.fetch_usage_for(idx, Refresh::Forced);
        self.set_status(format!("Refreshing {alias}"), 3);
    }

    /// Ask to use a reset of `alias`: opens the confirmation when its loaded
    /// usage names a usable grant and no claim is running or unsettled.
    pub fn request_use_reset(&mut self, alias: &str) {
        use crate::claude_api::usable_reset_grant;

        let Some(entry) = self.accounts.iter().find(|entry| entry.alias == alias) else {
            return;
        };
        if self.reset_in_flight.contains(alias) {
            self.set_status_error(format!("A reset for {alias} is already being used"), 5);
            return;
        }
        if self.reset_unknown.contains(alias) {
            self.set_status_error(
                format!(
                    "The last reset for {alias} may have been used; refresh {alias} (r), then try again"
                ),
                8,
            );
            return;
        }
        let grant = match &entry.usage {
            UsageStatus::Loaded(usage) => usable_reset_grant(
                usage.reset_grants.as_deref(),
                usage.next_reset_grant.as_deref(),
                auth::now_unix_secs(),
            ),
            _ => None,
        };
        let Some(grant) = grant else {
            self.set_status_error(format!("No reset to use for {alias}"), 5);
            return;
        };
        self.confirm = Some(ConfirmAction::UseReset {
            alias: alias.to_owned(),
            label: grant.label.clone(),
            resets_left: grant.resets_left,
            ends_at: grant.ends_at.clone(),
        });
    }

    /// Claim a reset in the background; the result comes back on `pending_resets`.
    fn start_reset_claim(&mut self, alias: String) {
        if self.reset_in_flight.contains(&alias) {
            self.set_status_error(format!("A reset for {alias} is already being used"), 5);
            return;
        }
        let request_id = self
            .reset_request_ids
            .entry(alias.clone())
            .or_insert_with(crate::claude_api::new_request_id)
            .clone();
        self.reset_in_flight.insert(alias.clone());
        self.set_status(format!("Using a reset for {alias}..."), 60);
        let claimer = self.claimer.clone();
        let sender = self.reset_sender.clone();
        tokio::spawn(async move {
            // A panic inside the claim must still report, as an unknown outcome.
            let claim = tokio::spawn(claimer(alias.clone(), request_id));
            let result = claim.await.unwrap_or_else(|error| {
                Err(crate::claude_api::ClaimError::Unknown(format!("claim task failed: {error}")))
            });
            let _ = sender.send((alias, result)).await;
        });
    }

    fn poll_reset_results(&mut self) {
        while let Ok((alias, result)) = self.pending_resets.try_recv() {
            self.handle_reset_claim_result(alias, result);
        }
    }

    /// Report a finished claim and refresh the account's usage. Only a definite
    /// outcome settles the request id; an unknown one blocks the account until
    /// its next usage reading.
    pub(crate) fn handle_reset_claim_result(
        &mut self,
        alias: String,
        result: Result<crate::claude_api::ResetClaim, crate::claude_api::ClaimError>,
    ) {
        use crate::claude_api::{ClaimError, ClaimResult, UsageError as ApiError};

        self.reset_in_flight.remove(&alias);
        if !matches!(result, Err(ClaimError::Unknown(_))) {
            self.reset_request_ids.remove(&alias);
        }
        match result {
            Ok(claim) => match claim.result {
                ClaimResult::Reset => {
                    let left = claim
                        .resets_left
                        .map(|left| format!(" · {left} left"))
                        .unwrap_or_default();
                    self.set_status(format!("Reset used for {alias}{left}"), 8);
                }
                ClaimResult::AlreadyUsed => {
                    self.set_status_error(format!("Reset already used for {alias}"), 8);
                }
                ClaimResult::NotLimited => {
                    self.set_status_error(format!("{alias} is not at its limit; nothing to reset"), 8);
                }
                ClaimResult::Cooldown => {
                    let until = claim
                        .cooldown_until
                        .as_deref()
                        .map(|at| match chrono::DateTime::parse_from_rfc3339(at) {
                            Ok(at) => format!(
                                " until {}",
                                at.with_timezone(&chrono::Local).format("%m-%d %H:%M")
                            ),
                            Err(_) => format!(" until {at}"),
                        })
                        .unwrap_or_default();
                    self.set_status_error(format!("Reset for {alias} is on cooldown{until}"), 8);
                }
                ClaimResult::Ineligible => {
                    self.set_status_error(format!("{alias} is not eligible for a reset"), 8);
                }
                ClaimResult::Unavailable => {
                    self.set_status_error(format!("Reset unavailable for {alias}"), 8);
                }
            },
            Err(ClaimError::Rejected(error)) => {
                let why = match error {
                    ApiError::RateLimited { .. } => "rate limited; try again later".to_owned(),
                    ApiError::Unauthorized => "sign-in expired; log in again".to_owned(),
                    ApiError::TokenExpired => {
                        "token expired; use Claude Code once to renew it".to_owned()
                    }
                    ApiError::Http(status) => format!("HTTP {status}"),
                    ApiError::Network(message) | ApiError::BadResponse(message) => message,
                };
                self.set_status_error(format!("No reset used for {alias}: {why}"), 8);
            }
            Err(ClaimError::Unknown(message)) => {
                self.reset_unknown.insert(alias.clone());
                self.set_status_error(
                    format!("Reset for {alias}: outcome unknown ({message}); refreshing"),
                    10,
                );
            }
        }
        if let Some(idx) = self.accounts.iter().position(|entry| entry.alias == alias) {
            self.fetch_usage_for(idx, Refresh::Forced);
        }
    }

    pub fn poll_update(&mut self) {
        if let Some(rx) = &mut self.update_rx {
            match rx.try_recv() {
                Ok(version) => {
                    self.update_available = Some(version);
                    self.update_rx = None;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    // Sender dropped without sending (no update or check failed)
                    self.update_rx = None;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                    // Still waiting, keep polling
                }
            }
        }
    }

    pub fn start_update_check(&mut self) {
        if self.update_rx.is_some() || self.update_available.is_some() {
            return;
        }

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.update_rx = Some(rx);
        // This fork is distributed on npm, not through the upstream GitHub releases the
        // inherited updater looks at, so ask the npm registry for the latest version.
        tokio::spawn(async move {
            let latest = async {
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(8))
                    .build()
                    .ok()?;
                let body: serde_json::Value = client
                    .get("https://registry.npmjs.org/paper-claude-switch/latest")
                    .send()
                    .await
                    .ok()?
                    .json()
                    .await
                    .ok()?;
                body["version"].as_str().map(str::to_string)
            }
            .await;
            if let Some(latest) = latest
                && let (Ok(new), Ok(current)) = (
                    semver::Version::parse(&latest),
                    semver::Version::parse(crate::update::current_version()),
                )
                && new > current
            {
                let _ = tx.send(latest);
            }
        });
    }

    fn get_5h_used_pct(&self, idx: usize) -> f64 {
        match &self.accounts[idx].usage {
            UsageStatus::Loaded(u) => u
                .primary
                .as_ref()
                .and_then(|w| w.used_percent)
                // free accounts have no 5h window — fall back to 7d usage for sorting
                .or_else(|| u.secondary.as_ref().and_then(|w| w.used_percent))
                .unwrap_or(999.0),
            _ => 999.0,
        }
    }

    fn status_order(&self, idx: usize) -> u8 {
        match &self.accounts[idx].usage {
            UsageStatus::Error(_) => 0,
            UsageStatus::Loaded(u) if !crate::usage::is_available(u) => 1,
            UsageStatus::Loaded(_) => 2,
            UsageStatus::Loading => 3,
            UsageStatus::Idle => 4,
        }
    }

    fn fetch_usage_for(&mut self, idx: usize, refresh: Refresh) {
        let entry = match self.accounts.get(idx) {
            Some(e) => e,
            None => return,
        };
        if self.refreshing_requests.contains_key(&entry.alias) {
            if refresh_fetches_loaded_usage(refresh) {
                self.pending_usage_refreshes
                    .entry(entry.alias.clone())
                    .and_modify(|queued| {
                        if refresh_priority(refresh) > refresh_priority(*queued) {
                            *queued = refresh;
                        }
                    })
                    .or_insert(refresh);
            }
            return;
        }
        let needs_usage =
            refresh_fetches_loaded_usage(refresh) || !matches!(entry.usage, UsageStatus::Loaded(_));
        if !needs_usage {
            return;
        }

        let alias = entry.alias.clone();
        let limiter = self.usage_limiter.clone();

        if !matches!(self.accounts[idx].usage, UsageStatus::Loaded(_)) {
            self.accounts[idx].usage = UsageStatus::Loading;
        }

        let usage_tx = self.result_sender.clone();
        let request_id = self.usage_next_id;
        self.usage_next_id = self.usage_next_id.wrapping_add(1);
        self.refreshing_requests
            .insert(alias.clone(), (request_id, refresh));
        tokio::spawn(async move {
            let result = with_usage_limiter(&limiter, async {
                match refresh {
                    Refresh::Cached => fetch_usage_retried(&alias).await,
                    Refresh::Unattended => fetch_usage_retried_unattended(&alias).await,
                    Refresh::Forced => fetch_usage_retried_force(&alias).await,
                }
            })
            .await;
            let _ = usage_tx.send((alias, request_id, result)).await;
        });
    }

    fn refresh_indices(&mut self, target_indices: &[usize], refresh: Refresh) {
        for &i in target_indices {
            let entry = &mut self.accounts[i];
            if let UsageStatus::Error(_) = &entry.usage {
                entry.usage = UsageStatus::Idle;
            }
            if matches!(refresh, Refresh::Cached)
                && let Some(cached) = crate::cache::get(&entry.alias)
            {
                entry.usage = UsageStatus::Loaded(Box::new(cached));

            }
        }
        for &i in target_indices {
            self.fetch_usage_for(i, refresh);
        }

        self.update_view();
    }

    /// Refresh usage for all visible accounts (search-filtered view).
    /// Batch refresh of just the marked accounts is exposed separately
    /// via the Enter > Batch menu so the implicit "marks change scope"
    /// behavior is gone.
    pub fn refresh(&mut self, refresh: Refresh) {
        let target_indices: Vec<usize> = self.view_indices.clone();
        self.refresh_indices(&target_indices, refresh);
    }

    pub fn refresh_all(&mut self, refresh: Refresh) {
        let target_indices: Vec<usize> = (0..self.accounts.len()).collect();
        self.refresh_indices(&target_indices, refresh);
    }

    pub fn poll_results(&mut self) {
        self.poll_reset_results();
        let mut changed = false;
        let open_account_alias = match self.menu.as_ref() {
            Some(super::menu::MenuState::Account { info, .. }) => Some(info.alias.clone()),
            _ => None,
        };
        let mut refresh_open_account = false;
        while let Ok((alias, request_id, result)) = self.pending_results.try_recv() {
            let Some((active_id, refresh)) = self.refreshing_requests.get(&alias).copied() else {
                continue;
            };
            if active_id != request_id {
                continue;
            }
            self.refreshing_requests.remove(&alias);
            let Some(idx) = self.accounts.iter().position(|entry| entry.alias == alias) else {
                continue;
            };
            self.accounts[idx].usage = match result {
                Ok(u) => {
                    // Fresh usage shows whether an unknown claim used a reset.
                    self.reset_unknown.remove(&alias);
                    if matches!(refresh, Refresh::Forced) {
                        tracing::info!(action = "usage_refresh", alias = %alias, outcome = "completed", "usage refresh completed");
                    }
                    UsageStatus::Loaded(Box::new(u))
                }
                Err(e) => {
                    if matches!(refresh, Refresh::Forced) {
                        tracing::error!(action = "usage_refresh", alias = %alias, outcome = "failed", "usage refresh failed");
                    }
                    UsageStatus::Error(e)
                }
            };
            refresh_open_account |= open_account_alias.as_deref() == Some(alias.as_str());
            changed = true;
            if let Some(refresh) = self.pending_usage_refreshes.remove(&alias) {
                self.fetch_usage_for(idx, refresh);
            }
        }
        if changed {
            self.update_view();
        }
        if refresh_open_account {
            self.rebuild_open_account_menu();
        }
    }

    pub fn switch_selected(&mut self) {
        let Some(alias) = self
            .selected_account_idx()
            .and_then(|idx| self.accounts.get(idx))
            .map(|entry| entry.alias.clone())
        else {
            self.set_status_error("No account selected".to_string(), 3);
            return;
        };
        self.start_switch(alias);
    }

    fn start_switch(&mut self, alias: String) {
        if let Some(active) = self.switching_alias.as_deref() {
            self.set_status(format!("Account switch already in progress ({active})"), 4);
            return;
        }

        self.switching_alias = Some(alias.clone());
        self.set_status(format!("Switching to {alias}..."), 60);
        let sender = self.switch_sender.clone();
        let panic_alias = alias.clone();
        tokio::task::spawn_blocking(move || {
            // Always report a completion so shutdown cannot wait forever if a
            // lower-level credential operation panics while holding a lock.
            let completion = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                match switch_profile(&alias) {
                    Ok(_) => {
                        let last_used_error = cache::set_last_used(&alias)
                            .err()
                            .map(|error| error.to_string());
                        SwitchCompletion::Succeeded {
                            current: read_current(),
                            alias,
                            last_used_error,
                        }
                    }
                    Err(error) => SwitchCompletion::Failed {
                        alias,
                        error: error.to_string(),
                    },
                }
            }))
            .unwrap_or_else(|_| SwitchCompletion::Failed {
                alias: panic_alias,
                error: "account switch task panicked".to_string(),
            });
            let _ = sender.blocking_send(completion);
        });
    }

    fn finish_switch(&mut self, completion: SwitchCompletion) {
        let alias = match &completion {
            SwitchCompletion::Succeeded { alias, .. } | SwitchCompletion::Failed { alias, .. } => {
                alias
            }
        };
        if self.switching_alias.as_deref() != Some(alias.as_str()) {
            return;
        }
        self.switching_alias = None;

        match completion {
            SwitchCompletion::Succeeded {
                alias,
                current,
                last_used_error,
            } => {
                let current = if current.is_empty() {
                    alias.clone()
                } else {
                    current
                };
                for account in &mut self.accounts {
                    account.is_current = account.alias == current;
                }
                self.update_view();
                let mut status = format!("Switched to {alias}");
                let mut seconds = 3;
                if let Some(error) = last_used_error {
                    tracing::warn!(
                        action = "switch",
                        alias = %alias,
                        outcome = "completed",
                        error = %error,
                        "account switched but last-used cache update failed"
                    );
                    status.push_str(&format!("; last-used cache update failed: {error}"));
                    seconds = 8;
                } else {
                    tracing::info!(
                        action = "switch",
                        alias = %alias,
                        outcome = "completed",
                        "account switched"
                    );
                }
                self.set_status(status, seconds);
            }
            SwitchCompletion::Failed { alias, error } => {
                tracing::error!(
                    action = "switch",
                    alias = %alias,
                    outcome = "failed",
                    error = %error,
                    "account switch failed"
                );
                self.set_status_error(format!("Switch failed: {error}"), 5);
            }
        }
    }

    pub fn poll_switch_results(&mut self) {
        while let Ok(completion) = self.pending_switches.try_recv() {
            self.finish_switch(completion);
        }
    }

    pub fn switch_in_flight(&self) -> bool {
        self.switching_alias.is_some()
    }

    fn defer_while_switching(&mut self, action: &str) -> bool {
        if !self.switch_in_flight() {
            return false;
        }
        self.set_status(
            format!("Waiting for account switch to finish before {action}"),
            60,
        );
        true
    }

    pub async fn wait_for_switch_completion(&mut self) {
        while self.switch_in_flight() {
            match self.pending_switches.recv().await {
                Some(completion) => self.finish_switch(completion),
                None => {
                    self.switching_alias = None;
                    self.set_status_error(
                        "Account switch task ended before reporting completion".to_string(),
                        8,
                    );
                }
            }
        }
    }

    pub fn confirm_action(&mut self) -> bool {
        if self.switch_in_flight()
            && matches!(
                self.confirm.as_ref(),
                Some(ConfirmAction::Delete(_) | ConfirmAction::BatchDelete(_))
            )
        {
            self.defer_while_switching("deleting accounts");
            return false;
        }
        let action = match self.confirm.take() {
            Some(a) => a,
            None => return false,
        };
        match action {
            ConfirmAction::DiscardSettings => return true,
            ConfirmAction::UseReset { alias, .. } => self.start_reset_claim(alias),
            ConfirmAction::Delete(alias) => match cmd_delete(&alias) {
                Ok(()) => {
                    self.set_status(format!("Deleted {alias} (recoverable)"), 3);
                    if self.load_profiles_preserving_selection() {
                        self.refresh(Refresh::Forced);
                    }
                }
                Err(e) => self.set_status_error(format!("Delete failed: {e}"), 5),
            },

            ConfirmAction::BatchDelete(aliases) => {
                let mut ok = 0usize;
                let mut errors: Vec<String> = Vec::new();
                let current = read_current();
                for alias in &aliases {
                    if alias == &current {
                        errors.push(format!("{alias}: active, skipped"));
                        continue;
                    }
                    match cmd_delete(alias) {
                        Ok(()) => ok += 1,
                        Err(e) => errors.push(format!("{alias}: {e}")),
                    }
                }
                let loaded = self.load_profiles_preserving_selection();
                if loaded {
                    self.refresh(Refresh::Forced);
                }
                let msg = if errors.is_empty() {
                    format!("Deleted {ok} account(s) (recoverable)")
                } else {
                    format!("Deleted {ok} ok, {} failed", errors.len())
                };
                if errors.is_empty() {
                    self.set_status(msg, 6);
                } else {
                    self.set_status_error(msg, 6);
                }
            }

        }
        false
    }

    pub fn request_batch_delete(&mut self) {
        if self.marked.is_empty() {
            return;
        }
        if self.defer_while_switching("deleting accounts") {
            return;
        }
        let aliases: Vec<String> = self.marked.iter().cloned().collect();
        self.confirm = Some(ConfirmAction::BatchDelete(aliases));
    }

    /// Refresh all marked accounts (force).
    pub fn refresh_marked(&mut self) {
        if self.marked.is_empty() {
            return;
        }
        let target_indices: Vec<usize> = self
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| self.marked.contains(&a.alias))
            .map(|(i, _)| i)
            .collect();
        let count = target_indices.len();
        self.refresh_indices(&target_indices, Refresh::Forced);
        self.set_status(format!("Refreshing {count} marked account(s)..."), 3);
    }

    pub fn cancel_confirm(&mut self) {
        self.confirm = None;
    }

    pub fn request_quit(&mut self) -> bool {
        if self.settings.is_dirty() {
            self.confirm = Some(ConfirmAction::DiscardSettings);
            false
        } else {
            true
        }
    }

    pub fn handle_rename_key(&mut self, code: KeyCode) -> bool {
        if self.active_tab == Tab::Accounts
            && matches!(code, KeyCode::Enter)
            && self.defer_while_switching("renaming an account")
        {
            return false;
        }
        let state = match &mut self.rename {
            Some(s) => s,
            None => return false,
        };
        match code {
            KeyCode::Esc => {
                self.rename = None;
                return false;
            }
            KeyCode::Enter => {
                let old = state.old_alias.clone();
                let new = state.input.trim().to_string();
                self.rename = None;
                if new.is_empty() || new == old {
                    return false;
                }
                if let Err(err) = validate_alias(&new) {
                    self.set_status_error(format!("Invalid alias: {err}"), 3);
                    return false;
                }
                match self.active_tab {

                    Tab::Accounts => match rename_profile(&old, &new) {
                        Ok(()) => {
                            let was_marked = self.marked.remove(&old);
                            if was_marked {
                                self.marked.insert(new.clone());
                            }
                            self.set_status(format!("Renamed {old} -> {new}"), 3);
                            let loaded = self.load_profiles();
                            if let Some(account_idx) = loaded
                                .then(|| self.accounts.iter().position(|a| a.alias == new))
                                .flatten()
                                && let Some(view_idx) =
                                    self.view_indices.iter().position(|&idx| idx == account_idx)
                            {
                                self.selected = view_idx;
                            }
                            if loaded {
                                self.refresh(Refresh::Forced);
                            }
                        }
                        Err(e) => self.set_status_error(format!("Rename failed: {e}"), 5),
                    },
                    Tab::Settings | Tab::Logs => {}
                }
                return false;
            }
            KeyCode::Backspace if state.cursor > 0 => {
                state.cursor -= 1;
                let byte_pos = char_to_byte(&state.input, state.cursor);
                state.input.remove(byte_pos);
            }
            KeyCode::Delete => {
                let char_count = state.input.chars().count();
                if state.cursor < char_count {
                    let byte_pos = char_to_byte(&state.input, state.cursor);
                    state.input.remove(byte_pos);
                }
            }
            KeyCode::Left if state.cursor > 0 => {
                state.cursor -= 1;
            }
            KeyCode::Right => {
                let char_count = state.input.chars().count();
                if state.cursor < char_count {
                    state.cursor += 1;
                }
            }
            KeyCode::Home => {
                state.cursor = 0;
            }
            KeyCode::End => {
                state.cursor = state.input.chars().count();
            }
            KeyCode::Char(c) => {
                let byte_pos = char_to_byte(&state.input, state.cursor);
                state.input.insert(byte_pos, c);
                state.cursor += 1;
            }
            _ => {}
        }
        true
    }

    pub fn handle_search_key(&mut self, code: KeyCode) -> bool {
        let mut clear_search = false;
        let mut accept_search = false;

        {
            let state = match &mut self.search {
                Some(s) => s,
                None => return false,
            };

            match code {
                KeyCode::Esc => {
                    clear_search = true;
                }
                KeyCode::Enter => {
                    accept_search = true;
                }
                KeyCode::Backspace if state.cursor > 0 => {
                    state.cursor -= 1;
                    let byte_pos = char_to_byte(&state.query, state.cursor);
                    state.query.remove(byte_pos);
                }
                KeyCode::Delete => {
                    let char_count = state.query.chars().count();
                    if state.cursor < char_count {
                        let byte_pos = char_to_byte(&state.query, state.cursor);
                        state.query.remove(byte_pos);
                    }
                }
                KeyCode::Left if state.cursor > 0 => {
                    state.cursor -= 1;
                }
                KeyCode::Right => {
                    let char_count = state.query.chars().count();
                    if state.cursor < char_count {
                        state.cursor += 1;
                    }
                }
                KeyCode::Home => {
                    state.cursor = 0;
                }
                KeyCode::End => {
                    state.cursor = state.query.chars().count();
                }
                KeyCode::Char(c) => {
                    let byte_pos = char_to_byte(&state.query, state.cursor);
                    state.query.insert(byte_pos, c);
                    state.cursor += 1;
                }
                _ => {}
            }
        }

        if clear_search {
            self.search = None;
            self.search_active = false;
            self.update_view();
            return false;
        }

        if accept_search {
            self.search_active = false;
            if self
                .search
                .as_ref()
                .is_some_and(|state| state.query.is_empty())
            {
                self.search = None;
            }
            self.update_view();
            return false;
        }

        self.update_view();
        true
    }

    fn set_status(&mut self, msg: String, secs: u64) {
        self.status_msg = Some(msg);
        self.status_is_error = false;
        self.status_expiry = Some(Instant::now() + Duration::from_secs(secs));
    }

    fn set_status_error(&mut self, msg: String, secs: u64) {
        self.status_msg = Some(msg);
        self.status_is_error = true;
        self.status_expiry = Some(Instant::now() + Duration::from_secs(secs));
    }

    pub fn auto_refresh_interval_secs(&self) -> u64 {
        self.auto_refresh_interval.as_secs()
    }

    pub fn auto_refresh_remaining_secs(&self) -> Option<u64> {
        if !self.auto_refresh_enabled {
            return None;
        }
        Some(
            self.next_auto_refresh
                .map(|next| next.saturating_duration_since(Instant::now()).as_secs())
                .unwrap_or(0),
        )
    }

    pub fn toggle_auto_refresh(&mut self) {
        self.auto_refresh_enabled = !self.auto_refresh_enabled;
        if self.auto_refresh_enabled {
            self.next_auto_refresh = Some(Instant::now());
            self.set_status(
                format!(
                    "Auto refresh on (every {}s)",
                    self.auto_refresh_interval_secs()
                ),
                4,
            );
        } else {
            self.next_auto_refresh = None;
            self.set_status("Auto refresh off".to_string(), 3);
        }
    }

    pub fn toggle_detail_panel(&mut self) {
        self.detail_visible = !self.detail_visible;
        if self.detail_visible {
            self.set_status("Account details shown".to_string(), 3);
        } else {
            self.set_status("Account details hidden".to_string(), 3);
        }
    }

    pub fn run_due_auto_refresh(&mut self) {
        if !self.auto_refresh_enabled {
            return;
        }

        let now = Instant::now();
        if self.next_auto_refresh.is_some_and(|next| now < next) {
            return;
        }

        if self.loading_count() > 0 || self.switch_in_flight() {
            self.next_auto_refresh = Some(now + Duration::from_secs(5));
            return;
        }

        if !self.load_profiles_preserving_selection() {
            self.next_auto_refresh = Some(now + self.auto_refresh_interval);
            return;
        }
        let account_count = self.accounts.len();
        self.refresh_all(Refresh::Unattended);
        self.next_auto_refresh = Some(now + self.auto_refresh_interval);

        self.set_status(
            format!("Auto refresh: refreshing {account_count} account(s)"),
            4,
        );
    }

    pub fn tick(&mut self) {
        if let Some(expiry) = self.status_expiry
            && Instant::now() >= expiry
        {
            self.status_msg = None;
            self.status_expiry = None;
        }

    }
}

pub async fn run() -> Result<()> {
    // auth-change detection runs before dispatch(), so auto_track is already handled.

    // The TUI is a designed full-screen UI. CLI still honors NO_COLOR;
    // leaving crossterm's default would strip every style and look like
    // the palette had been deleted.
    crossterm::style::force_color_output(true);

    // Ensure terminal is restored even on panic
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        disable_mouse_capture();
        ratatui::restore();
        original_hook(info);
    }));

    let mut shutdown =
        crate::signals::ShutdownListener::new().context("registering TUI shutdown handlers")?;
    let mut terminal = ratatui::init();
    enable_mouse_capture();
    let result = run_app(&mut terminal, &mut shutdown).await;
    disable_mouse_capture();
    ratatui::restore();
    if let Some(signal) = result? {
        std::process::exit(signal.exit_code());
    }
    Ok(())
}

async fn run_app(
    terminal: &mut DefaultTerminal,
    shutdown: &mut crate::signals::ShutdownListener,
) -> Result<Option<crate::signals::ShutdownSignal>> {
    let mut app = App::new();
    let profiles_loaded = app.load_profiles();
    app.update_view();

    if profiles_loaded && !app.accounts.is_empty() {
        app.refresh(Refresh::Cached);
    }
    app.start_update_check();
    let mut quit_after_switch = false;

    loop {
        app.poll_switch_results();
        if quit_after_switch && !app.switch_in_flight() {
            break;
        }
        app.poll_results();
        app.poll_update();
        app.tick();
        app.run_due_auto_refresh();

        terminal
            .draw(|f| super::ui::render(f, &mut app))
            .context("drawing TUI")?;

        let event = tokio::select! {
            signal = shutdown.recv_signal() => {
                wait_for_switch_before_exit(&mut app).await;
                return Ok(Some(signal));
            },
            event = tokio::task::spawn_blocking(|| -> Result<Option<Event>> {
                if event::poll(Duration::from_millis(100)).context("polling terminal events")? {
                    Ok(Some(event::read().context("reading terminal event")?))
                } else {
                    Ok(None)
                }
            }) => event.context("terminal event task panicked")??,
        };
        if let Some(event) = event {
            match event {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    if !accepts_key_event(&key) {
                        continue;
                    }
                    app.last_list_click = None;

                    // Search and rename inputs need raw case-sensitive keystrokes.
                    if app.rename.is_some() {
                        app.handle_rename_key(key.code);
                        continue;
                    }
                    if app.search_active {
                        app.handle_search_key(key.code);
                        continue;
                    }
                    // The provider form needs raw, case-sensitive keystrokes.

                    if app.active_tab == Tab::Settings && app.settings.is_editing() {
                        app.handle_settings_key(key.code);
                        continue;
                    }

                    // Normalize letter case for top-level dispatch:
                    // any uppercase letter is treated as its lowercase equivalent.
                    let code = match key.code {
                        KeyCode::Char(c) if c.is_ascii_uppercase() => {
                            KeyCode::Char(c.to_ascii_lowercase())
                        }
                        other => other,
                    };

                    // Help popup: any key (esc/q/h preferred) closes it; arrows scroll.
                    if app.help_popup.is_some() {
                        handle_help_key(&mut app, code);
                        continue;
                    }

                    // Active menu intercepts everything.
                    if app.menu.is_some() {
                        if let Some(signal) =
                            handle_menu_key(&mut app, terminal, code, shutdown).await
                        {
                            wait_for_switch_before_exit(&mut app).await;
                            return Ok(Some(signal));
                        }
                        continue;
                    }

                    if app.confirm.is_some() {
                        match code {
                            KeyCode::Char('y') if app.confirm_action() => {
                                if app.switch_in_flight() {
                                    quit_after_switch = true;
                                    app.set_status(
                                        "Waiting for account switch to finish before exit"
                                            .to_string(),
                                        60,
                                    );
                                } else {
                                    break;
                                }
                            }
                            KeyCode::Char('y') => {}
                            _ => app.cancel_confirm(),
                        }
                        continue;
                    }

                    match dispatch_main_key(
                        &mut app,
                        terminal,
                        code,
                        shutdown,
                        &mut quit_after_switch,
                    )
                    .await
                    {
                        MainKeyOutcome::Continue => {}
                        MainKeyOutcome::Quit => break,
                        MainKeyOutcome::Signal(signal) => {
                            wait_for_switch_before_exit(&mut app).await;
                            return Ok(Some(signal));
                        }
                    }
                }
                Event::Mouse(mouse) => {
                    if let Some(code) = app.handle_mouse(mouse) {

                        if app.rename.is_some() {
                            app.handle_rename_key(code);
                            continue;
                        }
                        if app.search_active {
                            app.handle_search_key(code);
                            continue;
                        }
                        if app.confirm.is_some() {
                            match code {
                                KeyCode::Char('y') if app.confirm_action() => {
                                    if app.switch_in_flight() {
                                        quit_after_switch = true;
                                        app.set_status(
                                            "Waiting for account switch to finish before exit"
                                                .to_string(),
                                            60,
                                        );
                                    } else {
                                        break;
                                    }
                                }
                                KeyCode::Char('y') => {}
                                _ => app.cancel_confirm(),
                            }
                            continue;
                        }
                        let outcome = if app.menu.is_some() {
                            handle_menu_key(&mut app, terminal, code, shutdown)
                                .await
                                .map_or(MainKeyOutcome::Continue, MainKeyOutcome::Signal)
                        } else {
                            dispatch_main_key(
                                &mut app,
                                terminal,
                                code,
                                shutdown,
                                &mut quit_after_switch,
                            )
                            .await
                        };
                        match outcome {
                            MainKeyOutcome::Continue => {}
                            MainKeyOutcome::Quit => break,
                            MainKeyOutcome::Signal(signal) => {
                                wait_for_switch_before_exit(&mut app).await;
                                return Ok(Some(signal));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    Ok(None)
}

enum MainKeyOutcome {
    Continue,
    Quit,
    Signal(crate::signals::ShutdownSignal),
}

async fn dispatch_main_key(
    app: &mut App,
    terminal: &mut DefaultTerminal,
    code: KeyCode,
    shutdown: &mut crate::signals::ShutdownListener,
    quit_after_switch: &mut bool,
) -> MainKeyOutcome {
    match code {
        KeyCode::Char('q') => {
            if app.request_quit() {
                if app.switch_in_flight() {
                    *quit_after_switch = true;
                    app.set_status(
                        "Waiting for account switch to finish before exit".to_string(),
                        60,
                    );
                    MainKeyOutcome::Continue
                } else {
                    MainKeyOutcome::Quit
                }
            } else {
                MainKeyOutcome::Continue
            }
        }
        KeyCode::Char('h') => {
            app.open_help();
            MainKeyOutcome::Continue
        }
        KeyCode::Tab => {
            app.cycle_tab(true);
            MainKeyOutcome::Continue
        }
        KeyCode::BackTab => {
            app.cycle_tab(false);
            MainKeyOutcome::Continue
        }
        _ => match app.active_tab {
            Tab::Accounts => {
                if let Some(alias) = app.handle_accounts_key(code)
                    && let Some(signal) = perform_launch(
                        terminal,
                        app,
                        alias,
                        Vec::new(),
                        shutdown,
                    )
                    .await
                {
                    MainKeyOutcome::Signal(signal)
                } else {
                    MainKeyOutcome::Continue
                }
            }

            Tab::Settings => {
                app.handle_settings_key(code);
                MainKeyOutcome::Continue
            }
            Tab::Logs => {
                match code {
                    KeyCode::Down | KeyCode::Char('j') => {
                        app.log_scroll = app.log_scroll.saturating_sub(1);
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        app.log_scroll = app.log_scroll.saturating_add(1);
                    }
                    KeyCode::PageDown => {
                        app.log_scroll = app.log_scroll.saturating_sub(10);
                    }
                    KeyCode::PageUp => {
                        app.log_scroll = app.log_scroll.saturating_add(10);
                    }
                    KeyCode::End => app.log_scroll = 0,
                    _ => {}
                }
                MainKeyOutcome::Continue
            }
        },
    }
}

async fn wait_for_switch_before_exit(app: &mut App) {
    if app.switch_in_flight() {
        app.set_status(
            "Waiting for account switch to finish before exit".to_string(),
            60,
        );
        app.wait_for_switch_completion().await;
    }
}

async fn handle_menu_key(
    app: &mut App,
    terminal: &mut DefaultTerminal,
    code: KeyCode,
    shutdown: &mut crate::signals::ShutdownListener,
) -> Option<crate::signals::ShutdownSignal> {
    let menu = app.menu.as_mut()?;
    let action = menu.handle_key(code);
    use super::menu::MenuAction;
    match action {
        MenuAction::Noop => {}
        MenuAction::Close => app.close_menu(),
        MenuAction::Use(alias) => {
            app.close_menu();
            // Reuse switch_selected logic by selecting the alias first.
            if let Some(account_idx) = app.accounts.iter().position(|a| a.alias == alias)
                && let Some(view_idx) = app.view_indices.iter().position(|&i| i == account_idx)
            {
                app.selected = view_idx;
            }
            app.switch_selected();
        }
        MenuAction::Launch(alias) => {
            if app.defer_while_switching("launching Claude Code") {
                return None;
            }
            app.close_menu();
            return perform_launch(terminal, app, alias, Vec::new(), shutdown).await;
        }
        MenuAction::ReloginRequest(alias, email) => {
            app.open_relogin_flow_menu(alias, email);
        }
        MenuAction::Relogin { alias, device } => {
            if app.defer_while_switching("re-logging in") {
                return None;
            }
            app.close_menu();
            perform_oauth(terminal, app, OAuthMode::Relogin(alias), device).await;
        }
        MenuAction::Add { device } => {
            if app.defer_while_switching("adding an account") {
                return None;
            }
            app.close_menu();
            perform_oauth(terminal, app, OAuthMode::Add, device).await;
        }
        MenuAction::RefreshOne(alias) => {
            app.close_menu();
            app.refresh_one(&alias);
        }
        MenuAction::Rename(alias) => {
            if app.defer_while_switching("renaming an account") {
                return None;
            }
            app.close_menu();
            app.start_rename_alias(&alias);
        }

        MenuAction::DeleteRequest(alias) => {
            if app.defer_while_switching("deleting an account") {
                return None;
            }
            app.close_menu();
            app.request_delete_alias(&alias);
        }
        MenuAction::UseReset(alias) => {
            app.close_menu();
            app.request_use_reset(&alias);
        }
        MenuAction::BatchRefresh => {
            app.close_menu();
            app.refresh_marked();
        }

        MenuAction::BatchDeleteRequest => {
            if app.defer_while_switching("deleting accounts") {
                return None;
            }
            app.close_menu();
            app.request_batch_delete();
        }
    }
    None
}

enum OAuthMode {
    Add,
    Relogin(String),
}

fn reset_plain_terminal_view() {
    let mut stdout = std::io::stdout();
    let _ = crossterm::execute!(
        stdout,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, 0),
    );
    let _ = std::io::Write::flush(&mut stdout);
}

fn suspend_tui_for_plain_output() {
    disable_mouse_capture();
    ratatui::restore();
    reset_plain_terminal_view();
}

fn resume_tui_after_plain_output(terminal: &mut DefaultTerminal) {
    reset_plain_terminal_view();
    *terminal = ratatui::init();
    enable_mouse_capture();
    let _ = terminal.clear();
}

async fn perform_launch(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    alias: String,
    extra_args: Vec<String>,
    shutdown: &mut crate::signals::ShutdownListener,
) -> Option<crate::signals::ShutdownSignal> {
    suspend_tui_for_plain_output();
    crate::output::set_message_mode(crate::output::MessageMode::Stdout);

    println!("\n=== Launch Claude Code: {alias} ===\n");

    let result = crate::launch::launch_for_tui(&alias, extra_args, shutdown).await;

    let _ = std::io::Write::flush(&mut std::io::stdout());

    match &result {
        Ok(crate::launch::TuiLaunchOutcome::Exited(0)) => {
            println!("\nClaude Code exited successfully.")
        }
        Ok(crate::launch::TuiLaunchOutcome::Exited(exit_code)) => {
            println!("\nClaude Code exited with code {exit_code}.")
        }
        Ok(crate::launch::TuiLaunchOutcome::Shutdown { .. }) => {
            println!("\nShutdown requested.")
        }
        Err(e) => eprintln!("\nError: {e}"),
    }
    println!("\nReturning to TUI...");
    if result.is_err()
        || result
            .as_ref()
            .is_ok_and(|outcome| !matches!(outcome, crate::launch::TuiLaunchOutcome::Exited(0)))
    {
        tokio::time::sleep(Duration::from_millis(1200)).await;
    }

    crate::output::set_message_mode(crate::output::MessageMode::Silent);
    resume_tui_after_plain_output(terminal);

    match result {
        Ok(crate::launch::TuiLaunchOutcome::Exited(0)) => {
            app.set_status(format!("Claude Code session ended ({alias})"), 4);
            if app.load_profiles_preserving_selection() {
                app.refresh(Refresh::Cached);
            }
            if app.auto_refresh_enabled {
                app.next_auto_refresh = Some(Instant::now() + app.auto_refresh_interval);
            }
        }
        Ok(crate::launch::TuiLaunchOutcome::Exited(exit_code)) => {
            app.set_status_error(format!("Claude Code exited with code {exit_code}"), 5);
        }
        Ok(crate::launch::TuiLaunchOutcome::Shutdown {
            signal,
            cleanup_error,
        }) => {
            if let Some(error) = cleanup_error {
                eprintln!("\nError while cleaning up interrupted launch: {error}");
            }
            return Some(signal);
        }
        Err(e) => app.set_status_error(format!("Launch failed: {e}"), 6),
    }
    None
}

/// Suspend the TUI, save the current Claude Code login to the appropriate
/// profile, then restore the TUI.
///
/// Always restores the terminal even on error so the caller can keep running.
async fn perform_oauth(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    mode: OAuthMode,
    device: bool,
) {
    // Tear down TUI: restore cooked mode + clear screen so the result is visible.
    suspend_tui_for_plain_output();
    // TUI starts with MessageMode::Silent; switch to Stdout so messages show.
    crate::output::set_message_mode(crate::output::MessageMode::Stdout);

    let mode_name = match &mode {
        OAuthMode::Add => "Save the current Claude Code login".to_string(),
        OAuthMode::Relogin(alias) => {
            format!("Save the current Claude Code login into '{alias}'")
        }
    };
    println!("\n=== {mode_name} ===\n");

    let result = run_oauth_inner(mode, device).await;

    // Flush stdout so any buffered output (e.g. device code URL) appears
    // before TUI repaints, particularly important on Windows.
    let _ = std::io::Write::flush(&mut std::io::stdout());

    if result.is_ok() {
        println!("\nReturning to TUI...");
    } else {
        if let Err(ref e) = result {
            eprintln!("\nError: {e}");
        }
        println!("\nReturning to TUI...");
        tokio::time::sleep(Duration::from_millis(1200)).await;
    }

    // Restore silent mode before reinitializing TUI.
    crate::output::set_message_mode(crate::output::MessageMode::Silent);
    resume_tui_after_plain_output(terminal);

    match result {
        Ok(msg) => {
            tracing::info!(action = "oauth", outcome = "completed", "OAuth completed");
            app.set_status(msg, 5);
            if app.load_profiles_preserving_selection() {
                app.refresh(Refresh::Forced);
            }
            // Reset auto-refresh timer so it doesn't fire immediately.
            if app.auto_refresh_enabled {
                app.next_auto_refresh = Some(Instant::now() + app.auto_refresh_interval);
            }
        }
        Err(e) => {
            tracing::error!(action = "oauth", outcome = "failed", "OAuth failed");
            app.set_status_error(
                format!("Save failed: {e}. Log in to Claude Code with that account (/login) first."),
                7,
            );
        }
    }
}

/// Claude Code owns the sign-in: log in with `claude` first, then this saves
/// the live login as a new profile (add) or into the named one (re-login).
async fn run_oauth_inner(mode: OAuthMode, _device: bool) -> Result<String> {
    let paths = crate::claude_usage::paths()?;
    let app_home = auth::app_home()?;
    let alias = match &mode {
        OAuthMode::Add => None,
        OAuthMode::Relogin(alias) => Some(alias.as_str()),
    };
    let action = crate::claude_store::save_current(
        &paths,
        &app_home,
        alias,
        &crate::claude_store::LockOptions::default(),
    )?;
    Ok(match action {
        crate::claude_store::SaveAction::Created(alias) => format!("Account created: {alias}"),
        crate::claude_store::SaveAction::Updated(alias) => format!("Account updated: {alias}"),
    })
}

fn enable_mouse_capture() {
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
}

fn disable_mouse_capture() {
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
}

fn handle_help_key(app: &mut App, code: KeyCode) {
    let Some(state) = app.help_popup.as_mut() else {
        return;
    };
    match code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('h') => app.close_help(),
        KeyCode::Down | KeyCode::Char('j') => state.scroll_down(u16::MAX),
        KeyCode::Up | KeyCode::Char('k') => state.scroll_up(),
        KeyCode::PageDown => state.page_down(5, u16::MAX),
        KeyCode::PageUp => state.page_up(5),
        KeyCode::Home => state.reset(),
        _ => app.close_help(),
    }
}

fn accepts_key_event(key: &KeyEvent) -> bool {
    !matches!(key.code, KeyCode::Char(_))
        || !key.modifiers.intersects(
            KeyModifiers::CONTROL
                | KeyModifiers::ALT
                | KeyModifiers::SUPER
                | KeyModifiers::HYPER
                | KeyModifiers::META,
        )
}

/// Convert a char-based cursor position to a byte offset in a string.
fn char_to_byte(s: &str, char_pos: usize) -> usize {
    s.char_indices()
        .nth(char_pos)
        .map(|(byte_idx, _)| byte_idx)
        .unwrap_or(s.len())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        AccountEntry, App, UsageStatus, refresh_fetches_loaded_usage, retained_usage_by_alias,
        with_usage_limiter,
    };
    use super::{ConfirmAction, Tab};
    use crate::{
        claude_usage::AccountInfo,
        usage::{Refresh, UsageInfo},
    };
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::{Terminal, backend::TestBackend};

    fn left_click(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[tokio::test]
    async fn queued_usage_fetch_runs_before_workspace_followup() {
        let limiter = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let (release_first, wait_first) = tokio::sync::oneshot::channel();
        let (release_second, wait_second) = tokio::sync::oneshot::channel();
        let (second_queued, second_queued_rx) = tokio::sync::oneshot::channel();
        let (second_started, second_started_rx) = tokio::sync::oneshot::channel();
        let (workspace_started, mut workspace_started_rx) = tokio::sync::oneshot::channel();

        let first_limiter = limiter.clone();
        let first = tokio::spawn(async move {
            with_usage_limiter(&first_limiter, async {
                wait_first.await.expect("release first usage phase");
            })
            .await;
            with_usage_limiter(&first_limiter, async {
                let _ = workspace_started.send(());
            })
            .await;
        });

        let second_limiter = limiter.clone();
        let second = tokio::spawn(async move {
            let _ = second_queued.send(());
            with_usage_limiter(&second_limiter, async {
                let _ = second_started.send(());
                wait_second.await.expect("release second usage phase");
            })
            .await;
        });

        second_queued_rx.await.expect("second fetch queued");
        tokio::task::yield_now().await;
        let _ = release_first.send(());
        tokio::time::timeout(Duration::from_secs(1), second_started_rx)
            .await
            .expect("queued usage fetch should acquire the released permit")
            .expect("second usage phase started");
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut workspace_started_rx)
                .await
                .is_err(),
            "the first account's workspace lookup must not delay queued usage"
        );
        let _ = release_second.send(());
        tokio::time::timeout(Duration::from_secs(1), &mut workspace_started_rx)
            .await
            .expect("workspace followup should run after usage completes")
            .expect("workspace followup started");
        first.await.expect("first task completes");
        second.await.expect("second task completes");
    }

    fn scroll(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn mouse_click_selects_tab_and_account_row() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "a".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.accounts.push(AccountEntry {
            alias: "b".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices = vec![0, 1];
        app.selected = 0;
        app.hitmap.tabs = vec![
            (
                ratatui::layout::Rect {
                    x: 0,
                    y: 0,
                    width: 14,
                    height: 1,
                },
                Tab::Accounts,
            ),
            (
                ratatui::layout::Rect {
                    x: 16,
                    y: 0,
                    width: 14,
                    height: 1,
                },
                Tab::Settings,
            ),
        ];
        app.hitmap.account_list = Some(crate::tui::hitmap::ListHit {
            rows_area: ratatui::layout::Rect {
                x: 1,
                y: 3,
                width: 40,
                height: 5,
            },
            offset: 0,
            row_count: 2,
        });

        app.handle_mouse(left_click(18, 0));
        assert_eq!(app.active_tab, Tab::Settings);

        app.select_tab(Tab::Accounts);
        app.handle_mouse(left_click(5, 4));
        assert_eq!(app.selected, 1);
    }

    #[tokio::test]
    async fn mouse_double_click_opens_the_selected_account_menu() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "a".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices = vec![0];
        app.hitmap.account_list = Some(crate::tui::hitmap::ListHit {
            rows_area: ratatui::layout::Rect {
                x: 1,
                y: 3,
                width: 40,
                height: 5,
            },
            offset: 0,
            row_count: 1,
        });

        app.handle_mouse(left_click(5, 3));
        assert!(app.menu.is_none());
        app.handle_mouse(left_click(5, 3));
        assert!(app.menu.is_some());
    }

    #[tokio::test]
    async fn mouse_double_click_is_interrupted_by_a_blank_click() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "a".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices = vec![0];
        app.hitmap.account_list = Some(crate::tui::hitmap::ListHit {
            rows_area: ratatui::layout::Rect {
                x: 1,
                y: 3,
                width: 40,
                height: 1,
            },
            offset: 0,
            row_count: 1,
        });

        app.handle_mouse(left_click(5, 3));
        app.handle_mouse(left_click(45, 3));
        app.handle_mouse(left_click(5, 3));
        assert!(app.menu.is_none());
    }

    #[tokio::test]
    async fn mouse_wheel_interrupts_a_pending_double_click() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "a".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices = vec![0];
        app.hitmap.account_list = Some(crate::tui::hitmap::ListHit {
            rows_area: ratatui::layout::Rect {
                x: 1,
                y: 3,
                width: 40,
                height: 1,
            },
            offset: 0,
            row_count: 1,
        });

        app.handle_mouse(left_click(5, 3));
        app.handle_mouse(scroll(MouseEventKind::ScrollDown, 5, 3));
        app.handle_mouse(left_click(5, 3));

        assert!(app.menu.is_none());
    }

    #[tokio::test]
    async fn mouse_keyboard_and_popup_lifecycle_interrupt_pending_double_click() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "a".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices = vec![0];
        app.hitmap.account_list = Some(crate::tui::hitmap::ListHit {
            rows_area: ratatui::layout::Rect {
                x: 1,
                y: 3,
                width: 40,
                height: 1,
            },
            offset: 0,
            row_count: 1,
        });

        app.handle_mouse(left_click(5, 3));
        app.handle_accounts_key(KeyCode::Char('i'));
        app.open_help();
        super::handle_help_key(&mut app, KeyCode::Esc);
        app.handle_mouse(left_click(5, 3));

        assert!(app.menu.is_none());
    }

    #[tokio::test]
    async fn mouse_double_click_does_not_follow_a_reordered_account_row() {
        let mut app = App::new();
        for alias in ["a", "b"] {
            app.accounts.push(AccountEntry {
                alias: alias.into(),
                info: AccountInfo::default(),
                usage: UsageStatus::Idle,
                is_current: false,
            });
        }
        app.view_indices = vec![0, 1];
        app.hitmap.account_list = Some(crate::tui::hitmap::ListHit {
            rows_area: ratatui::layout::Rect {
                x: 1,
                y: 3,
                width: 40,
                height: 1,
            },
            offset: 0,
            row_count: 1,
        });

        app.handle_mouse(left_click(5, 3));
        app.view_indices = vec![1, 0];
        app.handle_mouse(left_click(5, 3));
        assert!(app.menu.is_none());
    }

    #[test]
    fn mouse_wheel_scrolls_logs_and_outside_click_closes_help() {
        let mut app = App::new();
        app.active_tab = Tab::Logs;
        app.log_scroll = 3;
        app.hitmap.logs = Some(ratatui::layout::Rect {
            x: 0,
            y: 1,
            width: 40,
            height: 10,
        });
        app.handle_mouse(scroll(MouseEventKind::ScrollUp, 5, 3));
        assert_eq!(app.log_scroll, 4);
        app.handle_mouse(scroll(MouseEventKind::ScrollDown, 5, 3));
        assert_eq!(app.log_scroll, 3);

        app.open_help();
        app.hitmap.overlay = crate::tui::hitmap::OverlayHit::Dismissible {
            panel: ratatui::layout::Rect {
                x: 10,
                y: 5,
                width: 20,
                height: 10,
            },
        };
        app.handle_mouse(left_click(0, 0));
        assert!(app.help_popup.is_none());
    }

    #[test]
    fn mouse_modal_absorbs_clicks_and_wheel_without_changing_page_state() {
        let mut app = App::new();
        app.active_tab = Tab::Logs;
        app.log_scroll = 3;
        app.help_popup = Some(crate::tui::popup::PopupState::new());
        app.hitmap.logs = Some(ratatui::layout::Rect {
            x: 0,
            y: 1,
            width: 40,
            height: 10,
        });
        app.hitmap.overlay = crate::tui::hitmap::OverlayHit::Modal;

        app.handle_mouse(left_click(5, 3));
        app.handle_mouse(scroll(MouseEventKind::ScrollDown, 5, 3));

        assert_eq!(app.log_scroll, 3);
        assert!(app.help_popup.is_some());
    }

    #[tokio::test]
    async fn mouse_menu_panel_wheel_navigates_and_outside_click_closes() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "a".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices = vec![0];
        app.open_account_menu();
        app.hitmap.overlay = crate::tui::hitmap::OverlayHit::Dismissible {
            panel: ratatui::layout::Rect {
                x: 10,
                y: 5,
                width: 20,
                height: 10,
            },
        };

        app.handle_mouse(scroll(MouseEventKind::ScrollDown, 12, 6));
        let Some(crate::tui::menu::MenuState::Account { popup, .. }) = app.menu.as_ref() else {
            panic!("account menu should remain open");
        };
        assert_eq!(popup.scroll, 1);

        app.handle_mouse(left_click(0, 0));
        assert!(app.menu.is_none());
    }

    #[test]
    fn rendered_settings_field_click_toggles_boolean_and_starts_text_edit() {
        let mut app = App::new();
        app.active_tab = Tab::Settings;
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| crate::tui::ui::render(frame, &mut app))
            .unwrap();

        let (priority, index) = app
            .hitmap
            .settings_fields
            .iter()
            .find(|(_, index)| *index == 6)
            .copied()
            .expect("rendered team_priority hit region");
        assert_eq!(index, 6);
        let before = app.settings.draft.use_cfg.team_priority;
        app.handle_mouse(left_click(priority.x + 1, priority.y));
        assert_eq!(app.settings.focused_index(), 6);
        assert_eq!(app.settings.draft.use_cfg.team_priority, !before);
        assert!(app.settings.is_dirty());
        assert!(!app.settings.is_editing());

        terminal
            .draw(|frame| crate::tui::ui::render(frame, &mut app))
            .unwrap();
        let (url, _) = app
            .hitmap
            .settings_fields
            .iter()
            .find(|(_, index)| *index == 0)
            .copied()
            .expect("rendered proxy.url hit region");
        app.handle_mouse(left_click(url.x + 1, url.y));
        assert_eq!(app.settings.focused_index(), 0);
        assert!(app.settings.is_editing());
    }

    #[test]
    fn rendered_settings_wheel_moves_focus_and_edit_stays_on_tab() {
        let mut app = App::new();
        app.active_tab = Tab::Settings;
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| crate::tui::ui::render(frame, &mut app))
            .unwrap();
        let body = app
            .hitmap
            .settings_body
            .expect("rendered settings body hit region");
        app.handle_mouse(scroll(MouseEventKind::ScrollDown, body.x + 1, body.y + 1));
        assert_eq!(app.settings.focused_index(), 1);

        app.handle_settings_key(KeyCode::Enter);
        assert!(app.settings.is_editing());
        terminal
            .draw(|frame| crate::tui::ui::render(frame, &mut app))
            .unwrap();
        assert!(
            matches!(app.hitmap.overlay, crate::tui::hitmap::OverlayHit::Modal),
            "an active settings edit must not click through"
        );
        let (accounts_tab, _) = app
            .hitmap
            .tabs
            .iter()
            .find(|(_, tab)| *tab == Tab::Accounts)
            .copied()
            .expect("rendered Accounts tab hit region");
        app.handle_mouse(left_click(
            accounts_tab.x + accounts_tab.width / 2,
            accounts_tab.y,
        ));
        assert_eq!(app.active_tab, Tab::Settings);
        assert!(app.settings.is_editing());

        let (priority, _) = app
            .hitmap
            .settings_fields
            .iter()
            .find(|(_, index)| *index == 6)
            .copied()
            .expect("team_priority remains hittable in the map");
        app.handle_mouse(left_click(priority.x + 1, priority.y));
        assert_eq!(app.settings.focused_index(), 6);
        assert!(!app.settings.is_editing());
    }

    #[test]
    fn rendered_settings_footer_save_returns_the_same_key_as_keyboard() {
        let mut app = App::new();
        app.active_tab = Tab::Settings;
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| crate::tui::ui::render(frame, &mut app))
            .unwrap();
        let (area, code) = app
            .hitmap
            .footer_actions
            .iter()
            .find(|(_, code)| *code == KeyCode::Char('s'))
            .copied()
            .expect("rendered settings save action");
        assert_eq!(app.handle_mouse(left_click(area.x, area.y)), Some(code));
    }

    #[test]
    fn rendered_accounts_footer_actions_return_the_same_keys_as_keyboard() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices = vec![0];
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| crate::tui::ui::render(frame, &mut app))
            .unwrap();

        let (area, code) = app
            .hitmap
            .footer_actions
            .iter()
            .find(|(_, code)| *code == KeyCode::Char('u'))
            .copied()
            .expect("rendered use action hit region");
        assert_eq!(app.handle_mouse(left_click(area.x, area.y)), Some(code));
    }

    #[test]
    fn rendered_wrapped_footer_keeps_its_last_action_clickable() {
        let mut app = App::new();
        app.active_tab = Tab::Settings;
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal
            .draw(|frame| crate::tui::ui::render(frame, &mut app))
            .unwrap();

        let (area, _) = app
            .hitmap
            .footer_actions
            .iter()
            .find(|(_, code)| *code == KeyCode::Char('q'))
            .copied()
            .expect("wrapped footer must retain the final quit action");
        assert_eq!(
            app.handle_mouse(left_click(area.x, area.y)),
            Some(KeyCode::Char('q'))
        );
    }

    #[tokio::test]
    async fn rendered_menu_action_click_is_blocked_from_footer_passthrough() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices = vec![0];
        app.open_account_menu();
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| crate::tui::ui::render(frame, &mut app))
            .unwrap();
        let footer = app
            .hitmap
            .footer_actions
            .iter()
            .find(|(_, code)| *code == KeyCode::Char('u'))
            .copied();
        assert!(footer.is_some());
        let result = footer.map(|(area, _)| app.handle_mouse(left_click(area.x, area.y)));
        assert_eq!(result, Some(None));
        assert!(app.menu.is_none());
        assert!(!app.switch_in_flight());
    }

    #[tokio::test]
    async fn rendered_account_menu_action_click_returns_its_keyboard_action() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices = vec![0];
        app.open_account_menu();
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| crate::tui::ui::render(frame, &mut app))
            .unwrap();

        let (area, code) = app
            .hitmap
            .menu_actions
            .iter()
            .find(|(_, code)| *code == KeyCode::Char('u'))
            .copied()
            .expect("rendered use action hit region");
        assert_eq!(app.handle_mouse(left_click(area.x, area.y)), Some(code));
    }

    /// Isolate `PAPER_CLAUDE_SWITCH_HOME`/`CLAUDE_CONFIG_DIR` for tests that touch
    /// profiles or the live login, so they never reach the real `~/.claude`. Serialized via the shared env lock so it can't race sibling
    /// tests that also relocate these variables.
    struct EnvHome {
        _lock: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
        prev_cs: Option<std::ffi::OsString>,
        prev_ch: Option<std::ffi::OsString>,
    }

    impl EnvHome {
        fn new() -> Self {
            let lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let dir = tempfile::tempdir().unwrap();
            let prev_cs = std::env::var_os("PAPER_CLAUDE_SWITCH_HOME");
            let prev_ch = std::env::var_os("CLAUDE_CONFIG_DIR");
            unsafe {
                std::env::set_var("PAPER_CLAUDE_SWITCH_HOME", dir.path());
                std::env::set_var("CLAUDE_CONFIG_DIR", dir.path().join("claude"));
            }
            Self {
                _lock: lock,
                _dir: dir,
                prev_cs,
                prev_ch,
            }
        }
    }

    impl Drop for EnvHome {
        fn drop(&mut self) {
            unsafe {
                match &self.prev_cs {
                    Some(v) => std::env::set_var("PAPER_CLAUDE_SWITCH_HOME", v),
                    None => std::env::remove_var("PAPER_CLAUDE_SWITCH_HOME"),
                }
                match &self.prev_ch {
                    Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
                    None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
                }
            }
        }
    }

    #[test]
    fn failed_initial_profile_read_is_visible_and_does_not_start_refresh() {
        let _home = EnvHome::new();
        let root = crate::auth::app_home().unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("profiles"), "not a directory").unwrap();

        let mut app = App::new();
        assert!(!app.load_profiles());
        assert!(app.accounts.is_empty());
        assert_eq!(app.loading_count(), 0);
        assert!(app.status_is_error);
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| message.contains("Could not load"))
        );
        app.status_expiry = Some(Instant::now() - Duration::from_secs(1));
        app.tick();
        assert!(app.status_msg.is_none());
        assert!(
            app.profile_load_error
                .as_deref()
                .is_some_and(|message| message.contains("Could not load"))
        );
        app.set_status("A later informational message".into(), 5);
        assert!(app.profile_load_error.is_some());
        app.auto_refresh_enabled = true;
        app.next_auto_refresh = Some(Instant::now() - Duration::from_secs(1));
        app.run_due_auto_refresh();
        assert_eq!(app.loading_count(), 0);
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| message.contains("Could not load"))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn accounts_main_dispatch_wires_u_to_switch_selected() {
        let _home = EnvHome::new();
        let path = crate::profile::profile_auth_path("account").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{}").unwrap();

        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices.push(0);

        assert!(app.handle_accounts_key(KeyCode::Char('u')).is_none());
        assert!(app.menu.is_none());
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| message.contains("Switching to account"))
        );
        app.wait_for_switch_completion().await;
    }

    #[test]
    fn launch_is_deferred_while_account_switch_is_in_flight() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices.push(0);
        app.switching_alias = Some("account".into());

        assert!(app.handle_accounts_key(KeyCode::Char('o')).is_none());
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| { message.to_ascii_lowercase().contains("switch") })
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn use_switch_returns_before_a_profile_lock_finishes() {
        let _home = EnvHome::new();
        let path = crate::profile::profile_auth_path("account").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{}").unwrap();

        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices.push(0);

        let lease = crate::profile::lock_launch_session().unwrap();
        let started = std::time::Instant::now();
        app.switch_selected();
        let returned_before_the_lease = started.elapsed() < std::time::Duration::from_millis(100);
        drop(lease);
        app.wait_for_switch_completion().await;

        assert!(
            returned_before_the_lease,
            "a Use event must return control to the TUI while profile switching waits"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn auto_refresh_defers_until_an_account_switch_finishes() {
        let _home = EnvHome::new();
        let path = crate::profile::profile_auth_path("account").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{}").unwrap();

        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices.push(0);
        app.auto_refresh_enabled = true;
        app.next_auto_refresh = Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
        // Keep the refresh task from making a network request if the old
        // implementation reaches refresh_all after waiting on the lock.
        app.usage_limiter = std::sync::Arc::new(tokio::sync::Semaphore::new(0));

        let lease = crate::profile::lock_launch_session().unwrap();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let released_by_releaser = released.clone();
        let releaser = std::thread::spawn(move || {
            let _ = release_rx.recv_timeout(std::time::Duration::from_secs(2));
            released_by_releaser.store(true, std::sync::atomic::Ordering::SeqCst);
            drop(lease);
        });
        app.switch_selected();
        let switch_was_started = app.switch_in_flight();
        let started = std::time::Instant::now();
        let refresh_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            app.run_due_auto_refresh();
        }));
        let returned_before_lock_release = !released.load(std::sync::atomic::Ordering::SeqCst);
        let deferred_deadline = app.next_auto_refresh;

        let _ = release_tx.send(());
        releaser.join().unwrap();
        app.wait_for_switch_completion().await;
        if let Err(payload) = refresh_result {
            std::panic::resume_unwind(payload);
        }

        assert!(
            switch_was_started,
            "the switch must be in flight during refresh"
        );
        assert!(
            returned_before_lock_release,
            "auto-refresh must return while the account switch still owns the profile lock"
        );
        let deferred_deadline = deferred_deadline.expect("a due refresh must be deferred");
        assert!(
            deferred_deadline.saturating_duration_since(started)
                <= std::time::Duration::from_secs(10),
            "switch deferral must use the short retry window instead of the normal interval"
        );
    }

    #[test]
    fn usage_result_rebuilds_an_open_account_detail() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Loading,
            is_current: false,
        });
        app.view_indices.push(0);
        app.refreshing_requests
            .insert("account".into(), (1, Refresh::Cached));
        app.open_account_menu();

        app.result_sender
            .try_send(("account".into(), 1, Ok(UsageInfo::default())))
            .unwrap();
        app.poll_results();
        assert_eq!(app.loading_count(), 0);

        let Some(super::super::menu::MenuState::Account { info, .. }) = app.menu else {
            panic!("account detail should remain open");
        };
        assert!(info.usage.is_some());
    }

    #[test]
    fn stale_usage_result_is_ignored_after_a_new_request_generation_starts() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Loaded(Box::default()),
            is_current: true,
        });
        app.view_indices.push(0);
        app.refreshing_requests
            .insert("account".into(), (2, Refresh::Forced));

        app.result_sender
            .try_send((
                "account".into(),
                1,
                Err(crate::usage::UsageError {
                    summary: "old request".into(),
                    detail: "must be ignored".into(),
                }),
            ))
            .unwrap();
        app.poll_results();

        assert!(matches!(app.accounts[0].usage, UsageStatus::Loaded(_)));
        assert_eq!(app.loading_count(), 1);
    }

    #[test]
    fn forced_follow_up_is_queued_when_usage_request_is_already_in_flight() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Loaded(Box::default()),
            is_current: true,
        });
        app.view_indices.push(0);
        app.refreshing_requests
            .insert("account".into(), (1, Refresh::Cached));

        app.fetch_usage_for(0, Refresh::Forced);

        assert_eq!(
            app.pending_usage_refreshes.get("account"),
            Some(&Refresh::Forced)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn force_refresh_keeps_last_loaded_usage_visible_while_request_is_in_flight() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Loaded(Box::default()),
            is_current: true,
        });
        app.view_indices.push(0);

        app.refresh_indices(&[0], Refresh::Forced);

        assert!(
            matches!(app.accounts[0].usage, UsageStatus::Loaded(_)),
            "force refresh must retain the last value until its replacement arrives"
        );
        assert_eq!(app.loading_count(), 1);
    }

    // ── Using a usage-limit reset ───────────────────────────────────────────
    // Every test installs a claimer that only records its calls: the real claim
    // path would spend a real reset of a real account.

    type Calls = std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>;

    fn usage_with_usable_grant() -> UsageInfo {
        UsageInfo {
            reset_grants: Some(vec![crate::claude_api::ResetGrant {
                id: "g2".into(),
                paused: false,
                clears: Vec::new(),
                label: "Launch reset".into(),
                resets_left: 1,
                resets_total: 1,
                ends_at: Some("2099-10-22T16:00:00Z".into()),
                usable_now: true,
            }]),
            next_reset_grant: Some("g2".into()),
            ..Default::default()
        }
    }

    /// An App with account "work" holding `usage`, a claimer that records its
    /// calls and never finishes, and a usage refresh of "work" already running
    /// (a forced one queues behind it) with a limiter that lets no fetch start.
    fn reset_app(usage: UsageInfo) -> (App, Calls) {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "work".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Loaded(Box::new(usage)),
            is_current: false,
        });
        app.view_indices = vec![0];
        app.usage_limiter = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        app.refreshing_requests
            .insert("work".into(), (1, Refresh::Cached));
        let calls = Calls::default();
        let recorded = calls.clone();
        app.claimer = std::sync::Arc::new(
            move |alias: String, request_id: String| -> super::ClaimFuture {
                recorded.lock().unwrap().push((alias, request_id));
                Box::pin(std::future::pending())
            },
        );
        (app, calls)
    }

    fn status_lower(app: &App) -> String {
        app.status_msg.clone().unwrap_or_default().to_lowercase()
    }

    fn forced_refresh_queued(app: &App) -> bool {
        app.pending_usage_refreshes.get("work") == Some(&Refresh::Forced)
            || matches!(app.refreshing_requests.get("work"), Some((_, Refresh::Forced)))
    }

    async fn wait_for_calls(calls: &Calls, count: usize) {
        for _ in 0..100 {
            if calls.lock().unwrap().len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn claim(result: crate::claude_api::ClaimResult) -> crate::claude_api::ResetClaim {
        crate::claude_api::ResetClaim {
            result,
            resets_left: None,
            cleared: Vec::new(),
            cooldown_until: None,
        }
    }

    /// A1: asking opens a confirmation that names the account and the grant.
    #[tokio::test(flavor = "current_thread")]
    async fn use_reset_asks_for_confirmation_before_claiming() {
        let (mut app, calls) = reset_app(usage_with_usable_grant());

        app.request_use_reset("work");

        assert!(
            matches!(
                &app.confirm,
                Some(ConfirmAction::UseReset { alias, label, resets_left, .. })
                    if alias == "work" && label == "Launch reset" && *resets_left == 1
            ),
            "confirm should be UseReset for work / Launch reset / 1 left"
        );
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| crate::tui::ui::render(frame, &mut app))
            .unwrap();
        let screen = (0..30)
            .map(|y| {
                (0..120)
                    .map(|x| terminal.backend().buffer().cell((x, y)).unwrap().symbol().to_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("Use a reset for 'work'"), "{screen}");
        assert!(screen.contains("(y/n)"), "{screen}");
        assert!(calls.lock().unwrap().is_empty(), "asking must not claim anything");
    }

    /// A2: no usable grant, no question.
    #[tokio::test(flavor = "current_thread")]
    async fn use_reset_without_a_usable_grant_is_refused() {
        let (mut app, calls) = reset_app(UsageInfo::default());

        app.request_use_reset("work");

        assert!(app.confirm.is_none());
        assert!(app.status_is_error);
        assert!(status_lower(&app).contains("no reset"), "{:?}", app.status_msg);
        assert!(calls.lock().unwrap().is_empty());
    }

    /// A3: confirming claims once with a fresh request id; a second ask while it runs is refused.
    #[tokio::test(flavor = "current_thread")]
    async fn confirming_claims_once_and_blocks_a_second_ask() {
        let (mut app, calls) = reset_app(usage_with_usable_grant());
        app.request_use_reset("work");

        assert!(!app.confirm_action());
        wait_for_calls(&calls, 1).await;

        let seen = calls.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "claimer called once: {seen:?}");
        assert_eq!(seen[0].0, "work");
        let id = &seen[0].1;
        assert!(
            id.len() == 32 && id.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
            "request id should be 32 lowercase hex characters: {id}"
        );
        assert!(app.reset_in_flight.contains("work"));
        assert!(
            app.status_msg.as_deref().unwrap_or("").contains("Using a reset"),
            "{:?}",
            app.status_msg
        );

        app.request_use_reset("work");

        assert!(app.confirm.is_none());
        assert!(status_lower(&app).contains("already"), "{:?}", app.status_msg);
    }

    /// A4: a used reset is good news, ends the claim and refreshes the usage.
    #[tokio::test(flavor = "current_thread")]
    async fn a_used_reset_reports_success_and_refreshes() {
        let (mut app, _calls) = reset_app(usage_with_usable_grant());
        app.reset_in_flight.insert("work".into());
        let mut used = claim(crate::claude_api::ClaimResult::Reset);
        used.resets_left = Some(0);

        app.handle_reset_claim_result("work".into(), Ok(used));

        assert!(!app.status_is_error, "{:?}", app.status_msg);
        let status = app.status_msg.clone().unwrap_or_default();
        assert!(status.contains("Reset used"), "{status}");
        assert!(status.contains("0 left"), "{status}");
        assert!(!app.reset_in_flight.contains("work"));
        assert!(forced_refresh_queued(&app));
    }

    /// A5: every refusal is an error that says why, and still refreshes the usage.
    #[tokio::test(flavor = "current_thread")]
    async fn every_refusal_is_an_error_that_says_why() {
        use crate::claude_api::{ClaimError, ClaimResult, UsageError};
        let mut cooldown = claim(ClaimResult::Cooldown);
        cooldown.cooldown_until = Some("2026-10-10T00:00:00Z".into());
        let cases = [
            (Ok(claim(ClaimResult::AlreadyUsed)), "already used"),
            (Ok(claim(ClaimResult::NotLimited)), "not at its limit"),
            (Ok(cooldown), "cooldown"),
            (Ok(claim(ClaimResult::Ineligible)), "not eligible"),
            (Ok(claim(ClaimResult::Unavailable)), "unavailable"),
            (
                Err(ClaimError::Rejected(UsageError::RateLimited { retry_after: None })),
                "rate limited",
            ),
            (Err(ClaimError::Rejected(UsageError::Unauthorized)), "log in"),
        ];
        for (result, expected) in cases {
            let (mut app, _calls) = reset_app(usage_with_usable_grant());
            app.reset_in_flight.insert("work".into());

            app.handle_reset_claim_result("work".into(), result);

            assert!(app.status_is_error, "`{expected}` is an error: {:?}", app.status_msg);
            assert!(status_lower(&app).contains(expected), "`{expected}` in {:?}", app.status_msg);
            assert!(!app.reset_in_flight.contains("work"), "`{expected}` ends the claim");
            assert!(forced_refresh_queued(&app), "`{expected}` queues a forced refresh");
        }
    }

    /// A6: an unknown outcome blocks a new ask until fresh usage arrives, and the
    /// retry reuses the same request id so the server cannot spend two resets.
    #[tokio::test(flavor = "current_thread")]
    async fn an_unknown_outcome_waits_for_a_refresh_and_retries_with_the_same_id() {
        let (mut app, calls) = reset_app(usage_with_usable_grant());
        app.request_use_reset("work");
        app.confirm_action();
        wait_for_calls(&calls, 1).await;
        let first = calls.lock().unwrap()[0].clone();

        app.handle_reset_claim_result(
            "work".into(),
            Err(crate::claude_api::ClaimError::Unknown("timeout".into())),
        );

        assert!(app.status_is_error);
        assert!(status_lower(&app).contains("outcome unknown"), "{:?}", app.status_msg);
        app.status_msg = None;
        app.request_use_reset("work");
        assert!(app.confirm.is_none(), "no new ask while the outcome is unknown");
        assert!(status_lower(&app).contains("refresh"), "{:?}", app.status_msg);

        app.result_sender
            .try_send(("work".into(), 1, Ok(usage_with_usable_grant())))
            .unwrap();
        app.poll_results();

        app.request_use_reset("work");
        assert!(
            matches!(app.confirm, Some(ConfirmAction::UseReset { .. })),
            "fresh usage lets the ask open again"
        );
        app.confirm_action();
        wait_for_calls(&calls, 2).await;
        let seen = calls.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert_eq!(seen[1], first, "the retry reuses the unknown attempt's request id");
    }

    #[test]
    fn profile_reload_retains_loaded_usage_by_alias() {
        let retained = retained_usage_by_alias(vec![AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Loaded(Box::default()),
            is_current: false,
        }]);

        assert!(matches!(
            retained.get("account"),
            Some(UsageStatus::Loaded(_))
        ));
    }

    #[test]
    fn unattended_refresh_refetches_loaded_usage_without_forcing_negative_caches() {
        assert!(refresh_fetches_loaded_usage(Refresh::Unattended));
    }

    #[test]
    fn settings_s_saves_and_accounts_s_still_sorts() {
        let _home = EnvHome::new();
        let mut app = App::new();
        app.handle_settings_key(KeyCode::Char('s'));
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|m| m.contains("Saved config.toml"))
        );

        app.active_tab = Tab::Accounts;
        let before = app.sort_mode;
        app.cycle_sort();
        assert_ne!(app.sort_mode, before);
    }

    #[test]
    fn dirty_settings_require_confirmation_before_quit() {
        let _home = EnvHome::new();
        let mut app = App::new();
        app.active_tab = Tab::Settings;
        app.handle_settings_key(KeyCode::Enter);
        app.handle_settings_key(KeyCode::Char('x'));
        app.handle_settings_key(KeyCode::Enter);
        assert!(app.settings.is_dirty());

        assert!(!app.request_quit());
        assert!(matches!(app.confirm, Some(ConfirmAction::DiscardSettings)));
        assert!(app.confirm_action());
    }

    #[test]
    fn modified_character_keys_do_not_trigger_plain_text_bindings() {
        for code in ['c', 'q', 's'] {
            assert!(!super::accepts_key_event(&KeyEvent::new(
                KeyCode::Char(code),
                KeyModifiers::CONTROL,
            )));
        }
        assert!(super::accepts_key_event(&KeyEvent::new(
            KeyCode::BackTab,
            KeyModifiers::SHIFT,
        )));
    }
}
