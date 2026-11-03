//! Running external programs (`kiwix-manage`, `systemctl --user`, ...) behind a trait, so tests
//! never execute anything on the host.

use std::io;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct CmdOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOutput {
    pub fn ok() -> Self {
        Self {
            success: true,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    pub fn failed(stderr: &str) -> Self {
        Self {
            success: false,
            stdout: String::new(),
            stderr: stderr.to_string(),
        }
    }
}

pub trait CommandRunner {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<CmdOutput>;

    /// Long-running command whose output the user should watch (a crawl). Returns whether it
    /// succeeded. The default captures the output like `run`.
    fn run_visible(&self, program: &str, args: &[&str]) -> io::Result<bool> {
        Ok(self.run(program, args)?.success)
    }
}

pub struct RealRunner;

impl CommandRunner for RealRunner {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<CmdOutput> {
        let out = Command::new(program).args(args).output()?;
        Ok(CmdOutput {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn run_visible(&self, program: &str, args: &[&str]) -> io::Result<bool> {
        Ok(Command::new(program).args(args).status()?.success())
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::sync::Mutex;

    type Handler = Box<dyn Fn(&str, &[&str]) -> io::Result<CmdOutput>>;

    /// Records every command line and answers through a handler.
    pub struct FakeRunner {
        pub calls: Mutex<Vec<String>>,
        handler: Handler,
    }

    impl FakeRunner {
        pub fn new(handler: impl Fn(&str, &[&str]) -> io::Result<CmdOutput> + 'static) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                handler: Box::new(handler),
            }
        }

        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        /// Calls whose program (basename) and arguments contain `needle`.
        pub fn count(&self, needle: &str) -> usize {
            self.calls().iter().filter(|c| c.contains(needle)).count()
        }
    }

    /// A healthy host: `kiwix-manage` appends a line per ZIM to the library file, and the service
    /// is active exactly between `enable --now` and `disable --now`. `monitor` says whether
    /// `kiwix-serve --help` mentions `--monitorLibrary`.
    pub fn system(monitor: bool) -> FakeRunner {
        let active = std::rc::Rc::new(std::cell::Cell::new(false));
        FakeRunner::new(move |prog, args| match prog {
            "kiwix-manage" => {
                let mut text = std::fs::read_to_string(args[0]).unwrap_or_default();
                text.push_str(&format!("<book path=\"{}\"/>\n", args[2]));
                std::fs::write(args[0], text).unwrap();
                Ok(CmdOutput::ok())
            }
            "kiwix-serve" => Ok(CmdOutput {
                success: true,
                stdout: if monitor {
                    "  -M, --monitorLibrary\n".into()
                } else {
                    String::new()
                },
                stderr: String::new(),
            }),
            "systemctl" => {
                if args.contains(&"enable") {
                    active.set(true);
                } else if args.contains(&"disable") {
                    active.set(false);
                } else if args.contains(&"is-active") {
                    return Ok(if active.get() {
                        CmdOutput::ok()
                    } else {
                        CmdOutput::failed("inactive")
                    });
                }
                Ok(CmdOutput::ok())
            }
            _ => Ok(CmdOutput::ok()),
        })
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, program: &str, args: &[&str]) -> io::Result<CmdOutput> {
            let base = program.rsplit('/').next().unwrap_or(program);
            self.calls
                .lock()
                .unwrap()
                .push(format!("{base} {}", args.join(" ")));
            (self.handler)(base, args)
        }
    }
}
