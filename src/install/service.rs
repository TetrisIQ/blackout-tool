//! The systemd user unit that runs `kiwix-serve`.

use std::path::PathBuf;

use super::tools::s;

pub const UNIT_NAME: &str = "kit-kiwix.service";
/// First line of every unit kit writes; kit refuses to touch a unit without it.
pub const MARKER: &str =
    "# Managed by kit (emergency-kit). Remove with: kit uninstall <package> (the last one removes this unit)";

pub struct UnitSpec {
    pub kiwix_serve: PathBuf,
    pub library_xml: PathBuf,
    pub port: u16,
    /// `kiwix-serve` supports `--monitorLibrary`, so library changes need no restart.
    pub monitor: bool,
}

/// Quotes one `ExecStart=` argument for systemd (spaces, quotes, `%` specifiers, `$` variables).
pub fn quote_exec_arg(arg: &str) -> String {
    let needs_quotes = arg.is_empty() || arg.contains([' ', '\t', '"', '\'', '\\']);
    let escaped = arg.replace('%', "%%").replace('$', "$$");
    if needs_quotes {
        format!("\"{}\"", escaped.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        escaped
    }
}

pub fn render_unit(spec: &UnitSpec) -> String {
    let mut exec = vec![quote_exec_arg(s(&spec.kiwix_serve)), "--library".to_string()];
    if spec.monitor {
        exec.push("--monitorLibrary".into());
    }
    exec.push("--address=127.0.0.1".into());
    exec.push(format!("--port={}", spec.port));
    exec.push(quote_exec_arg(s(&spec.library_xml)));
    format!(
        "{MARKER}\n\
         [Unit]\n\
         Description=Kiwix offline content server (emergency-kit)\n\
         \n\
         [Service]\n\
         ExecStart={}\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exec.join(" ")
    )
}

pub fn is_managed(unit_text: &str) -> bool {
    unit_text.lines().next() == Some(MARKER)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(monitor: bool) -> UnitSpec {
        UnitSpec {
            kiwix_serve: "/usr/bin/kiwix-serve".into(),
            library_xml: "/home/u/.local/share/kit/library/library.xml".into(),
            port: 6589,
            monitor,
        }
    }

    #[test]
    fn unit_text_is_stable() {
        assert_eq!(
            render_unit(&spec(true)),
            format!(
                "{MARKER}\n[Unit]\nDescription=Kiwix offline content server (emergency-kit)\n\n[Service]\n\
                 ExecStart=/usr/bin/kiwix-serve --library --monitorLibrary --address=127.0.0.1 --port=6589 /home/u/.local/share/kit/library/library.xml\n\
                 Restart=on-failure\nRestartSec=2\n\n[Install]\nWantedBy=default.target\n"
            )
        );
    }

    #[test]
    fn without_monitor_support_the_flag_is_omitted() {
        assert!(!render_unit(&spec(false)).contains("monitorLibrary"));
    }

    #[test]
    fn serves_loopback_only() {
        let u = render_unit(&spec(true));
        assert!(u.contains("--address=127.0.0.1") && !u.contains("0.0.0.0"));
    }

    #[test]
    fn exec_arguments_are_quoted_for_systemd() {
        assert_eq!(quote_exec_arg("/plain/path"), "/plain/path");
        assert_eq!(quote_exec_arg("/with space/x"), "\"/with space/x\"");
        assert_eq!(quote_exec_arg("/100%/x"), "/100%%/x");
        assert_eq!(quote_exec_arg("/a$b"), "/a$$b");
        assert_eq!(quote_exec_arg("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_exec_arg(""), "\"\"");
    }

    #[test]
    fn only_marked_units_count_as_ours() {
        assert!(is_managed(&render_unit(&spec(true))));
        assert!(!is_managed("[Unit]\nDescription=someone else\n"));
        assert!(!is_managed(""));
    }
}
