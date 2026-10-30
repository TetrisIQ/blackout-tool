//! The marked block in `/etc/hosts`. Only text between the two marker lines is ever touched,
//! so everything else in the file (and its order) is preserved, and removal is exact.

pub const BEGIN: &str = "# BEGIN kit (emergency-kit) - managed block, remove with: kit net disable";
pub const END: &str = "# END kit (emergency-kit)";

pub fn render_block(hosts: &[String]) -> String {
    let mut sorted: Vec<&String> = hosts.iter().collect();
    sorted.sort();
    sorted.dedup();
    let mut s = format!("{BEGIN}\n");
    for h in sorted {
        s.push_str(&format!("127.0.0.1 {h}\n"));
    }
    s.push_str(END);
    s.push('\n');
    s
}

/// Byte range of the block (including its trailing newline), if there is a complete one.
fn find_block(text: &str) -> Option<(usize, usize)> {
    let start = text.lines().scan(0, |pos, l| {
        let at = *pos;
        *pos += l.len() + 1;
        Some((at, l))
    });
    let mut begin = None;
    for (at, line) in start {
        if line.trim_end() == BEGIN {
            begin = Some(at);
        } else if line.trim_end() == END
            && let Some(b) = begin
        {
            let end = (at + line.len() + 1).min(text.len());
            return Some((b, end));
        }
    }
    None
}

/// `text` with the block set to `hosts` (an empty list removes it). Idempotent.
pub fn apply(text: &str, hosts: &[String]) -> String {
    let without = match find_block(text) {
        Some((b, e)) => format!("{}{}", &text[..b], &text[e..]),
        None => text.to_string(),
    };
    if hosts.is_empty() {
        return without;
    }
    let mut out = without;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&render_block(hosts));
    out
}

/// Hosts currently in the block.
pub fn current(text: &str) -> Vec<String> {
    let Some((b, e)) = find_block(text) else {
        return vec![];
    };
    text[b..e]
        .lines()
        .filter_map(|l| l.strip_prefix("127.0.0.1 "))
        .map(|h| h.trim().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    const BASE: &str = "127.0.0.1 localhost\n::1 localhost\n10.0.0.5 nas\n";

    #[test]
    fn adds_block_and_keeps_foreign_lines() {
        let out = apply(BASE, &h(&["b.example.org", "a.example.org"]));
        assert!(out.starts_with(BASE));
        assert_eq!(current(&out), h(&["a.example.org", "b.example.org"]));
    }

    #[test]
    fn is_idempotent_and_updates_in_place() {
        let once = apply(BASE, &h(&["a.example.org"]));
        assert_eq!(apply(&once, &h(&["a.example.org"])), once);
        let twice = apply(&once, &h(&["a.example.org", "b.example.org"]));
        assert_eq!(twice.matches(BEGIN).count(), 1);
        assert_eq!(current(&twice), h(&["a.example.org", "b.example.org"]));
    }

    #[test]
    fn removal_restores_the_original_exactly() {
        let with = apply(BASE, &h(&["a.example.org"]));
        assert_eq!(apply(&with, &[]), BASE);
        assert_eq!(apply(BASE, &[]), BASE);
    }

    #[test]
    fn lines_after_the_block_survive() {
        let text = format!(
            "{BASE}{}192.168.0.9 printer\n",
            render_block(&h(&["a.example.org"]))
        );
        let out = apply(&text, &h(&["c.example.org"]));
        assert!(out.contains("192.168.0.9 printer\n"));
        assert_eq!(current(&out), h(&["c.example.org"]));
        assert_eq!(apply(&text, &[]), format!("{BASE}192.168.0.9 printer\n"));
    }

    #[test]
    fn missing_final_newline_and_unterminated_block() {
        let out = apply("127.0.0.1 localhost", &h(&["a.example.org"]));
        assert!(out.starts_with("127.0.0.1 localhost\n# BEGIN"));
        // A BEGIN without END is not a block: it is left alone and a new block is appended.
        let broken = format!("{BEGIN}\n127.0.0.1 x.example.org\n");
        let out = apply(&broken, &h(&["a.example.org"]));
        assert!(out.starts_with(&broken));
        assert_eq!(current(&out), h(&["a.example.org"]));
    }
}
