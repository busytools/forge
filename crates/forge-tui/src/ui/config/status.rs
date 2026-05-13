use super::theme;
use crate::app::App;
use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

pub(super) fn render(frame: &mut Frame, area: Rect, app: &App) {
    let lines = status_lines(app);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        area.inner(Margin { vertical: 1, horizontal: 2 }),
    );
}

pub(crate) fn status_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    // ---- Session ----
    section_header(&mut lines, "Session");
    kv_line(&mut lines, "Version", env!("CARGO_PKG_VERSION"));
    kv_line(&mut lines, "Session name", &derive_session_name(app));

    let session_id_str = app.session_id().map_or_else(|| "(none)".to_owned(), |s| s.to_string());
    kv_line(&mut lines, "Session ID", &session_id_str);

    kv_line(&mut lines, "cwd", app.cwd());

    if let Some(branch_text) = app
        .active_session()
        .and_then(|s| s.git_diff_snapshot.as_ref())
        .and_then(|snap| match &snap.branch {
            forge_primitives::git::GitBranch::Named(name) => Some(name.clone()),
            forge_primitives::git::GitBranch::Detached => Some("(detached)".to_owned()),
            forge_primitives::git::GitBranch::NoRepo
            | forge_primitives::git::GitBranch::Unknown => None,
        })
    {
        kv_line(&mut lines, "Git branch", &branch_text);
    }

    lines.push(Line::default());

    // ---- Account ----
    if let Some(account) = app.account_info() {
        section_header(&mut lines, "Account");
        kv_line(&mut lines, "Login method", &login_method_label(&account));
        if let Some(ref provider) = account.api_provider
            && !provider.trim().is_empty()
        {
            kv_line(&mut lines, "API provider", &api_provider_label(provider.trim()));
        }
        if let Some(ref org) = account.organization
            && !org.is_empty()
        {
            kv_line(&mut lines, "Organization", org);
        }
        if let Some(ref email) = account.email
            && !email.is_empty()
        {
            kv_line(&mut lines, "Email", email);
        }
        if let Some(name) = app.active_account_display_name()
            && !name.trim().is_empty()
        {
            kv_line(&mut lines, "Profile", &name);
        }
        if let Some(ref sub) = account.subscription_type
            && !sub.is_empty()
        {
            kv_line(&mut lines, "Subscription", sub);
        }
        lines.push(Line::default());
    }

    // ---- Model ----
    section_header(&mut lines, "Model");
    kv_line(&mut lines, "Model", &model_display(app));
    if let Some(current_model) = app.current_model() {
        kv_line(&mut lines, "Resolved model ID", &current_model.resolved_id);
        if let Some(requested_id) = current_model.requested_id.as_deref()
            && requested_id != current_model.resolved_id
        {
            kv_line(&mut lines, "Requested model", requested_id);
        }
    }

    if let Some(mode) = app.mode() {
        kv_line(&mut lines, "Mode", &mode.current_mode_name);
    }

    lines.push(Line::default());

    // ---- Settings ----
    section_header(&mut lines, "Settings");

    let memory_path = resolve_memory_path(app);
    kv_line(&mut lines, "Memory", &memory_path);

    let sources = setting_sources(app);
    kv_line(&mut lines, "Setting sources", &sources);

    lines
}

fn section_header(lines: &mut Vec<Line<'static>>, title: &str) {
    lines.push(Line::from(Span::styled(
        title.to_owned(),
        Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
    )));
}

fn kv_line(lines: &mut Vec<Line<'static>>, key: &str, value: &str) {
    lines.push(Line::from(vec![
        Span::styled(format!("  {key}: "), Style::default().fg(theme::DIM)),
        Span::styled(value.to_owned(), Style::default().fg(Color::White)),
    ]));
}

fn derive_session_name(app: &App) -> String {
    if let Some(sid) = app.session_id() {
        let sid_str = sid.to_string();
        if let Some(session) = app.recent_sessions().iter().find(|s| s.session_id == sid_str) {
            if let Some(ref title) = session.custom_title
                && !title.trim().is_empty()
            {
                return title.clone();
            }
            if !session.summary.trim().is_empty() {
                let summary = &session.summary;
                return if summary.len() > 60 {
                    format!("{}...", &summary[..57])
                } else {
                    summary.clone()
                };
            }
            if let Some(ref prompt) = session.first_prompt
                && !prompt.trim().is_empty()
            {
                return if prompt.len() > 60 {
                    format!("{}...", &prompt[..57])
                } else {
                    prompt.clone()
                };
            }
        }
    }
    "(unnamed session)".to_owned()
}

fn model_display(app: &App) -> String {
    let Some(current_model) = app.current_model() else {
        return "(not set)".to_owned();
    };
    current_model.display_name_long.clone()
}

pub(crate) fn login_method_label(account: &forge_primitives::AccountInfo) -> String {
    if let Some(ref provider) = account.api_provider
        && !provider.trim().is_empty()
        && provider != "firstParty"
    {
        return "External provider".to_owned();
    }
    if let Some(ref source) = account.api_key_source {
        match source.as_str() {
            "oauth" => return "Claude Max Account".to_owned(),
            "user" => return "User API key".to_owned(),
            "project" => return "Project API key".to_owned(),
            "org" => return "Organization API key".to_owned(),
            "temporary" => return "Temporary key".to_owned(),
            other if !other.is_empty() => return other.to_owned(),
            _ => {}
        }
    }
    if let Some(ref source) = account.token_source
        && !source.is_empty()
    {
        return source.clone();
    }
    "Unknown".to_owned()
}

fn api_provider_label(provider: &str) -> String {
    match provider {
        "firstParty" => "First party".to_owned(),
        "bedrock" => "Bedrock".to_owned(),
        "vertex" => "Vertex".to_owned(),
        "foundry" => "Foundry".to_owned(),
        "anthropicAws" => "Anthropic AWS".to_owned(),
        "mantle" => "Mantle".to_owned(),
        other => other.to_owned(),
    }
}

fn resolve_memory_path(app: &App) -> String {
    let Some(workspace) = app.workspace.as_ref() else {
        return "(no connection)".to_owned();
    };
    let Some(key) = app.active_session_key.as_ref() else {
        return "(no connection)".to_owned();
    };
    let Some(memory_md) = workspace.project_memory_path(key, std::path::Path::new(&app.cwd_raw()))
    else {
        return "(no connection)".to_owned();
    };
    if memory_md.exists() {
        format!("auto memory ({})", memory_md.display())
    } else {
        "(no memory file found)".to_owned()
    }
}

fn setting_sources(app: &App) -> String {
    let mut sources = Vec::new();
    if app.config.settings_path.is_some() {
        sources.push("User settings");
    }
    if app.config.local_settings_path.is_some() {
        sources.push("Project local settings");
    }
    if app.config.preferences_path.is_some() {
        sources.push("Preferences");
    }
    if sources.is_empty() { "(none loaded)".to_owned() } else { sources.join(", ") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_lines_contains_version() {
        let app = App::test_default();
        let text = lines_to_string(&status_lines(&app));
        assert!(text.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn status_lines_shows_cwd() {
        let mut app = App::test_default();
        app.set_cwd("/test/project");
        let text = lines_to_string(&status_lines(&app));
        assert!(text.contains("/test/project"));
    }

    #[test]
    fn status_lines_shows_model() {
        let mut app = App::test_default();
        app.set_current_model(Some(
            crate::agent::model::CurrentModel::new("claude-sonnet-4-7", "Sonnet", "Sonnet 4.7")
                .authoritative(true),
        ));
        let text = lines_to_string(&status_lines(&app));
        assert!(text.contains("Sonnet 4.7"));
    }

    #[test]
    fn status_lines_unnamed_session_fallback() {
        let app = App::test_default();
        let text = lines_to_string(&status_lines(&app));
        assert!(text.contains("(unnamed session)"));
    }

    #[test]
    fn status_lines_uses_custom_title() {
        let mut app = App::test_default();
        app.set_session_id(Some(crate::agent::model::SessionId::new("test-sess-1")));
        *app.recent_sessions_mut() = vec![crate::app::RecentSessionInfo {
            session_id: "test-sess-1".to_owned(),
            summary: String::new(),
            last_modified_ms: 0,
            file_size_bytes: 0,
            cwd: None,
            git_branch: None,
            custom_title: Some("My Custom Title".to_owned()),
            first_prompt: None,
        }];
        let text = lines_to_string(&status_lines(&app));
        assert!(text.contains("My Custom Title"));
    }

    #[test]
    fn section_headers_present() {
        let app = App::test_default();
        let text = lines_to_string(&status_lines(&app));
        assert!(text.contains("Session"));
        assert!(text.contains("Model"));
        assert!(text.contains("Settings"));
    }

    #[test]
    fn login_method_maps_oauth() {
        let account = forge_primitives::AccountInfo {
            api_key_source: Some("oauth".to_owned()),
            ..Default::default()
        };
        assert_eq!(login_method_label(&account), "Claude Max Account");
    }

    #[test]
    fn login_method_maps_user_key() {
        let account = forge_primitives::AccountInfo {
            api_key_source: Some("user".to_owned()),
            ..Default::default()
        };
        assert_eq!(login_method_label(&account), "User API key");
    }

    #[test]
    fn login_method_maps_external_provider() {
        let account = forge_primitives::AccountInfo {
            api_provider: Some("bedrock".to_owned()),
            ..Default::default()
        };
        assert_eq!(login_method_label(&account), "External provider");
    }

    #[test]
    fn status_lines_render_api_provider() {
        let mut app = App::test_default();
        app.set_account_info(Some(forge_primitives::AccountInfo {
            api_provider: Some("mantle".to_owned()),
            ..Default::default()
        }));

        let text = lines_to_string(&status_lines(&app));

        assert!(text.contains("API provider"));
        assert!(text.contains("Mantle"));
    }

    #[test]
    fn login_method_falls_back_to_unknown() {
        let account = forge_primitives::AccountInfo::default();
        assert_eq!(login_method_label(&account), "Unknown");
    }

    #[test]
    fn account_section_renders_profile_when_display_name_set() {
        let mut app = App::test_default();
        app.set_account_info(Some(forge_primitives::AccountInfo {
            email: Some("ved@subspace.network".to_owned()),
            organization: Some("Autonomys".to_owned()),
            subscription_type: Some("team".to_owned()),
            token_source: Some("claude.ai".to_owned()),
            api_key_source: Some("oauth".to_owned()),
            api_provider: Some("firstParty".to_owned()),
        }));
        app.set_active_account_display_name(Some("Subspace".to_owned()));

        let text = lines_to_string(&status_lines(&app));
        assert!(
            text.contains("Profile") && text.contains("Subspace"),
            "expected Profile: Subspace line, got:\n{text}"
        );
        assert!(
            text.contains("Subscription") && text.contains("team"),
            "expected Subscription: team line, got:\n{text}"
        );
    }

    #[test]
    fn account_section_omits_profile_when_display_name_absent() {
        let mut app = App::test_default();
        app.set_account_info(Some(forge_primitives::AccountInfo {
            email: Some("ved@example.com".to_owned()),
            organization: None,
            subscription_type: Some("pro".to_owned()),
            token_source: None,
            api_key_source: None,
            api_provider: None,
        }));
        app.set_active_account_display_name(None);

        let text = lines_to_string(&status_lines(&app));
        assert!(
            !text.contains("Profile"),
            "Profile line must not render when display_name is None, got:\n{text}"
        );
        assert!(
            text.contains("Subscription") && text.contains("pro"),
            "expected Subscription: pro line, got:\n{text}"
        );
    }

    fn lines_to_string(lines: &[Line<'_>]) -> String {
        lines.iter().map(std::string::ToString::to_string).collect::<Vec<_>>().join("\n")
    }
}
