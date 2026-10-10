//! Steam hidraw suppression while the Gamescope controller overlay is active.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;

use crate::hidraw::HidrawBlocker;

#[derive(Default)]
pub struct SteamBlocker {
    /// Set on first use. A setup failure is kept, so it is warned about once
    /// and the BPF object is not reloaded on every retry.
    hidraw: Option<Result<HidrawBlocker, String>>,
}

impl SteamBlocker {
    /// Blocks Steam and its descendants from reading hidraw while the overlay
    /// owns controller input. The daemon derives PIDs itself; no socket client
    /// can nominate arbitrary processes for BPF blocking.
    pub fn block(&mut self) {
        let hidraw = self.hidraw.get_or_insert_with(|| {
            HidrawBlocker::new().inspect_err(|error| {
                tracing::warn!(%error, "Steam hidraw suppression is unavailable");
            })
        });
        let Ok(hidraw) = hidraw else {
            return;
        };
        let pids = steam_family();
        if pids.is_empty() {
            tracing::debug!("Steam process not found for hidraw suppression");
        }
        if pids.len() > 64 {
            tracing::warn!(
                count = pids.len(),
                "Steam process tree exceeds BPF blocker capacity; only the 64 PIDs nearest the root are blocked"
            );
        }
        match hidraw.set_blocked_pids(pids.iter().copied()) {
            Ok(()) => tracing::debug!(count = pids.len().min(64), "blocked Steam hidraw reads"),
            Err(error) => tracing::warn!(%error, "could not block Steam hidraw reads"),
        }
    }

    /// Whether PIDs remain in the BPF map, including ones a failed unblock left.
    pub fn has_blocked_pids(&self) -> bool {
        matches!(&self.hidraw, Some(Ok(hidraw)) if hidraw.has_blocked_pids())
    }

    pub fn unblock(&mut self) {
        let Some(Ok(hidraw)) = self.hidraw.as_mut() else {
            return;
        };
        if let Err(error) = hidraw.set_blocked_pids([]) {
            tracing::warn!(%error, "could not unblock Steam hidraw reads");
        }
    }
}

impl Drop for SteamBlocker {
    fn drop(&mut self) {
        self.unblock();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Process {
    pid: u32,
    parent: u32,
    comm: String,
}

fn steam_family() -> Vec<u32> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    let processes: Vec<_> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_string_lossy().parse::<u32>().ok())
        .filter_map(read_process)
        .collect();
    steam_family_from_processes(&processes)
}

fn read_process(pid: u32) -> Option<Process> {
    let base = format!("/proc/{pid}");
    let comm = fs::read_to_string(format!("{base}/comm"))
        .ok()?
        .trim()
        .to_owned();
    let status = fs::read_to_string(format!("{base}/status")).ok()?;
    let parent = status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:\t"))?
        .trim()
        .parse()
        .ok()?;
    Some(Process { pid, parent, comm })
}

/// Steam roots, then their descendants breadth-first, so truncating to the BPF
/// map capacity keeps ancestors regardless of PID order.
fn steam_family_from_processes(processes: &[Process]) -> Vec<u32> {
    let mut children = BTreeMap::<u32, Vec<u32>>::new();
    let mut queue = VecDeque::new();
    for process in processes {
        children
            .entry(process.parent)
            .or_default()
            .push(process.pid);
        if process.comm == "steam" {
            queue.push_back(process.pid);
        }
    }

    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    while let Some(pid) = queue.pop_front() {
        if !seen.insert(pid) {
            continue;
        }
        result.push(pid);
        if let Some(next) = children.get(&pid) {
            queue.extend(next);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn includes_all_steam_descendants_without_unrelated_processes() {
        let processes = [
            Process {
                pid: 10,
                parent: 1,
                comm: "steam".into(),
            },
            Process {
                pid: 11,
                parent: 10,
                comm: "steamwebhelper".into(),
            },
            Process {
                pid: 12,
                parent: 11,
                comm: "game".into(),
            },
            Process {
                pid: 13,
                parent: 1,
                comm: "other".into(),
            },
        ];
        assert_eq!(steam_family_from_processes(&processes), [10, 11, 12]);
    }

    #[test]
    fn bpf_capacity_truncation_keeps_the_steam_root_after_pid_wrap() {
        // After PID wrap the long-lived root outnumbers its children, so an
        // ascending-PID cut at the 64-entry BPF map would drop it.
        let processes: Vec<_> = std::iter::once(Process {
            pid: 50_000,
            parent: 1,
            comm: "steam".into(),
        })
        .chain((2..=80).map(|pid| Process {
            pid,
            parent: 50_000,
            comm: "steamwebhelper".into(),
        }))
        .collect();
        let family = steam_family_from_processes(&processes);
        assert!(family.into_iter().take(64).any(|pid| pid == 50_000));
    }
}
