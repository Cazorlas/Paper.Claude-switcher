/// TUI menu state machines for Phase 2:
///   - Account menu (single-account actions)
///   - Add menu (save the current Claude Code login as a new account)
///   - Re-login menu (save the current Claude Code login into an account)
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
};

use super::popup::{PopupLayout, PopupState, render_popup};
use super::theme::{
    C_CYAN, C_GREEN, C_RED, C_WHITE, C_YELLOW, DIM, base, dim as dim_style, header, key,
};
use ratatui::crossterm::event::KeyCode;

pub struct MenuRender {
    pub panel: Rect,
    pub actions: Vec<(Rect, KeyCode)>,
}

/// Active menu state. Only one menu is visible at a time.
pub enum MenuState {
    /// Account-scoped action menu (Enter on a single account).
    Account {
        info: Box<AccountMenuInfo>,
        popup: PopupState,
    },
    /// Add new account: choose OAuth flow.
    Add { popup: PopupState },
    /// Re-login: choose OAuth flow for an existing account.
    ReloginFlow {
        alias: String,
        email: Option<String>,
        popup: PopupState,
    },
    /// Batch menu shown when one or more accounts are marked.
    Batch { count: usize, popup: PopupState },
}

#[derive(Debug, Clone)]
pub struct AccountMenuInfo {
    pub alias: String,
    pub email: Option<String>,
    pub account_id: Option<String>,
    pub plan_label: String,
    pub plan_type: Option<String>,
    pub is_current: bool,
    pub auth_expiries: Vec<String>,
    pub usage: Option<Box<crate::usage::UsageInfo>>,
    pub usage_meta: Vec<String>,
}

impl AccountMenuInfo {
    /// True when the loaded usage names a grant a reset can be used with.
    fn reset_usable(&self) -> bool {
        self.usage.as_deref().is_some_and(|usage| {
            crate::claude_api::usable_reset_grant(
                usage.reset_grants.as_deref(),
                usage.next_reset_grant.as_deref(),
                crate::auth::now_unix_secs(),
            )
            .is_some()
        })
    }
}

#[derive(Debug, Clone)]
pub enum MenuAction {
    /// Keep the menu open and ignore the key.
    Noop,
    /// Close the menu, no further action.
    Close,
    /// Switch to alias.
    Use(String),
    /// Switch to alias, then start Claude Code.
    Launch(String),
    /// Open re-login flow chooser for alias.
    ReloginRequest(String, Option<String>),
    /// Trigger re-login with chosen flow.
    Relogin { alias: String, device: bool },
    /// Save the current Claude Code login as a new account.
    Add { device: bool },
    /// Refresh usage and model metadata for one account.
    RefreshOne(String),
    /// Open rename input for alias.
    Rename(String),
    /// Request delete confirmation for alias.
    DeleteRequest(String),
    /// Ask to use one usage-limit reset of alias.
    UseReset(String),

    // Batch actions ────────────────────────────
    /// Force-refresh all marked accounts.
    BatchRefresh,
    /// Request batch-delete confirmation.
    BatchDeleteRequest,
}

fn quota_window_lines(
    window: &crate::usage::WindowUsage,
    fallback_label: &str,
) -> Vec<Line<'static>> {
    const BAR_WIDTH: usize = 22;
    let label = match window.window_minutes {
        Some(minutes) if minutes % 1_440 == 0 => format!("{}d", minutes / 1_440),
        Some(minutes) if minutes % 60 == 0 => format!("{}h", minutes / 60),
        Some(minutes) => format!("{minutes}m"),
        None => fallback_label.to_string(),
    };
    let used = window.used_percent.unwrap_or(0.0).clamp(0.0, 100.0);
    let remaining = (100.0 - used).max(0.0);
    let used_width = ((used / 100.0) * BAR_WIDTH as f64).round() as usize;
    let used_color = if used >= 90.0 {
        C_RED
    } else if used >= 70.0 {
        C_YELLOW
    } else {
        C_GREEN
    };
    let window_secs = window
        .window_minutes
        .map(|minutes| minutes.saturating_mul(60))
        .unwrap_or_else(|| {
            if fallback_label == "5h" {
                crate::usage::WINDOW_5H_SECS
            } else {
                crate::usage::WINDOW_7D_SECS
            }
        });
    let pace = crate::usage::pace_percent(window, window_secs);
    let pace_index = pace.map(|value| {
        ((value / 100.0) * BAR_WIDTH as f64)
            .round()
            .clamp(0.0, (BAR_WIDTH - 1) as f64) as usize
    });
    let mut spans = vec![Span::styled(format!("{label:<3} "), base().fg(C_WHITE))];
    for index in 0..BAR_WIDTH {
        let (symbol, style) = if Some(index) == pace_index {
            ("┃", base().fg(C_CYAN).add_modifier(Modifier::BOLD))
        } else if index < used_width {
            ("█", base().fg(used_color))
        } else {
            ("░", base().fg(DIM))
        };
        spans.push(Span::styled(symbol, style));
    }
    spans.push(Span::styled(
        format!("  {remaining:.0}% left"),
        base().fg(if remaining <= 10.0 { C_RED } else { C_YELLOW }),
    ));
    if let Some(pace) = pace {
        let delta = used - pace;
        if delta > 0.0 {
            let seconds = ((delta * window_secs as f64 / 100.0) as i64).max(1);
            spans.push(Span::styled(
                format!(
                    " · {delta:.0}% over pace · rest {}",
                    format_duration(seconds)
                ),
                base().fg(C_YELLOW),
            ));
        }
    }
    let reset_relative = window
        .resets_at
        .map(crate::output::format_reset_time)
        .unwrap_or_else(|| "--".to_string());
    spans.push(Span::styled(
        format!(" · reset {reset_relative}"),
        base().fg(DIM),
    ));
    vec![Line::from(spans)]
}

fn format_duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let days = seconds / 86_400;
    let hours = seconds % 86_400 / 3_600;
    let minutes = seconds % 3_600 / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{}m", minutes.max(1))
    }
}

fn quota_lines(usage: Option<&crate::usage::UsageInfo>) -> Vec<Line<'static>> {
    let Some(usage) = usage else {
        return vec![Line::from(Span::styled("Usage not loaded", base().fg(DIM)))];
    };
    let mut lines = Vec::new();
    let mut add_pool = |name: &str,
                        primary: Option<&crate::usage::WindowUsage>,
                        secondary: Option<&crate::usage::WindowUsage>,
                        unavailable: bool| {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(vec![
            Span::styled(
                name.to_string(),
                base().fg(C_CYAN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if unavailable { "  unavailable" } else { "" },
                base().fg(C_RED),
            ),
        ]));
        if let Some(window) = primary {
            lines.extend(quota_window_lines(window, "5h"));
        }
        if let Some(window) = secondary {
            lines.extend(quota_window_lines(window, "7d"));
        }
        if primary.is_none() && secondary.is_none() {
            lines.push(Line::from(Span::styled(
                "  No active window",
                base().fg(DIM),
            )));
        }
    };
    add_pool(
        "Main",
        usage.primary.as_ref(),
        usage.secondary.as_ref(),
        false,
    );
    for pool in &usage.additional_limits {
        add_pool(
            pool.limit_name.as_deref().unwrap_or("Additional"),
            pool.primary.as_ref(),
            pool.secondary.as_ref(),
            pool.allowed == Some(false) || pool.limit_reached == Some(true),
        );
    }
    lines
}

impl MenuState {
    pub fn account(info: AccountMenuInfo) -> Self {
        MenuState::Account {
            info: Box::new(info),
            popup: PopupState::new(),
        }
    }

    pub fn add() -> Self {
        MenuState::Add {
            popup: PopupState::new(),
        }
    }

    pub fn relogin_flow(alias: String, email: Option<String>) -> Self {
        MenuState::ReloginFlow {
            alias,
            email,
            popup: PopupState::new(),
        }
    }

    pub fn batch(count: usize) -> Self {
        MenuState::Batch {
            count,
            popup: PopupState::new(),
        }
    }

    /// Translate a key press into an action. Returns `Close` to dismiss menu only.
    pub fn handle_key(&mut self, code: ratatui::crossterm::event::KeyCode) -> MenuAction {
        use ratatui::crossterm::event::KeyCode;
        match self {
            MenuState::Account { info, popup } => match code {
                KeyCode::Esc | KeyCode::Char('q') => MenuAction::Close,
                KeyCode::Down | KeyCode::Char('j') => {
                    popup.scroll_down(u16::MAX);
                    MenuAction::Noop
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    popup.scroll_up();
                    MenuAction::Noop
                }
                KeyCode::PageDown => {
                    popup.page_down(5, u16::MAX);
                    MenuAction::Noop
                }
                KeyCode::PageUp => {
                    popup.page_up(5);
                    MenuAction::Noop
                }
                KeyCode::Home => {
                    popup.reset();
                    MenuAction::Noop
                }
                KeyCode::Char('u') => MenuAction::Use(info.alias.clone()),
                KeyCode::Char('o') => MenuAction::Launch(info.alias.clone()),
                KeyCode::Char('l') => {
                    MenuAction::ReloginRequest(info.alias.clone(), info.email.clone())
                }
                KeyCode::Char('n') => MenuAction::Rename(info.alias.clone()),
                KeyCode::Char('r') => MenuAction::RefreshOne(info.alias.clone()),

                KeyCode::Char('d') => MenuAction::DeleteRequest(info.alias.clone()),
                KeyCode::Char('c') if info.reset_usable() => {
                    MenuAction::UseReset(info.alias.clone())
                }
                _ => MenuAction::Noop,
            },
            MenuState::Add { .. } => match code {
                KeyCode::Esc | KeyCode::Char('q') => MenuAction::Close,
                KeyCode::Char('s') | KeyCode::Enter => MenuAction::Add { device: false },
                _ => MenuAction::Noop,
            },
            MenuState::ReloginFlow { alias, .. } => match code {
                KeyCode::Esc | KeyCode::Char('q') => MenuAction::Close,
                KeyCode::Char('s') | KeyCode::Enter => MenuAction::Relogin {
                    alias: alias.clone(),
                    device: false,
                },
                _ => MenuAction::Noop,
            },
            MenuState::Batch { .. } => match code {
                KeyCode::Esc | KeyCode::Char('q') => MenuAction::Close,
                KeyCode::Char('r') => MenuAction::BatchRefresh,
                KeyCode::Char('d') => MenuAction::BatchDeleteRequest,
                _ => MenuAction::Noop,
            },
        }
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect) -> Option<MenuRender> {
        let key_style = key();
        let label_style = base();
        let dim = dim_style();
        let header_style = header();

        match self {
            MenuState::Account { info, popup } => {
                let title = "Account details";
                let mut left_lines = vec![Line::from(Span::styled(
                    "Identity",
                    header_style.add_modifier(Modifier::BOLD),
                ))];
                let mut identity = vec![
                    Span::styled(
                        info.alias.clone(),
                        base().fg(C_WHITE).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("  ", base()),
                    Span::styled(
                        info.plan_label.clone(),
                        base().fg(C_YELLOW).add_modifier(Modifier::BOLD),
                    ),
                ];
                if info.is_current {
                    identity.push(Span::styled(
                        "  ● active",
                        base().fg(C_GREEN).add_modifier(Modifier::BOLD),
                    ));
                }
                left_lines.push(Line::from(identity));
                if let Some(email) = &info.email {
                    left_lines.push(Line::from(vec![
                        Span::styled("email      ", dim),
                        Span::styled(email.clone(), base().fg(C_WHITE)),
                    ]));
                }
                if let Some(plan_type) = &info.plan_type {
                    left_lines.push(Line::from(vec![
                        Span::styled("plan       ", dim),
                        Span::styled(plan_type.clone(), label_style),
                    ]));
                }
                if let Some(account_id) = &info.account_id {
                    left_lines.push(Line::from(vec![
                        Span::styled("account id ", dim),
                        Span::styled(account_id.clone(), dim),
                    ]));
                }
                for expiry in &info.auth_expiries {
                    if let Some((name, details)) = expiry.split_once(" · ") {
                        left_lines.push(Line::from(vec![
                            Span::styled(
                                name.to_string(),
                                base().fg(C_WHITE).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(format!(" · {details}"), dim),
                        ]));
                    } else {
                        left_lines.push(Line::from(Span::styled(expiry.clone(), dim)));
                    }
                }

                left_lines.push(Line::from(""));
                left_lines.push(Line::from(Span::styled(
                    "Quota pools",
                    header_style.add_modifier(Modifier::BOLD),
                )));
                left_lines.extend(quota_lines(info.usage.as_deref()));
                for item in &info.usage_meta {
                    left_lines.push(Line::from(Span::styled(item.clone(), dim)));
                }
                left_lines.push(Line::from(""));
                left_lines.push(Line::from(Span::styled(
                    "Actions",
                    header_style.add_modifier(Modifier::BOLD),
                )));
                let actions = [
                    ("u", "use", true),
                    ("o", "launch", true),
                    ("r", "refresh", true),
                    ("l", "login", true),
                    ("n", "rename", true),
                    ("d", "delete", true),
                    ("c", "reset", info.reset_usable()),
                ];
                for row in [&actions[..4], &actions[4..]] {
                    let mut action_spans = Vec::new();
                    for (idx, (key, label, enabled)) in row.iter().enumerate() {
                        if idx > 0 {
                            action_spans.push(Span::styled("  ·  ", dim));
                        }
                        action_spans.push(Span::styled(
                            (*key).to_string(),
                            if *enabled { key_style } else { dim },
                        ));
                        action_spans.push(Span::styled(
                            format!(" {label}"),
                            if *enabled { label_style } else { dim },
                        ));
                    }
                    left_lines.push(Line::from(action_spans));
                }
                left_lines.push(Line::from(""));
                left_lines.push(Line::from(Span::styled(
                    "j k / arrows / PgUp PgDn scroll details · esc / q cancel",
                    dim,
                )));
                let first_action_line = left_lines.len().saturating_sub(4);
                let layout = render_popup(f, title, &left_lines, popup, area)?;
                let mut hit_actions = Vec::new();
                for (row_offset, row) in [&actions[..4], &actions[4..]].iter().enumerate() {
                    let Some(row_area) = layout.line_rect(first_action_line + row_offset) else {
                        continue;
                    };
                    let mut x = row_area.x;
                    let content_right = row_area.x.saturating_add(row_area.width);
                    for (idx, (key, label, enabled)) in row.iter().enumerate() {
                        if idx > 0 {
                            x = x.saturating_add(5);
                        }
                        let width = u16::try_from(key.len() + 1 + label.len()).unwrap_or(u16::MAX);
                        if *enabled && x < content_right {
                            let visible_width = width.min(content_right.saturating_sub(x));
                            hit_actions.push((
                                Rect::new(x, row_area.y, visible_width, 1),
                                KeyCode::Char(key.chars().next().unwrap()),
                            ));
                        }
                        x = x.saturating_add(width);
                    }
                }
                Some(MenuRender {
                    panel: layout.panel,
                    actions: hit_actions,
                })
            }
            MenuState::Add { popup } => {
                let title = "Add new account";
                let items = [("s", "Save the current Claude Code login")];
                let mut lines: Vec<Line<'static>> = Vec::new();
                lines.push(Line::from(Span::styled(
                    "Log in to Claude Code first (claude, then /login; do not /logout).",
                    header_style,
                )));
                lines.push(Line::from(""));
                lines.extend(menu_items(&items, key_style, label_style));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("esc / q to cancel", dim)));
                render_popup(f, title, &lines, popup, area).map(|layout| MenuRender {
                    panel: layout.panel,
                    actions: hits_for_menu_items(&layout, 2, &items),
                })
            }
            MenuState::ReloginFlow {
                alias,
                email,
                popup,
            } => {
                let header = match email {
                    Some(e) => format!("{alias}  ({e})"),
                    None => alias.clone(),
                };
                let items = [("s", "Save the current Claude Code login into this account")];
                let mut lines: Vec<Line<'static>> =
                    vec![Line::from(Span::styled(header, header_style))];
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "In Claude Code, run /login with this account first.",
                    header_style,
                )));
                lines.push(Line::from(""));
                lines.extend(menu_items(&items, key_style, label_style));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("esc / q to cancel", dim)));
                render_popup(f, "Re-login", &lines, popup, area).map(|layout| MenuRender {
                    panel: layout.panel,
                    actions: hits_for_menu_items(&layout, 4, &items),
                })
            }
            MenuState::Batch { count, popup } => {
                let title = "Batch";
                let header = format!("{count} account(s) marked");
                let items = [
                    ("r", "Refresh selected"),
                    ("d", "Delete selected"),
                ];
                let mut lines: Vec<Line<'static>> = Vec::new();
                lines.push(Line::from(Span::styled(header, header_style)));
                lines.push(Line::from(""));
                lines.extend(menu_items(&items, key_style, label_style));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("esc / q to cancel", dim)));
                render_popup(f, title, &lines, popup, area).map(|layout| MenuRender {
                    panel: layout.panel,
                    actions: hits_for_menu_items(&layout, 2, &items),
                })
            }
        }
    }
}

fn hits_for_menu_items(
    layout: &PopupLayout,
    first_line: usize,
    items: &[(&str, &str)],
) -> Vec<(Rect, KeyCode)> {
    items
        .iter()
        .enumerate()
        .filter_map(|(offset, (key, _))| {
            let ch = key.chars().next()?;
            Some((layout.line_rect(first_line + offset)?, KeyCode::Char(ch)))
        })
        .collect()
}

fn menu_items(items: &[(&str, &str)], key_style: Style, label_style: Style) -> Vec<Line<'static>> {
    let key_w = items
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(1);
    items
        .iter()
        .map(|(k, label)| {
            let pad = key_w.saturating_sub(k.chars().count());
            Line::from(vec![
                Span::styled("  ", base()),
                Span::styled((*k).to_string(), key_style),
                Span::styled(" ".repeat(pad), base()),
                Span::styled("  ", base()),
                Span::styled((*label).to_string(), label_style),
            ])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend, crossterm::event::KeyCode};

    use super::{
        AccountMenuInfo, MenuAction, MenuState, quota_lines,
    };
    use crate::usage::{AdditionalRateLimit, UsageInfo, WindowUsage};

    fn find_text(backend: &TestBackend, needle: &str) -> Option<(u16, u16)> {
        let area = backend.buffer().area;
        for y in 0..area.height {
            let row = (0..area.width)
                .map(|x| {
                    backend
                        .buffer()
                        .cell((x, y))
                        .expect("cell inside test buffer")
                        .symbol()
                })
                .collect::<String>();
            if let Some(x) = row.find(needle) {
                return Some((x as u16, y));
            }
        }
        None
    }

    #[test]
    fn account_menu_launch_action() {
        let mut menu = MenuState::account(AccountMenuInfo {
            alias: "work".into(),
            email: None,
            account_id: None,
            plan_label: "Unknown".into(),
            plan_type: None,
            is_current: false,
            auth_expiries: Vec::new(),
            usage: None,
            usage_meta: Vec::new(),
        });
        assert!(matches!(
            menu.handle_key(KeyCode::Char('o')),
            MenuAction::Launch(alias) if alias == "work"
        ));
    }

    fn grant(id: &str, resets_left: u32) -> crate::claude_api::ResetGrant {
        crate::claude_api::ResetGrant {
            id: id.into(),
            paused: false,
            clears: Vec::new(),
            label: "Launch reset".into(),
            resets_left,
            resets_total: 1,
            ends_at: Some("2099-10-22T16:00:00Z".into()),
            usable_now: true,
        }
    }

    fn reset_menu(usage: Option<UsageInfo>) -> MenuState {
        MenuState::account(AccountMenuInfo {
            alias: "work".into(),
            email: None,
            account_id: None,
            plan_label: "Max".into(),
            plan_type: None,
            is_current: false,
            auth_expiries: Vec::new(),
            usage: usage.map(Box::new),
            usage_meta: Vec::new(),
        })
    }

    fn usage_with_grant(next: Option<&str>) -> UsageInfo {
        UsageInfo {
            reset_grants: Some(vec![grant("g1", 0), grant("g2", 1)]),
            next_reset_grant: next.map(str::to_owned),
            ..Default::default()
        }
    }

    /// Screen text plus the cell column of the first `needle` on any row.
    fn column_of(backend: &TestBackend, needle: &str) -> Option<(u16, u16)> {
        let area = backend.buffer().area;
        for y in 0..area.height {
            let row = (0..area.width)
                .map(|x| backend.buffer().cell((x, y)).unwrap().symbol().to_owned())
                .collect::<String>();
            if let Some(byte) = row.find(needle) {
                return Some((row[..byte].chars().count() as u16, y));
            }
        }
        None
    }

    fn render_menu(menu: &mut MenuState) -> (Terminal<TestBackend>, super::MenuRender) {
        let mut terminal = Terminal::new(TestBackend::new(160, 40)).unwrap();
        let mut rendered = None;
        terminal
            .draw(|frame| {
                rendered = menu.render(frame, frame.area());
            })
            .unwrap();
        (terminal, rendered.expect("menu fits the test terminal"))
    }

    /// M1: with a usable grant, `c` asks to use a reset and the action is drawn and clickable.
    #[test]
    fn c_uses_a_reset_when_a_grant_is_usable() {
        let mut menu = reset_menu(Some(usage_with_grant(Some("g2"))));
        assert!(matches!(
            menu.handle_key(KeyCode::Char('c')),
            MenuAction::UseReset(alias) if alias == "work"
        ));

        let (terminal, rendered) = render_menu(&mut menu);

        assert!(find_text(terminal.backend(), "c reset").is_some(), "popup shows `c reset`");
        assert!(
            rendered.actions.iter().any(|(_, code)| *code == KeyCode::Char('c')),
            "a hit area for `c`: {:?}",
            rendered.actions
        );
    }

    /// M2: without a usable grant `c` does nothing, the action is still drawn but not clickable.
    #[test]
    fn c_does_nothing_without_a_usable_grant() {
        for usage in [None, Some(usage_with_grant(None))] {
            let mut menu = reset_menu(usage);
            assert!(matches!(menu.handle_key(KeyCode::Char('c')), MenuAction::Noop));

            let (terminal, rendered) = render_menu(&mut menu);

            assert!(find_text(terminal.backend(), "c reset").is_some(), "`c reset` still drawn");
            assert!(
                !rendered.actions.iter().any(|(_, code)| *code == KeyCode::Char('c')),
                "no hit area for `c`: {:?}",
                rendered.actions
            );
        }
    }

    /// M3: every enabled key's hit area starts where its `key label` text is drawn.
    #[test]
    fn action_hit_areas_sit_on_their_drawn_labels() {
        let mut menu = reset_menu(Some(usage_with_grant(Some("g2"))));
        let (terminal, rendered) = render_menu(&mut menu);

        for (key, label) in [
            ('u', "use"),
            ('o', "launch"),
            ('r', "refresh"),
            ('l', "login"),
            ('n', "rename"),
            ('d', "delete"),
            ('c', "reset"),
        ] {
            let text = format!("{key} {label}");
            let (x, y) = column_of(terminal.backend(), &text)
                .unwrap_or_else(|| panic!("`{text}` is drawn"));
            let (area, _) = rendered
                .actions
                .iter()
                .find(|(_, code)| *code == KeyCode::Char(key))
                .unwrap_or_else(|| panic!("hit area for `{key}`: {:?}", rendered.actions));
            assert_eq!((area.x, area.y), (x, y), "hit area of `{text}`");
        }
    }

    #[test]
    fn unknown_key_keeps_menu_open() {
        let mut menu = MenuState::add();
        assert!(matches!(
            menu.handle_key(KeyCode::Char('x')),
            MenuAction::Noop
        ));
    }

    #[test]
    fn account_details_navigation_scrolls_popup() {
        let mut menu = MenuState::account(AccountMenuInfo {
            alias: "account".into(),
            email: None,
            account_id: None,
            plan_label: "Unknown".into(),
            plan_type: None,
            is_current: false,
            auth_expiries: Vec::new(),
            usage: None,
            usage_meta: Vec::new(),
        });

        assert!(matches!(menu.handle_key(KeyCode::Down), MenuAction::Noop));
        let MenuState::Account { popup, .. } = menu else {
            unreachable!();
        };
        assert_eq!(popup.scroll, 1);
    }

    #[test]
    fn quota_visuals_include_main_and_future_model_pools() {
        let now = crate::auth::now_unix_secs();
        let window = WindowUsage {
            used_percent: Some(80.0),
            resets_at: Some(now + 2 * 60 * 60),
            window_minutes: Some(300),
        };
        let usage = UsageInfo {
            primary: Some(window.clone()),
            additional_limits: vec![AdditionalRateLimit {
                limit_name: Some("Sonnet-Burst".to_string()),
                metered_feature: Some("future_burst".to_string()),
                primary: Some(window),
                ..Default::default()
            }],
            ..Default::default()
        };
        let text = quota_lines(Some(&usage))
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("Main"));
        assert!(text.contains("Sonnet-Burst"));
        assert!(text.contains('█'));
        assert!(text.contains('┃'));
        assert!(text.contains("20% left"));
        assert!(text.contains("reset"));
        assert!(text.contains("over pace"));
        assert!(!text.contains("Pace"));
        assert!(!text.contains("Rest"));
    }

    #[test]
    fn realistic_account_detail_renders_quota_pools() {
        let now = crate::auth::now_unix_secs();
        let window = WindowUsage {
            used_percent: Some(50.0),
            resets_at: Some(now + 3_600),
            window_minutes: Some(300),
        };
        let usage = UsageInfo {
            primary: Some(window.clone()),
            secondary: Some(window.clone()),
            additional_limits: vec![AdditionalRateLimit {
                limit_name: Some("Sonnet".into()),
                primary: Some(window.clone()),
                secondary: Some(window),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut menu = MenuState::account(AccountMenuInfo {
            alias: "account".into(),
            email: Some("account@example.com".into()),
            account_id: Some("account-id".into()),
            plan_label: "Max".into(),
            plan_type: Some("max".into()),
            is_current: true,
            auth_expiries: vec!["Access token · expires soon".into()],
            usage: Some(Box::new(usage)),
            usage_meta: vec!["  updated now".into()],
        });
        let backend = TestBackend::new(160, 40);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                let _ = menu.render(frame, frame.area());
            })
            .unwrap();

        let pools = find_text(terminal.backend(), "Quota pools").expect("quota heading");
        assert!(pools.0 < 80, "quota pools should follow the account details");
    }
}
