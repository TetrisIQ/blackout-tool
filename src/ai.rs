//! Automatic choice of the local language model. Pure functions: the hardware and the models'
//! live sizes come in as values, so the rules are tested with tables, not with a real machine.
//!
//! The assistant has to answer quickly, so the rule is "the largest model that runs entirely in
//! fast memory": on the GPU if there is one, otherwise only small models on the CPU.

use crate::catalog::manifest::PackageType;
use crate::catalog::resolve::Resolver;
use crate::catalog::{Catalog, Manifest};
use crate::hw::Hw;

/// Profiles and `--select` may name this instead of a concrete model; kit replaces it with the
/// model that fits the machine.
pub const AUTO_LLM: &str = "llm_auto";

/// Working memory beyond the weights: KV cache at the default context and runtime buffers.
const OVERHEAD_FACTOR_PERCENT: u64 = 125;
const OVERHEAD_FIXED_BYTES: u64 = 500_000_000;
/// A GPU is never filled completely: the desktop and the driver use some.
const GPU_USABLE_PERCENT: u64 = 95;
/// On a CPU the answer speed is bound by memory bandwidth; above this size an answer takes
/// minutes on ordinary hardware.
pub const CPU_FAST_MAX_WEIGHTS: u64 = 3_500_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    pub name: String,
    /// What the registry reports for the whole download.
    pub weights_bytes: u64,
    pub thinking: bool,
}

impl Candidate {
    pub fn working_set_bytes(&self) -> u64 {
        self.weights_bytes / 100 * OVERHEAD_FACTOR_PERCENT + OVERHEAD_FIXED_BYTES
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub id: String,
    pub name: String,
    pub thinking: bool,
    /// One sentence for the user.
    pub reason: String,
}

/// Why nothing could be chosen.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NoModel {
    #[error("there are no model packages in the catalog")]
    NoCandidates,
    #[error(
        "no model fits this machine: the smallest needs about {need} of fast memory and {have} are available. \
         Free some memory or choose a model by hand with --select"
    )]
    TooSmall { need: String, have: String },
}

fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

pub fn choose(hw: &Hw, candidates: &[Candidate]) -> Result<Choice, NoModel> {
    if candidates.is_empty() {
        return Err(NoModel::NoCandidates);
    }
    let mut by_size: Vec<&Candidate> = candidates.iter().collect();
    by_size.sort_by_key(|c| (std::cmp::Reverse(c.weights_bytes), c.id.clone()));
    let pick = |c: &Candidate, reason: String| Choice {
        id: c.id.clone(),
        name: c.name.clone(),
        thinking: c.thinking,
        reason,
    };

    if let Some(gpu) = hw.gpus.first() {
        let usable = gpu.vram_bytes / 100 * GPU_USABLE_PERCENT;
        if let Some(c) = by_size.iter().find(|c| c.working_set_bytes() <= usable) {
            return Ok(pick(
                c,
                format!(
                    "largest model that fits entirely in the {} of {} ({} needed)",
                    gb(gpu.vram_bytes),
                    gpu.name,
                    gb(c.working_set_bytes())
                ),
            ));
        }
    }

    // CPU: only small models are quick, and the free RAM must hold the model.
    let budget = hw.ram_available_bytes / 10 * 6;
    let fits_ram = |c: &&&Candidate| c.working_set_bytes() <= budget;
    if let Some(c) = by_size
        .iter()
        .filter(fits_ram)
        .find(|c| c.weights_bytes <= CPU_FAST_MAX_WEIGHTS)
    {
        let why = if hw.on_gpu() {
            "the graphics memory is too small for the larger models, so the CPU runs this small one"
        } else {
            "no graphics card found; the largest small model that runs quickly on the CPU"
        };
        return Ok(pick(c, why.into()));
    }
    // Slow but possible beats nothing: the smallest model that fits the RAM.
    if let Some(c) = by_size.iter().rev().find(|c| c.working_set_bytes() <= budget) {
        return Ok(pick(
            c,
            "the smallest model that fits the free RAM; answers will be slow on this machine".into(),
        ));
    }
    let smallest = by_size.last().expect("checked non-empty");
    Err(NoModel::TooSmall {
        need: gb(smallest.working_set_bytes()),
        have: gb(hw.model_budget_bytes()),
    })
}

/// Would this model run in fast memory on this machine (the same test `choose` applies)?
pub fn fits_fast(hw: &Hw, c: &Candidate) -> bool {
    let on_gpu = hw
        .gpus
        .first()
        .is_some_and(|g| c.working_set_bytes() <= g.vram_bytes / 100 * GPU_USABLE_PERCENT);
    on_gpu
        || (c.weights_bytes <= CPU_FAST_MAX_WEIGHTS
            && c.working_set_bytes() <= hw.ram_available_bytes / 10 * 6)
}

/// The model packages of the catalog with their live sizes. A package whose size cannot be read
/// (offline, no cache) is left out, with a warning for the caller.
pub fn candidates(catalog: &Catalog, resolver: &dyn Resolver) -> (Vec<Candidate>, Vec<String>) {
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    let models: Vec<&Manifest> = catalog
        .packages()
        .filter(|m| m.kind == PackageType::Model)
        .collect();
    for m in models {
        let Some(spec) = &m.model else { continue };
        match resolver.resolve(m) {
            Ok(r) => out.push(Candidate {
                id: m.id.clone(),
                name: m.name.clone(),
                weights_bytes: r.size_bytes,
                thinking: spec.thinking,
            }),
            Err(e) => warnings.push(format!("{}: size unknown, not considered ({e})", m.id)),
        }
    }
    (out, warnings)
}

/// Replaces [`AUTO_LLM`] in `ids` with the chosen model. Returns a line to show the user, or
/// `None` when the selection did not ask for it.
pub fn expand_auto(
    ids: &mut [String],
    catalog: &Catalog,
    resolver: &dyn Resolver,
    hw: &Hw,
) -> Result<Option<String>, NoModel> {
    if !ids.iter().any(|i| i == AUTO_LLM) {
        return Ok(None);
    }
    let (cands, warnings) = candidates(catalog, resolver);
    let choice = choose(hw, &cands)?;
    for id in ids.iter_mut().filter(|i| i.as_str() == AUTO_LLM) {
        *id = choice.id.clone();
    }
    let mut line = format!("{AUTO_LLM} -> {}: {}", choice.id, choice.reason);
    for w in warnings {
        line.push_str(&format!("\n  note: {w}"));
    }
    Ok(Some(line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw::{Gpu, GpuKind};

    fn cand(id: &str, gb: f64, thinking: bool) -> Candidate {
        Candidate {
            id: id.into(),
            name: id.into(),
            weights_bytes: (gb * 1e9) as u64,
            thinking,
        }
    }

    fn models() -> Vec<Candidate> {
        vec![
            cand("granite_3b", 2.2, true),
            cand("qwen_4b", 3.3, true),
            cand("qwen_9b", 6.6, true),
        ]
    }

    fn hw(ram_free_gb: u64, vram_gb: Option<u64>) -> Hw {
        Hw {
            ram_total_bytes: ram_free_gb * 2 * 1_000_000_000,
            ram_available_bytes: ram_free_gb * 1_000_000_000,
            cpu_model: "test".into(),
            cpu_threads: 8,
            avx2: true,
            gpus: vram_gb
                .map(|v| {
                    vec![Gpu {
                        name: "Test GPU".into(),
                        kind: GpuKind::Nvidia,
                        vram_bytes: v * 1024 * 1024 * 1024,
                    }]
                })
                .unwrap_or_default(),
        }
    }

    fn id(h: &Hw) -> String {
        choose(h, &models()).unwrap().id
    }

    #[test]
    fn gpu_machines_get_the_largest_model_that_fits_the_vram() {
        assert_eq!(id(&hw(20, Some(24))), "qwen_9b");
        assert_eq!(id(&hw(20, Some(12))), "qwen_9b");
        // 8 GiB: the 9B model needs 6.6*1.25+0.5 = 8.75 GB, the 4B model 4.6 GB.
        assert_eq!(id(&hw(20, Some(8))), "qwen_4b");
        assert_eq!(id(&hw(20, Some(4))), "granite_3b");
    }

    #[test]
    fn cpu_machines_only_get_small_models() {
        // Plenty of RAM does not make a 9B model quick on a CPU.
        assert_eq!(id(&hw(64, None)), "qwen_4b");
        assert_eq!(id(&hw(6, None)), "granite_3b");
        let c = choose(&hw(64, None), &models()).unwrap();
        assert!(c.reason.contains("no graphics card"), "{}", c.reason);
    }

    #[test]
    fn a_too_small_gpu_falls_back_to_the_cpu_rule() {
        let c = choose(&hw(16, Some(2)), &models()).unwrap();
        assert_eq!(c.id, "qwen_4b");
        assert!(c.reason.contains("too small"), "{}", c.reason);
    }

    #[test]
    fn a_big_only_catalog_is_slow_but_chosen_when_it_fits_and_refused_when_not() {
        let big = vec![cand("big", 6.6, true)];
        let c = choose(&hw(20, None), &big).unwrap();
        assert!(c.reason.contains("slow"), "{}", c.reason);
        let err = choose(&hw(4, None), &big).unwrap_err();
        assert!(matches!(err, NoModel::TooSmall { .. }));
        assert!(err.to_string().contains("--select"));
        assert_eq!(choose(&hw(4, None), &[]).unwrap_err(), NoModel::NoCandidates);
    }

    #[test]
    fn the_auto_id_is_replaced_and_other_ids_are_untouched() {
        use crate::catalog::resolve::{ResolveError, Resolved};
        struct Sizes;
        impl Resolver for Sizes {
            fn resolve(&self, m: &Manifest) -> Result<Resolved, ResolveError> {
                let gb = if m.id.contains("9b") { 6.6 } else { 3.3 };
                Ok(Resolved {
                    size_bytes: (gb * 1e9) as u64,
                    sha256: None,
                    url: None,
                    mirrors: vec![],
                    version: None,
                    live: true,
                })
            }
        }
        let cat = Catalog::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/catalog_shipped"),
        )
        .unwrap();
        let mut ids = vec!["archwiki_en".to_string(), AUTO_LLM.to_string()];
        let line = expand_auto(&mut ids, &cat, &Sizes, &hw(20, Some(24)))
            .unwrap()
            .unwrap();
        assert_eq!(ids, ["archwiki_en", "llm_qwen35_9b"]);
        assert!(line.starts_with("llm_auto -> llm_qwen35_9b: "), "{line}");
        let mut plain = vec!["archwiki_en".to_string()];
        assert_eq!(
            expand_auto(&mut plain, &cat, &Sizes, &hw(20, None)).unwrap(),
            None
        );
    }
}
