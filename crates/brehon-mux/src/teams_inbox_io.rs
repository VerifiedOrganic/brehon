static TEAMS_INBOX_WRITE_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
    std::sync::OnceLock::new();

fn teams_inbox_write_lock() -> &'static std::sync::Mutex<()> {
    TEAMS_INBOX_WRITE_LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

fn write_inbox_file_atomic(inbox_path: &std::path::Path, content: &str) -> Result<()> {
    let parent = inbox_path
        .parent()
        .ok_or_else(|| Error::terminal(format!("Inbox path has no parent: {inbox_path:?}")))?;
    let file_name = inbox_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::terminal(format!("Inbox path has no file name: {inbox_path:?}")))?;
    let tmp_path = parent.join(format!(
        ".{file_name}.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));

    std::fs::write(&tmp_path, content)?;
    if let Err(err) = std::fs::rename(&tmp_path, inbox_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err.into());
    }
    Ok(())
}

impl TeamsManager {
    /// Write a message to a target agent's inbox file.
    pub fn write_to_inbox(
        &self,
        target: &str,
        from: &str,
        message: &str,
        summary: Option<&str>,
    ) -> Result<()> {
        let resolved_target = self.resolve_member_name(target);
        let resolved_from = self.resolve_member_name(from);
        let resolved_summary = summary.unwrap_or(message).to_string();
        let summary_chars = resolved_summary.chars().count();
        let inbox_path = self
            .paths
            .inbox_for(&resolved_target)
            .map_err(|e| Error::terminal(e.to_string()))?;

        if let Some(parent) = inbox_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let _guard = teams_inbox_write_lock()
            .lock()
            .expect("Teams inbox write lock poisoned");

        if !inbox_path.exists() {
            write_inbox_file_atomic(&inbox_path, "[]")?;
        }

        let content = std::fs::read_to_string(&inbox_path)?;
        let mut messages: Vec<InboxMessage> = match serde_json::from_str(&content) {
            Ok(msgs) => msgs,
            Err(parse_err) => {
                let timestamp = corrupt_inbox_timestamp();
                let corrupt_path = inbox_path
                    .with_file_name(format!("{}.json.corrupt-{}", resolved_target, timestamp));
                std::fs::rename(&inbox_path, &corrupt_path)?;
                tracing::info!(
                    team = %self.team_name,
                    agent = %resolved_target,
                    from = %resolved_from,
                    summary_chars,
                    file_size_after = content.len(),
                    error = %parse_err,
                    "Failed to append message to Teams inbox because inbox JSON was corrupt"
                );
                return Err(Error::corrupt_inbox(
                    &corrupt_path,
                    format!("Inbox JSON parse error: {parse_err}"),
                ));
            }
        };

        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        messages.push(InboxMessage {
            from: resolved_from.clone(),
            text: message.to_string(),
            summary: Some(resolved_summary),
            timestamp: now,
            color: InboxMessageColor::Green.to_string(),
            read: false,
        });

        let serialized = serde_json::to_string_pretty(&messages)?;
        let file_size_after = serialized.len();
        write_inbox_file_atomic(&inbox_path, &serialized)?;

        tracing::info!(
            team = %self.team_name,
            agent = %resolved_target,
            from = %resolved_from,
            summary_chars,
            file_size_after,
            "Wrote message to Teams inbox"
        );
        Ok(())
    }
}
