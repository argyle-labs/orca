use regex::Regex;
use std::sync::OnceLock;

/// Strip decorative markdown syntax, producing token-efficient plain text for LLM consumption.
///
/// Preserved: headings, fenced code blocks, inline code, list markers, links (text kept, URL
/// dropped), numbered lists. Stripped: bold, italic (*x*), images, horizontal rules, HTML tags.
/// Multiple consecutive blank lines collapsed to one.
pub fn to_llm_text(input: &str) -> String {
    static BOLD1: OnceLock<Regex> = OnceLock::new();
    static BOLD2: OnceLock<Regex> = OnceLock::new();
    static ITALIC: OnceLock<Regex> = OnceLock::new();
    static IMAGE: OnceLock<Regex> = OnceLock::new();
    static LINK: OnceLock<Regex> = OnceLock::new();
    static HR: OnceLock<Regex> = OnceLock::new();
    static HTML_TAG: OnceLock<Regex> = OnceLock::new();

    let bold1 = BOLD1.get_or_init(|| Regex::new(r"\*\*(.+?)\*\*").expect("bold1 regex"));
    let bold2 = BOLD2.get_or_init(|| Regex::new(r"__(.+?)__").expect("bold2 regex"));
    // Only match *x* when not a list bullet (requires a closing * on the same span)
    let italic = ITALIC.get_or_init(|| Regex::new(r"\*([^*\n]+)\*").expect("italic regex"));
    let image = IMAGE.get_or_init(|| Regex::new(r"!\[.*?\]\(.*?\)").expect("image regex"));
    // Keep link text, drop URL
    let link = LINK.get_or_init(|| Regex::new(r"\[([^\]]+)\]\([^)]+\)").expect("link regex"));
    let hr = HR.get_or_init(|| Regex::new(r"^[-*_]{3,}\s*$").expect("hr regex"));
    let html_tag =
        HTML_TAG.get_or_init(|| Regex::new(r"<[a-zA-Z/][^>]*>").expect("html_tag regex"));

    let mut in_fence = false;
    let mut out = String::with_capacity(input.len());
    let mut consecutive_blanks: usize = 0;

    for line in input.lines() {
        let trimmed = line.trim_start();

        // Toggle fenced code block tracking
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            out.push_str(line);
            out.push('\n');
            consecutive_blanks = 0;
            continue;
        }

        if in_fence {
            out.push_str(line);
            out.push('\n');
            consecutive_blanks = 0;
            continue;
        }

        // Drop horizontal rules entirely
        if hr.is_match(line) {
            continue;
        }

        // Collapse consecutive blank lines
        if line.trim().is_empty() {
            consecutive_blanks += 1;
            if consecutive_blanks == 1 {
                out.push('\n');
            }
            continue;
        }
        consecutive_blanks = 0;

        // Apply inline transformations (images before links to avoid partial matches)
        let s = image.replace_all(line, "");
        let s = link.replace_all(&s, "$1");
        let s = bold1.replace_all(&s, "$1");
        let s = bold2.replace_all(&s, "$1");
        let s = italic.replace_all(&s, "$1");
        let s = html_tag.replace_all(&s, "");

        out.push_str(s.trim_end());
        out.push('\n');
    }

    out
}

#[cfg(test)]
mod tests {
    use super::to_llm_text;

    #[test]
    fn strips_bold_and_italic() {
        assert_eq!(to_llm_text("**bold** and *italic*"), "bold and italic\n");
    }

    #[test]
    fn strips_images() {
        assert_eq!(to_llm_text("see ![diagram](foo.png) here"), "see  here\n");
    }

    #[test]
    fn keeps_link_text() {
        assert_eq!(
            to_llm_text("[read more](https://example.com)"),
            "read more\n"
        );
    }

    #[test]
    fn strips_horizontal_rules() {
        let input = "above\n---\nbelow\n";
        assert_eq!(to_llm_text(input), "above\nbelow\n");
    }

    #[test]
    fn collapses_blank_lines() {
        let input = "a\n\n\n\nb\n";
        assert_eq!(to_llm_text(input), "a\n\nb\n");
    }

    #[test]
    fn preserves_code_fence() {
        let input = "```rust\nlet x = **not bold**;\n```\n";
        assert_eq!(to_llm_text(input), input);
    }

    #[test]
    fn preserves_headings() {
        assert_eq!(to_llm_text("## Section"), "## Section\n");
    }

    #[test]
    fn list_bullets_not_stripped() {
        let input = "* item one\n* item two\n";
        assert_eq!(to_llm_text(input), input);
    }
}
