//! Steam hidraw suppression while the Gamescope controller overlay is active.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;

use crate::hidraw::HidrawBlocker;

#[derive(Default)]
pub struct SteamBlocker {
    hidraw: Option<HidrawBlocker>,
}

impl SteamBlocker {
    fn ensure_hidraw_blocker(&mut self) -> bool {
        if self.hidraw.is_some() {
            return true;
        }
        match HidrawBlocker::new() {
            Ok(hidraw) => {
                self.hidraw = Some(hidraw);
                true
            }
            Err(error) => {
                tracing::debug!(
                    %error,
                    "Steam hidraw suppression is not available for this overlay"
                );
                false
            }
        }
    }

    /// Blocks Steam and its descendants from reading hidraw while the overlay
    /// owns controller input. The daemon derives PIDs itself; no socket client
    /// can nominate arbitrary processes for BPF blocking.
    pub fn block(&mut self) {
        if !self.ensure_hidraw_blocker() {
            return;
        }
        let Some(hidraw) = self.hidraw.as_mut() else {
            return;
        };
        let pids = steam_family();
        if pids.is_empty() {
            tracing::debug!("Steam process not found for hidraw suppression");
        }
        if pids.len() > 64 {
            tracing::warn!(
                count = pids.len(),
                "Steam process tree exceeds BPF blocker capacity; only the first 64 PIDs are blocked"
            );
        }
        match hidraw.set_blocked_pids(pids.iter().copied()) {
            Ok(()) => tracing::debug!(count = pids.len().min(64), "blocked Steam hidraw reads"),
            Err(error) => tracing::warn!(%error, "could not block Steam hidraw reads"),
        }
    }

    pub fn unblock(&mut self) {
        let Some(hidraw) = self.hidraw.as_mut() else {
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

fn steam_family() -> BTreeSet<u32> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return BTreeSet::new();
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

fn steam_family_from_processes(processes: &[Process]) -> BTreeSet<u32> {
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

    let mut result = BTreeSet::new();
    while let Some(pid) = queue.pop_front() {
        if !result.insert(pid) {
            continue;
        }
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
        assert_eq!(
            steam_family_from_processes(&processes),
            BTreeSet::from([10, 11, 12])
        );
    }
}
