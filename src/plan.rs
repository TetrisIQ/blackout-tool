//! `kit plan`: what a selection costs in disk space, and whether it fits.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::catalog::resolve::Resolver;
use crate::catalog::{Catalog, CatalogError, PackageType};
use crate::diskspace::FreeSpace;

/// Safety margin added on top of packages plus temp space, in percent.
pub const MARGIN_PERCENT: u64 = 5;

#[derive(Debug, Clone)]
pub struct Selection {
    /// Shown in the output, e.g. the profile name or `custom`.
    pub label: String,
    pub ids: Vec<String>,
    /// Nominal budget of the profile, if any; exceeding it only warns.
    pub budget_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanItem {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: PackageType,
    pub size_bytes: u64,
    pub version: Option<String>,
    pub license: String,
    /// Size came from the live Kiwix catalog.
    pub live: bool,
    /// Pulled in through `depends_on` rather than selected directly.
    pub dependency: bool,
    /// Download location and checksum; absent for packages without a direct download.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mirrors: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub label: String,
    pub items: Vec<PlanItem>,
    pub total_bytes: u64,
    pub margin_bytes: u64,
    pub required_bytes: u64,
    pub free_bytes: u64,
    pub data_dir: PathBuf,
    pub budget_bytes: Option<u64>,
    pub warnings: Vec<String>,
}

impl Plan {
    pub fn sufficient(&self) -> bool {
        self.free_bytes >= self.required_bytes
    }

    pub fn shortfall_bytes(&self) -> u64 {
        self.required_bytes.saturating_sub(self.free_bytes)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error("cannot determine the size of:\n{}", .0.iter().map(|(id, why)| format!("  - {id}: {why}")).collect::<Vec<_>>().join("\n"))]
    Unresolved(Vec<(String, String)>),
    #[error("cannot determine free space for {}: {source}", path.display())]
    Disk { path: PathBuf, source: std::io::Error },
}

/// Safety margin for `bytes`, rounded up.
pub fn margin_for(bytes: u64) -> u64 {
    (u128::from(bytes) * u128::from(MARGIN_PERCENT)).div_ceil(100) as u64
}

pub fn build(
    catalog: &Catalog,
    selection: &Selection,
    resolver: &dyn Resolver,
    space: &dyn FreeSpace,
    data_dir: &Path,
) -> Result<Plan, PlanError> {
    let manifests = catalog.resolve_selection(&selection.ids)?;

    let mut items = Vec::new();
    let mut failures = Vec::new();
    for m in &manifests {
        match resolver.resolve(m) {
            Ok(r) => items.push(PlanItem {
                id: m.id.clone(),
                name: m.name.clone(),
                kind: m.kind,
                size_bytes: r.size_bytes,
                version: r.version,
                license: m.license.spdx.clone(),
                live: r.live,
                dependency: !selection.ids.contains(&m.id),
                url: r.url,
                mirrors: r.mirrors,
                sha256: r.sha256,
            }),
            Err(e) => failures.push((m.id.clone(), e.to_string())),
        }
    }
    if !failures.is_empty() {
        return Err(PlanError::Unresolved(failures));
    }

    let total_bytes = items.iter().fold(0u64, |acc, i| acc.saturating_add(i.size_bytes));
    // Downloads land in `<name>.part` and are renamed into place, so no extra temp space is needed.
    let margin_bytes = margin_for(total_bytes);
    let required_bytes = total_bytes.saturating_add(margin_bytes);

    let free_bytes = space.available(data_dir).map_err(|source| PlanError::Disk {
        path: data_dir.to_path_buf(),
        source,
    })?;

    let mut warnings = resolver.warnings();
    if let Some(budget) = selection.budget_bytes
        && total_bytes > budget
    {
        warnings.push(format!(
            "selection ({}) exceeds the nominal {} budget of {} by {}",
            format_bytes(total_bytes),
            format_bytes(budget),
            selection.label,
            format_bytes(total_bytes - budget)
        ));
    }
    warnings.extend(variant_warnings(&manifests));

    Ok(Plan {
        label: selection.label.clone(),
        items,
        total_bytes,
        margin_bytes,
        required_bytes,
        free_bytes,
        data_dir: data_dir.to_path_buf(),
        budget_bytes: selection.budget_bytes,
        warnings,
    })
}

/// Warns when several variants of the same Kiwix content (e.g. `nopic` and `maxi`) are selected.
fn variant_warnings(manifests: &[&crate::catalog::Manifest]) -> Vec<String> {
    let mut groups: BTreeMap<(String, String), Vec<&str>> = BTreeMap::new();
    for m in manifests {
        if let Some(k) = &m.source.kiwix {
            groups
                .entry((k.name.clone(), k.lang.clone().unwrap_or_default()))
                .or_default()
                .push(&m.id);
        }
    }
    groups
        .into_iter()
        .filter(|(_, ids)| ids.len() > 1)
        .map(|((name, _), ids)| {
            format!(
                "{} are different variants of the same content ({name}); you probably want only one",
                ids.join(", ")
            )
        })
        .collect()
}

/// Decimal units (1 GB = 10^9 bytes), matching Kiwix and disk vendors.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [(&str, f64); 4] = [("TB", 1e12), ("GB", 1e9), ("MB", 1e6), ("kB", 1e3)];
    for (unit, size) in UNITS {
        if bytes as f64 >= size {
            return format!("{:.1} {unit}", bytes as f64 / size);
        }
    }
    format!("{bytes} B")
}

pub fn render(plan: &Plan) -> String {
    let mut rows: Vec<[String; 5]> = vec![[
        "ID".into(),
        "TYPE".into(),
        "SIZE".into(),
        "VERSION".into(),
        "LICENSE".into(),
    ]];
    for i in &plan.items {
        rows.push([
            if i.dependency {
                format!("{} (dependency)", i.id)
            } else {
                i.id.clone()
            },
            i.kind.as_str().to_string(),
            format_bytes(i.size_bytes),
            i.version
                .as_deref()
                .map_or("-".into(), |v| v.chars().take(10).collect()),
            i.license.clone(),
        ]);
    }
    let widths: Vec<usize> = (0..5)
        .map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0))
        .collect();
    let mut out = format!("Plan: {}\n\n", plan.label);
    for (n, r) in rows.iter().enumerate() {
        out.push_str(&format!(
            "{:<w0$}  {:<w1$}  {:>w2$}  {:<w3$}  {}\n",
            r[0],
            r[1],
            r[2],
            r[3],
            r[4],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
            w3 = widths[3],
        ));
        if n == 0 {
            out.push_str(&format!("{}\n", "-".repeat(widths.iter().sum::<usize>() + 8)));
        }
    }
    let lines = [
        ("Packages".to_string(), plan.total_bytes),
        (format!("Safety margin ({MARGIN_PERCENT}%)"), plan.margin_bytes),
        ("Required".to_string(), plan.required_bytes),
        (format!("Free on {}", plan.data_dir.display()), plan.free_bytes),
    ];
    let lw = lines.iter().map(|(l, _)| l.chars().count()).max().unwrap_or(0);
    out.push('\n');
    for (label, bytes) in &lines {
        out.push_str(&format!("{label:<lw$}  {:>10}\n", format_bytes(*bytes)));
    }
    out.push('\n');
    if plan.sufficient() {
        out.push_str(&format!(
            "OK: fits, {} to spare.\n",
            format_bytes(plan.free_bytes - plan.required_bytes)
        ));
    } else {
        out.push_str(&format!(
            "INSUFFICIENT DISK SPACE: short by {}. Free up space or choose another --data-dir.\n",
            format_bytes(plan.shortfall_bytes())
        ));
    }
    for w in &plan.warnings {
        out.push_str(&format!("warning: {w}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::manifest::{KiwixRef, License, Manifest, Source};
    use crate::catalog::resolve::{ResolveError, Resolved};
    use std::collections::HashMap;
    use std::io;

    struct FixedSpace(u64);
    impl FreeSpace for FixedSpace {
        fn available(&self, _: &Path) -> io::Result<u64> {
            Ok(self.0)
        }
    }

    struct BrokenSpace;
    impl FreeSpace for BrokenSpace {
        fn available(&self, _: &Path) -> io::Result<u64> {
            Err(io::Error::other("boom"))
        }
    }

    /// Resolver with canned sizes per id; ids not listed fail like an unreachable catalog.
    struct FakeResolver {
        sizes: HashMap<&'static str, u64>,
        warn: Vec<String>,
    }
    impl Resolver for FakeResolver {
        fn resolve(&self, m: &Manifest) -> Result<Resolved, ResolveError> {
            match self.sizes.get(m.id.as_str()) {
                Some(&size_bytes) => Ok(Resolved {
                    size_bytes,
                    sha256: None,
                    url: None,
                    mirrors: vec![],
                    version: Some("2026-10".into()),
                    live: m.source.kiwix.is_some(),
                }),
                None => Err(ResolveError::NoSize),
            }
        }
        fn warnings(&self) -> Vec<String> {
            self.warn.clone()
        }
    }

    fn resolver(sizes: &[(&'static str, u64)]) -> FakeResolver {
        FakeResolver {
            sizes: sizes.iter().copied().collect(),
            warn: vec![],
        }
    }

    fn pkg(id: &str, deps: &[&str], kiwix: Option<(&str, &str)>) -> Manifest {
        let mut m = crate::catalog::tests::pkg(id, deps);
        if let Some((name, flavour)) = kiwix {
            m.kind = PackageType::Zim;
            m.sha256 = None;
            m.size_bytes = None;
            m.source = Source {
                kiwix: Some(KiwixRef {
                    name: name.into(),
                    flavour: Some(flavour.into()),
                    lang: Some("deu".into()),
                }),
                ..Default::default()
            };
        }
        m.license = License {
            spdx: "CC-BY-SA-4.0".into(),
            url: None,
            attribution: None,
        };
        m
    }

    fn sel(ids: &[&str], budget: Option<u64>) -> Selection {
        Selection {
            label: "test".into(),
            ids: ids.iter().map(|s| s.to_string()).collect(),
            budget_bytes: budget,
        }
    }

    fn run(pkgs: Vec<Manifest>, s: &Selection, r: &FakeResolver, free: u64) -> Result<Plan, PlanError> {
        let cat = Catalog::from_parts(pkgs, vec![]).unwrap();
        build(&cat, s, r, &FixedSpace(free), Path::new("/data"))
    }

    #[test]
    fn sums_sizes_and_computes_required_space() {
        let r = resolver(&[("a", 1000), ("b", 3000)]);
        let p = run(
            vec![pkg("a", &[], None), pkg("b", &[], None)],
            &sel(&["a", "b"], None),
            &r,
            10_000,
        )
        .unwrap();
        assert_eq!(p.total_bytes, 4000);
        assert_eq!(p.margin_bytes, 200, "5% of 4000");
        assert_eq!(p.required_bytes, 4200);
        assert!(p.sufficient());
        assert_eq!(p.shortfall_bytes(), 0);
    }

    #[test]
    fn margin_rounds_up_and_survives_huge_sizes() {
        let r = resolver(&[("a", 1)]);
        let p = run(vec![pkg("a", &[], None)], &sel(&["a"], None), &r, 100).unwrap();
        assert_eq!(p.required_bytes, 1 + 1, "margin rounds up to 1");

        let r = resolver(&[("a", u64::MAX / 2), ("b", u64::MAX / 2)]);
        let p = run(
            vec![pkg("a", &[], None), pkg("b", &[], None)],
            &sel(&["a", "b"], None),
            &r,
            0,
        )
        .unwrap();
        assert_eq!(p.required_bytes, u64::MAX, "saturates instead of overflowing");
    }

    #[test]
    fn refuses_when_space_is_insufficient() {
        let r = resolver(&[("a", 1000)]);
        let p = run(vec![pkg("a", &[], None)], &sel(&["a"], None), &r, 1000).unwrap();
        assert_eq!(p.required_bytes, 1050);
        assert!(!p.sufficient());
        assert_eq!(p.shortfall_bytes(), 50);
        assert!(render(&p).contains("INSUFFICIENT DISK SPACE"));
    }

    #[test]
    fn exact_fit_is_sufficient() {
        let r = resolver(&[("a", 1000)]);
        let p = run(vec![pkg("a", &[], None)], &sel(&["a"], None), &r, 1050).unwrap();
        assert!(p.sufficient());
    }

    #[test]
    fn dependencies_are_included_and_marked() {
        let r = resolver(&[("app", 10), ("lib", 20)]);
        let p = run(
            vec![pkg("app", &["lib"], None), pkg("lib", &[], None)],
            &sel(&["app"], None),
            &r,
            1000,
        )
        .unwrap();
        assert_eq!(
            p.items
                .iter()
                .map(|i| (i.id.as_str(), i.dependency))
                .collect::<Vec<_>>(),
            vec![("lib", true), ("app", false)]
        );
        assert_eq!(p.total_bytes, 30);
    }

    #[test]
    fn unknown_selection_and_unresolvable_sizes_are_errors() {
        let r = resolver(&[("a", 1)]);
        assert!(matches!(
            run(vec![pkg("a", &[], None)], &sel(&["ghost"], None), &r, 100),
            Err(PlanError::Catalog(CatalogError::UnknownPackage(_)))
        ));
        let r = resolver(&[("a", 1)]);
        match run(
            vec![pkg("a", &[], None), pkg("b", &[], None), pkg("c", &[], None)],
            &sel(&["a", "b", "c"], None),
            &r,
            100,
        ) {
            Err(PlanError::Unresolved(f)) => assert_eq!(
                f.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
                vec!["b", "c"]
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn disk_errors_are_reported() {
        let cat = Catalog::from_parts(vec![pkg("a", &[], None)], vec![]).unwrap();
        let r = resolver(&[("a", 1)]);
        assert!(matches!(
            build(&cat, &sel(&["a"], None), &r, &BrokenSpace, Path::new("/x")),
            Err(PlanError::Disk { .. })
        ));
    }

    #[test]
    fn warns_when_over_budget_but_not_otherwise() {
        let r = resolver(&[("a", 1000)]);
        let over = run(vec![pkg("a", &[], None)], &sel(&["a"], Some(500)), &r, 10_000).unwrap();
        assert!(
            over.warnings.iter().any(|w| w.contains("exceeds")),
            "{:?}",
            over.warnings
        );
        assert!(over.sufficient(), "over budget alone does not refuse");
        let within = run(vec![pkg("a", &[], None)], &sel(&["a"], Some(1000)), &r, 10_000).unwrap();
        assert!(within.warnings.is_empty());
    }

    #[test]
    fn warns_about_two_variants_of_the_same_kiwix_content() {
        let r = resolver(&[("de_mini", 1), ("de_maxi", 2), ("other", 3)]);
        let pkgs = vec![
            pkg("de_mini", &[], Some(("wikipedia_de_all", "mini"))),
            pkg("de_maxi", &[], Some(("wikipedia_de_all", "maxi"))),
            pkg("other", &[], Some(("wiktionary_de_all", "nopic"))),
        ];
        let p = run(pkgs, &sel(&["de_mini", "de_maxi", "other"], None), &r, 1000).unwrap();
        assert_eq!(p.warnings.len(), 1);
        assert!(p.warnings[0].contains("de_mini, de_maxi"));
    }

    #[test]
    fn resolver_warnings_are_passed_through() {
        let mut r = resolver(&[("a", 1)]);
        r.warn.push("stale".into());
        let p = run(vec![pkg("a", &[], None)], &sel(&["a"], None), &r, 100).unwrap();
        assert_eq!(p.warnings, vec!["stale"]);
    }

    #[test]
    fn byte_formatting() {
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1_500), "1.5 kB");
        assert_eq!(format_bytes(18_585_337_585), "18.6 GB");
        assert_eq!(format_bytes(1_000_000_000_000), "1.0 TB");
    }

    #[test]
    fn rendered_table_contains_sizes_and_totals() {
        let r = resolver(&[("a", 18_585_337_585)]);
        let p = run(
            vec![pkg("a", &[], Some(("wikipedia_de_all", "nopic")))],
            &sel(&["a"], None),
            &r,
            100_000_000_000,
        )
        .unwrap();
        let t = render(&p);
        assert!(t.contains("18.6 GB"), "{t}");
        assert!(t.contains("zim"));
        assert!(t.contains("CC-BY-SA-4.0"));
        assert!(t.contains("Required"));
        assert!(t.contains("OK: fits"));
    }
}
