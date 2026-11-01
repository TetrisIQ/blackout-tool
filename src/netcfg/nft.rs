//! Redirect of `127.77.0.1:443` and `:80` to the unprivileged Caddy port. Lives in its own nftables table,
//! so removing the table removes exactly what kit added and nothing else.

pub const TABLE: &str = "kit_net";
pub const FAMILY: &str = "ip";
pub const HTTPS_PORT: u16 = 443;
use super::hosts::MIRROR_ADDR as ADDR;
pub const HTTP_PORT: u16 = 80;

/// Loaded atomically with `nft -f`. Declaring and deleting the table first makes a reload replace
/// the old rules instead of stacking a second redirect.
pub fn ruleset(https_target: u16, http_target: u16) -> String {
    format!(
        "# Managed by kit (emergency-kit). Remove with: kit net disable\n\
         table {FAMILY} {TABLE}\n\
         delete table {FAMILY} {TABLE}\n\
         table {FAMILY} {TABLE} {{\n\
         \tchain output {{\n\
         \t\ttype nat hook output priority -100; policy accept;\n\
         \t\tip daddr {ADDR} tcp dport {HTTPS_PORT} redirect to :{https_target}\n\
         \t\tip daddr {ADDR} tcp dport {HTTP_PORT} redirect to :{http_target}\n\
         \t}}\n\
         }}\n"
    )
}

/// Arguments of `nft` to read back the table (exit status tells whether it exists).
pub fn list_args() -> [&'static str; 4] {
    ["list", "table", FAMILY, TABLE]
}

pub fn delete_args() -> [&'static str; 4] {
    ["delete", "table", FAMILY, TABLE]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ruleset_only_touches_the_mirror_address() {
        let r = ruleset(8443, 8444);
        assert!(r.contains("ip daddr 127.77.0.1 tcp dport 443 redirect to :8443"));
        assert!(r.contains("table ip kit_net"));
        assert!(r.contains("ip daddr 127.77.0.1 tcp dport 80 redirect to :8444"));
        assert_eq!(r.matches("redirect").count(), 2);
        assert!(r.find("delete table").unwrap() < r.find("chain output").unwrap());
    }
}
