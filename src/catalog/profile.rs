//! Install profiles (`catalog/profiles/<id>.yaml`): named package sets with a nominal size budget.

use serde::{Deserialize, Serialize};

use super::manifest::is_valid_id;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub description: String,
    /// Nominal size the profile is meant to fit in; exceeding it is a warning, not an error.
    pub budget_bytes: u64,
    pub packages: Vec<String>,
}

impl Profile {
    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if !is_valid_id(&self.id) {
            errs.push(format!("id {:?} must match [a-z0-9][a-z0-9_-]*", self.id));
        }
        if self.name.trim().is_empty() {
            errs.push("name must not be empty".into());
        }
        if self.budget_bytes == 0 {
            errs.push("budget_bytes must be greater than 0".into());
        }
        if self.packages.is_empty() {
            errs.push("packages must not be empty".into());
        }
        let mut seen = std::collections::HashSet::new();
        for p in &self.packages {
            if !seen.insert(p) {
                errs.push(format!("packages lists {p:?} more than once"));
            }
        }
        errs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(budget: u64, pkgs: &[&str]) -> Profile {
        Profile {
            id: "mini".into(),
            name: "Mini".into(),
            description: "d".into(),
            budget_bytes: budget,
            packages: pkgs.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn valid_profile() {
        assert!(p(50, &["a", "b"]).validate().is_empty());
    }

    #[test]
    fn invalid_profiles() {
        assert!(!p(0, &["a"]).validate().is_empty());
        assert!(!p(1, &[]).validate().is_empty());
        assert!(!p(1, &["a", "a"]).validate().is_empty());
    }

    #[test]
    fn unknown_field_rejected() {
        let y = "id: mini\nname: M\ndescription: d\nbudget_bytes: 1\npackages: [a]\nextra: 1\n";
        assert!(serde_yaml_ng::from_str::<Profile>(y).is_err());
    }
}
