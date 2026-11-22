/// Escape a string as a single POSIX shell word using single quotes.
///
/// Every embedded `'` is replaced with `'\''`. Safe against spaces, `$`,
/// backticks, semicolons, quotes, globs, and newlines.
pub fn shell_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Expand a resume command template by substituting `{id}` with the
/// shell-escaped session ID.
pub fn expand_resume_template(template: &str, session_id: &str) -> String {
    let escaped_id = shell_escape(session_id);
    template.replace("{id}", &escaped_id)
}

/// Build the full shell command executed inside a restored terminal tab:
/// `cd <escaped-cwd> && <resume-command>`.
pub fn build_tab_command(cwd: &str, resume_command: &str) -> String {
    format!("cd {} && {}", shell_escape(cwd), resume_command)
}

/// Escape a string for inclusion inside an AppleScript double-quoted literal.
///
/// Replaces `\`, `"`, and control newlines so the generated AppleScript cannot
/// be broken or injected by unusual paths or session IDs.
pub fn applescript_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_escape_handles_spaces_quotes_and_metachars() {
        assert_eq!(shell_escape("/tmp/simple"), "'/tmp/simple'");
        assert_eq!(
            shell_escape("/Users/me/Security Research/a'b$c`d;rm -rf /"),
            "'/Users/me/Security Research/a'\\''b$c`d;rm -rf /'"
        );
    }

    #[test]
    fn expand_template_escapes_session_id() {
        let cmd = expand_resume_template("claude --resume {id}", "abc-123; touch /tmp/pwn");
        assert_eq!(cmd, "claude --resume 'abc-123; touch /tmp/pwn'");
    }

    #[test]
    fn build_tab_command_escapes_cwd_and_preserves_resume() {
        let resume = expand_resume_template("claude --resume {id}", "5e45591c-e790");
        let full = build_tab_command("/Users/me/My Project's Dir", &resume);
        assert_eq!(
            full,
            "cd '/Users/me/My Project'\\''s Dir' && claude --resume '5e45591c-e790'"
        );
    }

    #[test]
    fn applescript_escape_escapes_backslashes_and_quotes() {
        assert_eq!(
            applescript_escape(r#"cd '/tmp/"a\b'"#),
            r#"cd '/tmp/\"a\\b'"#
        );
    }
}
