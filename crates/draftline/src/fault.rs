//! Deterministic crash points for testing recovery, behind the
//! `fault-injection` feature (never enable it in a shipped host).
//!
//! Each writing operation is named by the lock it takes (`save_version`,
//! `switch_variation`, `move_folder`, `send`, `publish_changes`,
//! `repair_recovery`, ...) and passes through the [`POINTS`] it reaches. With
//! the feature on:
//!
//! - `DRAFTLINE_FAULT_TRACE=<file>` appends one line per point reached,
//!   `<operation>/<point>#<n>`, where `n` counts that point's hits in the
//!   process (1-based). Points outside a named operation report `-`.
//! - `DRAFTLINE_FAULT=<operation>/<point>#<n>` aborts the process, with no
//!   cleanup, the `n`th time that point is reached (`#<n>` defaults to 1;
//!   `*` matches any operation).
//!
//! So a harness runs an operation once with the trace, then reruns it in a
//! fresh process once per traced line with `DRAFTLINE_FAULT` set to it, and
//! checks recovery after each crash. Without the feature every hook compiles
//! to nothing.

#[cfg(feature = "fault-injection")]
mod imp {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::io::Write;
    use std::sync::Mutex;

    thread_local! {
        static OPERATION: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    static HITS: Mutex<Option<HashMap<String, u32>>> = Mutex::new(None);

    pub struct Operation {
        outermost: bool,
    }

    impl Drop for Operation {
        fn drop(&mut self) {
            if self.outermost {
                OPERATION.with(|operation| *operation.borrow_mut() = None);
            }
        }
    }

    pub fn operation(name: &str) -> Operation {
        let outermost = OPERATION.with(|operation| {
            let mut operation = operation.borrow_mut();
            if operation.is_some() {
                return false;
            }
            *operation = Some(name.to_string());
            true
        });
        Operation { outermost }
    }

    pub fn point(point: &str) {
        let operation = OPERATION.with(|operation| operation.borrow().clone());
        let key = format!("{}/{point}", operation.as_deref().unwrap_or("-"));
        let hit = {
            let mut hits = HITS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let count = hits
                .get_or_insert_with(HashMap::new)
                .entry(key.clone())
                .or_default();
            *count += 1;
            *count
        };
        if let Some(path) = std::env::var_os("DRAFTLINE_FAULT_TRACE") {
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(file, "{key}#{hit}");
            }
        }
        let Ok(target) = std::env::var("DRAFTLINE_FAULT") else {
            return;
        };
        let (name, nth) = match target.rsplit_once('#') {
            Some((name, nth)) => (name, nth.parse().unwrap_or(0)),
            None => (target.as_str(), 1),
        };
        let matches = match name.split_once('/') {
            Some(("*", wanted)) => wanted == point,
            Some(_) => name == key,
            None => false,
        };
        if matches && nth == hit {
            eprintln!("draftline: DRAFTLINE_FAULT={target}: aborting");
            std::process::abort();
        }
    }
}

#[cfg(not(feature = "fault-injection"))]
mod imp {
    pub struct Operation;

    #[inline(always)]
    pub fn operation(_name: &str) -> Operation {
        Operation
    }

    #[inline(always)]
    pub fn point(_point: &str) {}
}

/// Names the public operation in progress on this thread for crash points;
/// the outermost one wins while the returned guard lives.
pub(crate) use imp::Operation;

/// Every crash point, roughly in the order an operation reaches them.
pub const POINTS: &[&str] = &[
    // Before an operation that moves files or refs, Draftline records what
    // it is about to do; repair and rollback start from this record.
    "recovery_recorded",
    // The folder move: content checked out under the new path, then committed.
    "after_folder_move_checkout",
    "after_folder_move_commit",
    // A switch: the branch pinned, then each step around moving HEAD.
    "after_branch_pinned",
    "before_branch_switch",
    "between_branch_and_switch",
    // libgit2 wrote the files of a checkout (switch, restore, discard,
    // incoming, move, repair, rollback); filtered files are still pointers.
    "files_written",
    // One filtered file (say, Git LFS) replaced by its content; hit once
    // per file.
    "file_smudged",
    // Every filtered file of the checkout smudged.
    "smudged",
    "before_settle_files",
    // A save: index checked, parent read, commit written and staged, branch
    // moved.
    "after_index_precheck",
    "after_parent_read",
    "index_staged",
    "head_advanced",
    // Incoming changes fetched and applied.
    "after_apply_fetch",
    // A publish or send: Git LFS content uploaded, then the branch pushed.
    "lfs_uploaded",
    "pushed",
    // The operation marked its record complete.
    "recovery_completed",
    // Another Git process held the index; Draftline waits and retries.
    "git_busy_retry",
];

#[must_use]
pub(crate) fn operation(name: &str) -> imp::Operation {
    imp::operation(name)
}

/// A crash point: aborts here when `DRAFTLINE_FAULT` names it.
pub(crate) fn point(point: &str) {
    imp::point(point)
}

#[cfg(test)]
mod tests {
    use super::POINTS;

    /// Every point the code reaches is listed, and every listed one exists.
    #[test]
    fn points_lists_every_crash_point() {
        let source = include_str!("workspace.rs");
        let mut found = Vec::new();
        for marker in ["fault::point(\"", "test_hooks::fire(\"", "\"recovery_"] {
            for (index, _) in source.match_indices(marker) {
                let rest = &source[index + marker.len()..];
                let name = &rest[..rest.find('"').unwrap()];
                let name = if marker == "\"recovery_" {
                    format!("recovery_{name}")
                } else {
                    name.to_string()
                };
                if marker != "\"recovery_" || POINTS.contains(&name.as_str()) {
                    found.push(name);
                }
            }
        }
        for name in &found {
            assert!(POINTS.contains(&name.as_str()), "{name} is not in POINTS");
        }
        for point in POINTS {
            assert!(
                found.iter().any(|name| name == point),
                "{point} is never reached"
            );
        }
    }
}
