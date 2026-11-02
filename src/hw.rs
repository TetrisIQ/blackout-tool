//! Hardware detection for `kit hw` and the automatic choice of a local language model. Everything
//! is read-only: `/proc`, `/sys/class/drm` and a query to `nvidia-smi`. File access and commands go
//! through traits, so tests use recorded fixtures and never look at the host.

use std::fs;
use std::path::Path;

use serde::Serialize;

use crate::install::runner::CommandRunner;

pub trait HwFiles {
    fn read(&self, path: &str) -> Option<String>;
    /// Names of the entries of a directory, sorted.
    fn list(&self, path: &str) -> Vec<String>;
}

pub struct RealFiles;

impl HwFiles for RealFiles {
    fn read(&self, path: &str) -> Option<String> {
        fs::read_to_string(path).ok()
    }

    fn list(&self, path: &str) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(Path::new(path))
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GpuKind {
    Nvidia,
    Amd,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Gpu {
    pub name: String,
    pub kind: GpuKind,
    pub vram_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hw {
    pub ram_total_bytes: u64,
    pub ram_available_bytes: u64,
    pub cpu_model: String,
    pub cpu_threads: u32,
    pub avx2: bool,
    /// Discrete GPUs with their own memory, largest first. Integrated graphics share the RAM and
    /// are not listed.
    pub gpus: Vec<Gpu>,
}

impl Hw {
    /// Memory a model can use at full speed: the largest GPU's VRAM, otherwise 60 % of the RAM
    /// that is free now (the operating system and the other programs keep the rest).
    pub fn model_budget_bytes(&self) -> u64 {
        match self.gpus.first() {
            Some(g) => g.vram_bytes,
            None => self.ram_available_bytes / 10 * 6,
        }
    }

    pub fn on_gpu(&self) -> bool {
        !self.gpus.is_empty()
    }
}

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;

/// `(total, available)` bytes from the text of `/proc/meminfo`.
pub fn parse_meminfo(text: &str) -> Option<(u64, u64)> {
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
            .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
            .map(|kb| kb * KIB)
    };
    let total = field("MemTotal")?;
    Some((total, field("MemAvailable").unwrap_or(total / 2)))
}

/// `(model name, threads, avx2)` from the text of `/proc/cpuinfo`.
pub fn parse_cpuinfo(text: &str) -> (String, u32, bool) {
    let model = text
        .lines()
        .find_map(|l| l.strip_prefix("model name"))
        .and_then(|v| v.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_else(|| "unknown CPU".into());
    let threads = text.lines().filter(|l| l.starts_with("processor")).count() as u32;
    let avx2 = text
        .lines()
        .find(|l| l.starts_with("flags"))
        .is_some_and(|l| l.split_whitespace().any(|f| f == "avx2"));
    (model, threads.max(1), avx2)
}

/// Rows of `nvidia-smi --query-gpu=name,memory.total --format=csv,noheader,nounits` (MiB).
pub fn parse_nvidia_smi(text: &str) -> Vec<Gpu> {
    text.lines()
        .filter_map(|l| {
            let (name, mib) = l.rsplit_once(',')?;
            Some(Gpu {
                name: name.trim().to_string(),
                kind: GpuKind::Nvidia,
                vram_bytes: mib.trim().parse::<u64>().ok()? * MIB,
            })
        })
        .collect()
}

/// AMD cards from `/sys/class/drm`: `vendor` is 0x1002 and `mem_info_vram_total` is in bytes.
/// Integrated AMD graphics report a small carve-out of the RAM (under 1 GiB) and are skipped.
fn amd_gpus(files: &dyn HwFiles) -> Vec<Gpu> {
    files
        .list("/sys/class/drm")
        .into_iter()
        .filter(|n| n.starts_with("card") && n[4..].bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|card| {
            let dir = format!("/sys/class/drm/{card}/device");
            if files.read(&format!("{dir}/vendor"))?.trim() != "0x1002" {
                return None;
            }
            let vram: u64 = files
                .read(&format!("{dir}/mem_info_vram_total"))?
                .trim()
                .parse()
                .ok()?;
            (vram >= 1024 * MIB).then(|| Gpu {
                name: format!("AMD GPU ({card})"),
                kind: GpuKind::Amd,
                vram_bytes: vram,
            })
        })
        .collect()
}

pub fn detect(files: &dyn HwFiles, runner: &dyn CommandRunner) -> Hw {
    let (ram_total_bytes, ram_available_bytes) =
        parse_meminfo(&files.read("/proc/meminfo").unwrap_or_default()).unwrap_or((0, 0));
    let (cpu_model, cpu_threads, avx2) = parse_cpuinfo(&files.read("/proc/cpuinfo").unwrap_or_default());
    let mut gpus = runner
        .run(
            "nvidia-smi",
            &["--query-gpu=name,memory.total", "--format=csv,noheader,nounits"],
        )
        .ok()
        .filter(|o| o.success)
        .map(|o| parse_nvidia_smi(&o.stdout))
        .unwrap_or_default();
    gpus.extend(amd_gpus(files));
    gpus.sort_by_key(|g| std::cmp::Reverse(g.vram_bytes));
    Hw {
        ram_total_bytes,
        ram_available_bytes,
        cpu_model,
        cpu_threads,
        avx2,
        gpus,
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::collections::BTreeMap;

    #[derive(Default)]
    pub struct FakeFiles(pub BTreeMap<String, String>);

    impl FakeFiles {
        pub fn with(mut self, path: &str, text: &str) -> Self {
            self.0.insert(path.into(), text.into());
            self
        }
    }

    impl HwFiles for FakeFiles {
        fn read(&self, path: &str) -> Option<String> {
            self.0.get(path).cloned()
        }

        fn list(&self, path: &str) -> Vec<String> {
            let prefix = format!("{path}/");
            let mut names: Vec<String> = self
                .0
                .keys()
                .filter_map(|k| k.strip_prefix(&prefix)?.split('/').next().map(str::to_string))
                .collect();
            names.dedup();
            names
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeFiles;
    use super::*;
    use crate::install::runner::CmdOutput;
    use crate::install::runner::testing::FakeRunner;

    const MEMINFO: &str =
        "MemTotal:       32768000 kB\nMemFree:         1000000 kB\nMemAvailable:   20971520 kB\n";
    const CPUINFO: &str = "processor\t: 0\nmodel name\t: AMD Ryzen 7 5800X\nflags\t\t: fpu sse avx avx2 fma\n\n\
                           processor\t: 1\nmodel name\t: AMD Ryzen 7 5800X\nflags\t\t: fpu sse avx avx2 fma\n";

    #[test]
    fn parses_memory_cpu_and_nvidia_output() {
        assert_eq!(
            parse_meminfo(MEMINFO),
            Some((32_768_000 * 1024, 20_971_520 * 1024))
        );
        assert_eq!(parse_meminfo("nothing"), None);
        assert_eq!(parse_cpuinfo(CPUINFO), ("AMD Ryzen 7 5800X".into(), 2, true));
        assert!(!parse_cpuinfo("processor : 0\nflags : fpu sse\n").2);
        let gpus = parse_nvidia_smi(
            "NVIDIA GeForce RTX 3070, 8192\nNVIDIA GeForce GTX 1050 Ti, 4096\nbroken line\n",
        );
        assert_eq!(gpus.len(), 2);
        assert_eq!(
            (gpus[0].name.as_str(), gpus[0].vram_bytes),
            ("NVIDIA GeForce RTX 3070", 8192 * MIB)
        );
    }

    fn runner(nvidia: Option<&'static str>) -> FakeRunner {
        FakeRunner::new(move |prog, _| {
            Ok(match (prog, nvidia) {
                ("nvidia-smi", Some(out)) => CmdOutput {
                    success: true,
                    stdout: out.into(),
                    stderr: String::new(),
                },
                _ => CmdOutput::failed("not found"),
            })
        })
    }

    #[test]
    fn detects_a_machine_with_an_nvidia_card() {
        let files = FakeFiles::default()
            .with("/proc/meminfo", MEMINFO)
            .with("/proc/cpuinfo", CPUINFO);
        let hw = detect(&files, &runner(Some("NVIDIA GeForce RTX 3070, 8192\n")));
        assert_eq!(hw.ram_total_bytes, 32_768_000 * 1024);
        assert!(hw.on_gpu() && hw.avx2 && hw.cpu_threads == 2);
        assert_eq!(hw.model_budget_bytes(), 8192 * MIB);
    }

    #[test]
    fn amd_cards_count_but_integrated_graphics_do_not() {
        let files = FakeFiles::default()
            .with("/proc/meminfo", MEMINFO)
            .with("/sys/class/drm/card0/device/vendor", "0x1002\n")
            .with("/sys/class/drm/card0/device/mem_info_vram_total", "17163091968\n")
            .with("/sys/class/drm/card1/device/vendor", "0x1002\n")
            .with("/sys/class/drm/card1/device/mem_info_vram_total", "536870912\n")
            .with("/sys/class/drm/card2/device/vendor", "0x8086\n")
            .with("/sys/class/drm/card2/device/mem_info_vram_total", "17163091968\n");
        let hw = detect(&files, &runner(None));
        assert_eq!(hw.gpus.len(), 1, "{:?}", hw.gpus);
        assert_eq!(
            (hw.gpus[0].kind, hw.gpus[0].vram_bytes),
            (GpuKind::Amd, 17_163_091_968)
        );
    }

    #[test]
    fn without_a_gpu_the_budget_is_sixty_percent_of_the_free_ram() {
        let files = FakeFiles::default()
            .with("/proc/meminfo", MEMINFO)
            .with("/proc/cpuinfo", CPUINFO);
        let hw = detect(&files, &runner(None));
        assert!(!hw.on_gpu());
        assert_eq!(hw.model_budget_bytes(), 20_971_520 * 1024 / 10 * 6);
        // A host where nothing can be read gives an empty, not a panicking, answer.
        let none = detect(&FakeFiles::default(), &runner(None));
        assert_eq!((none.ram_total_bytes, none.model_budget_bytes()), (0, 0));
    }
}
