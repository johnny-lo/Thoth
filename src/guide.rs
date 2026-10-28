use crate::output;

/// The commander guide, embedded at compile time so it always matches the
/// executable that prints it. The single source is the plugin's skill file.
const SKILL: &str = include_str!("../plugins/thoth/skills/thoth-dispatch/SKILL.md");

/// Drops a leading YAML frontmatter block (first line `---` through the next
/// line that is exactly `---`). Text without frontmatter is returned as is.
fn strip_frontmatter(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("---\n") else {
        return text;
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        offset += line.len();
        if line.trim_end_matches('\n') == "---" {
            return &rest[offset..];
        }
    }
    text
}

/// True when the whole line is a single HTML comment.
fn is_comment_line(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 7
        && t.starts_with("<!--")
        && t.ends_with("-->")
        && t.matches("-->").count() == 1
        && t.matches("<!--").count() == 1
}

fn strip_comment_lines(text: &str) -> String {
    text.split_inclusive('\n')
        .filter(|line| !is_comment_line(line))
        .collect()
}

fn render(source: &str) -> String {
    let text = source.replace("\r\n", "\n");
    let body = strip_comment_lines(strip_frontmatter(&text));
    body.trim().to_owned()
}

/// The skill file exactly as built into this executable.
pub fn source() -> &'static str {
    SKILL
}

/// The guide text, without frontmatter or comment lines, no trailing newline.
pub fn text() -> String {
    render(SKILL)
}

/// Prints the guide. Reads no settings, opens no database, touches no network.
pub fn run(json: bool) -> anyhow::Result<i32> {
    let guide = text();
    let doc = serde_json::json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "guide": guide,
    });
    output::emit(json, &doc, &guide);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_is_removed_and_only_the_rest_is_kept() {
        let text = "---\nname: x\ndescription: y\n---\n\n# Title\nbody\n";
        assert_eq!(strip_frontmatter(text), "\n# Title\nbody\n");
    }

    #[test]
    fn text_without_frontmatter_is_returned_unchanged() {
        let text = "# Title\n---\nnot frontmatter\n";
        assert_eq!(strip_frontmatter(text), text);
    }

    #[test]
    fn a_line_that_is_one_html_comment_is_deleted() {
        let text = "a\n<!-- note -->\n  <!-- indented -->  \nb\n";
        assert_eq!(strip_comment_lines(text), "a\nb\n");
    }

    #[test]
    fn a_comment_sharing_its_line_with_other_text_is_kept() {
        let text = "before <!-- note -->\n<!-- note --> after\n<!-- a --> <!-- b -->\nend\n";
        assert_eq!(strip_comment_lines(text), text);
    }

    #[test]
    fn the_embedded_guide_starts_at_the_title() {
        let guide = text();
        assert!(guide.starts_with("# "));
        assert!(guide.contains("agy-a"));
        assert!(!guide.contains("\r"));
        assert!(!guide.lines().any(|l| l.starts_with("name:")));
    }
}
