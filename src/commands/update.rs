use crate::output::{self, print_json};
use crate::{color, update};
use anyhow::Result;

// ── self-update ──────────────────────────────────────────

pub(crate) async fn self_update_cmd(
    check: bool,
    version: Option<&str>,
    dev: bool,
    stable: bool,
    json: bool,
) -> Result<()> {
    // This fork has no release channel: the upstream one ships codex-switch,
    // which would replace this binary with a build that has no `auto`.
    let _ = (check, version, dev, stable, json);
    if true {
        anyhow::bail!(
            "this binary was run directly; update with `paper-claude-switch self-update` from the npm install, or reinstall (`npm i -g paper-claude-switch@latest` / `cargo install --git https://github.com/Cazorlas/Paper.Claude-switcher`)"
        );
    }
    // Resolve the effective channel:
    // --dev → dev, --stable → stable, otherwise auto-detect from current version.
    let use_dev = if dev {
        true
    } else if stable || version.is_some() {
        false
    } else {
        update::is_dev_version(update::current_version())
    };

    if let Err(error) = update::ensure_legacy_system_install_migrated(use_dev, version) {
        if !json
            && error
                .downcast_ref::<update::LegacySystemInstallMigrationRequired>()
                .is_some()
        {
            output::user_println(&color::warn(&error.to_string()));
            return Err(crate::output::OutputAlreadyReported.into());
        }
        return Err(error);
    }

    if check {
        let current_version = update::current_version().to_string();
        let result = if use_dev {
            update::check_for_dev_update().await?
        } else {
            update::check_for_update(true).await?
        };

        if json {
            let (latest_version, update_available, install_source) = match &result {
                Some(info) => (
                    info.latest_version.clone(),
                    true,
                    info.install_source.as_str().to_string(),
                ),
                None => (
                    current_version.clone(),
                    false,
                    update::detect_install_source().as_str().to_string(),
                ),
            };
            print_json(&output::JsonSelfUpdate {
                ok: true,
                current_version,
                latest_version,
                update_available,
                updated: false,
                install_source,
                action: "checked".into(),
            });
            return Ok(());
        }

        let channel_label = if use_dev { " (dev)" } else { "" };
        match result {
            Some(info) => {
                let homebrew_to_dev = use_dev
                    && info.install_source == update::InstallSource::Homebrew
                    && !update::is_dev_version(&info.current_version);
                let instruction = if homebrew_to_dev {
                    format!("To switch to dev, {}.", update::homebrew_dev_install_hint())
                } else {
                    let hint = if use_dev && dev {
                        // Explicit --dev flag: include it in the hint.
                        "paper-claude-switch self-update --dev"
                    } else if use_dev {
                        // Already on dev (auto-detected): plain self-update stays in dev.
                        "paper-claude-switch self-update"
                    } else if stable {
                        "paper-claude-switch self-update --stable"
                    } else {
                        info.install_source.upgrade_hint()
                    };
                    format!("Run `{hint}`.")
                };
                println!(
                    "{}",
                    color::warn(&format!(
                        "New version available{channel_label}: v{} (current v{}). {instruction}",
                        info.latest_version, info.current_version
                    ))
                );
            }
            None => {
                println!(
                    "{}",
                    color::success(&format!(
                        "Already up to date{channel_label}: v{}",
                        update::current_version()
                    ))
                );
            }
        }
        return Ok(());
    }

    let show_progress = !json && update::should_show_download_progress();
    let result = if use_dev {
        update::self_update_dev(show_progress).await
    } else {
        update::self_update(version, show_progress).await
    };
    let result = result?;

    if json {
        print_json(&output::JsonSelfUpdate {
            ok: true,
            current_version: result.current_version.clone(),
            latest_version: result.latest_version.clone(),
            update_available: result.updated,
            updated: result.updated,
            install_source: result.install_source.as_str().to_string(),
            action: if result.updated {
                "updated".into()
            } else {
                "up_to_date".into()
            },
        });
        return Ok(());
    }

    if result.updated {
        let channel_label = if use_dev { " (dev)" } else { "" };
        println!(
            "{}",
            color::success(&format!(
                "Updated paper-claude-switch{channel_label}: v{} -> v{}",
                result.current_version, result.latest_version
            ))
        );
        if dev && !update::is_dev_version(&result.current_version) {
            output::user_println(&color::dim(
                "Switched to dev channel. Run `paper-claude-switch self-update --stable` to return.",
            ));
        } else if stable && update::is_dev_version(&result.current_version) {
            output::user_println(&color::dim("Switched back to stable channel."));
        }
    } else {
        println!(
            "{}",
            color::success(&format!("Already up to date: v{}", result.current_version))
        );
    }

    Ok(())
}
