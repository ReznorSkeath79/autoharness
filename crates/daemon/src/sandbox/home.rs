//! Resolve the signed-in account home without trusting an overridable `HOME`.
//!
//! The packaged app can be launched with a temporary `HOME` for acceptance
//! tests. macOS still authenticates providers against the signed-in account,
//! so sandbox deny rules and startup canaries must protect that account's
//! credentials as well as any environment-provided home.

use std::path::PathBuf;

pub(super) fn primary_home() -> Option<PathBuf> {
    account_home().or_else(environment_home)
}

pub(super) fn protected_homes() -> Vec<PathBuf> {
    merge_home_candidates(account_home(), environment_home())
}

fn account_home() -> Option<PathBuf> {
    autoharness_engines::process::account_home()
}

fn environment_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

fn merge_home_candidates(
    account_home: Option<PathBuf>,
    environment_home: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut homes = Vec::with_capacity(2);
    if let Some(home) = account_home {
        homes.push(home);
    }
    if let Some(home) = environment_home
        && !homes.contains(&home)
    {
        homes.push(home);
    }
    homes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_account_and_overridden_environment_homes() {
        assert_eq!(
            merge_home_candidates(
                Some(PathBuf::from("/Users/real-account")),
                Some(PathBuf::from("/tmp/clean-home")),
            ),
            vec![
                PathBuf::from("/Users/real-account"),
                PathBuf::from("/tmp/clean-home"),
            ]
        );
    }

    #[test]
    fn deduplicates_matching_homes() {
        assert_eq!(
            merge_home_candidates(
                Some(PathBuf::from("/Users/dev")),
                Some(PathBuf::from("/Users/dev")),
            ),
            vec![PathBuf::from("/Users/dev")]
        );
    }
}
