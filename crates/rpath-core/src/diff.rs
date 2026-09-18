use crate::model::{DiffReport, EnvironmentPlan};
use std::collections::{HashMap, HashSet};

pub fn diff_plan_against_current(plan: &EnvironmentPlan) -> DiffReport {
    let current = current_path_entries();
    let planned = plan.path_entries.iter().map(|entry| entry.expanded.clone()).collect::<Vec<_>>();
    diff_path_entries(&current, &planned)
}

pub fn diff_path_entries(current: &[String], planned: &[String]) -> DiffReport {
    let current_keys = current.iter().map(|value| key(value)).collect::<Vec<_>>();
    let planned_keys = planned.iter().map(|value| key(value)).collect::<Vec<_>>();
    let current_set = current_keys.iter().cloned().collect::<HashSet<_>>();
    let planned_set = planned_keys.iter().cloned().collect::<HashSet<_>>();

    let added = planned
        .iter()
        .zip(planned_keys.iter())
        .filter(|(_, key)| !current_set.contains(*key))
        .map(|(value, _)| value.clone())
        .collect::<Vec<_>>();
    let removed = current
        .iter()
        .zip(current_keys.iter())
        .filter(|(_, key)| !planned_set.contains(*key))
        .map(|(value, _)| value.clone())
        .collect::<Vec<_>>();

    let current_positions = current_keys
        .iter()
        .enumerate()
        .map(|(index, value)| (value.clone(), index))
        .collect::<HashMap<_, _>>();
    let reordered = planned
        .iter()
        .zip(planned_keys.iter())
        .enumerate()
        .filter_map(|(index, (value, key))| {
            current_positions
                .get(key)
                .filter(|current_index| **current_index != index)
                .map(|_| value.clone())
        })
        .collect::<Vec<_>>();

    DiffReport {
        added,
        removed,
        reordered,
        unchanged_count: planned_set.intersection(&current_set).count(),
    }
}

fn current_path_entries() -> Vec<String> {
    std::env::var_os(path_var_name())
        .map(|value| {
            std::env::split_paths(&value)
                .map(|path| path.to_string_lossy().to_string())
                .filter(|path| !path.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn path_var_name() -> &'static str {
    if cfg!(windows) {
        "Path"
    } else {
        "PATH"
    }
}

fn key(value: &str) -> String {
    if cfg!(windows) {
        value.trim_end_matches(['\\', '/']).to_ascii_lowercase()
    } else {
        value.trim_end_matches('/').to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::diff_path_entries;

    #[test]
    fn reports_added_removed_and_reordered_paths() {
        let current = vec!["/bin".to_string(), "/usr/bin".to_string()];
        let planned =
            vec!["/usr/bin".to_string(), "/usr/local/bin".to_string(), "/bin".to_string()];

        let diff = diff_path_entries(&current, &planned);

        assert_eq!(diff.added, vec!["/usr/local/bin"]);
        assert!(diff.removed.is_empty());
        assert_eq!(diff.reordered, vec!["/usr/bin", "/bin"]);
        assert_eq!(diff.unchanged_count, 2);
    }
}
