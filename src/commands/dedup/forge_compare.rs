// SPDX-License-Identifier: EUPL-1.2

use std::collections::{
    BTreeSet,
    HashMap,
};

use crate::{
    commands::tolerate,
    fetch::{
        self,
        github::CompareStatus,
    },
    lock::LockedNode,
    source::{
        gitlab,
        id::SourceId,
    },
};

#[derive(PartialEq, Eq, Hash)]
struct CompareKey {
    id:   SourceId,
    base: String,
    head: String,
}

pub(super) struct CompareAttempt {
    status: Option<CompareStatus>,
    cause:  Option<String>,
}

/// whether `compare` has a branch-topology backend to ask for this identity
pub(super) const fn comparable(id: &SourceId) -> bool {
    matches!(
        *id,
        SourceId::Github { .. } | SourceId::Gitlab { .. } | SourceId::Git { .. }
    )
}

pub(super) fn compare(id: &SourceId, base: &str, head: &str) -> CompareAttempt {
    let (maybe_status, cause) = match *id {
        SourceId::Github {
            ref owner,
            ref repo,
        } => tolerate(fetch::github::compare_status(owner, repo, base, head)),
        SourceId::Gitlab {
            ref host,
            ref owner,
            ref repo,
        } => {
            let url = gitlab::clone_url(host, owner, repo);
            tolerate(fetch::git_compare_status(&url, base, head))
        },
        SourceId::Git { ref url } => tolerate(fetch::git_compare_status(url, base, head)),
        SourceId::Tarball { .. } | SourceId::Indirect { .. } | SourceId::Path { .. } => {
            (None, None)
        },
    };
    CompareAttempt {
        status: maybe_status.flatten(),
        cause,
    }
}

#[derive(Default)]
pub(super) struct SurfacedCauses {
    causes: BTreeSet<String>,
}

impl SurfacedCauses {
    pub(super) fn record(&mut self, attempt: CompareAttempt) -> Option<CompareStatus> {
        if let Some(cause) = attempt.cause {
            self.causes.insert(cause);
        }
        attempt.status
    }

    pub(super) fn print_tack_messages(&self) {
        for cause in &self.causes {
            eprintln!("tack: {cause}");
        }
    }

    pub(super) fn into_inner(self) -> BTreeSet<String> {
        self.causes
    }
}

#[derive(Default)]
pub(super) struct CachedComparator {
    cache:    HashMap<CompareKey, Option<CompareStatus>>,
    surfaced: SurfacedCauses,
}

impl CachedComparator {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn compare_locked_nodes(
        &mut self,
        base: &LockedNode,
        head: &LockedNode,
    ) -> Option<CompareStatus> {
        self.compare_key(locked_compare_key(base, head)?)
    }

    fn compare_key(&mut self, key: CompareKey) -> Option<CompareStatus> {
        if key.base == key.head {
            return Some(CompareStatus::Identical);
        }
        if let Some(cached) = self.cache.get(&key) {
            return *cached;
        }

        let status = self.surfaced.record(compare(&key.id, &key.base, &key.head));
        self.cache.insert(key, status);
        status
    }

    pub(super) fn into_surfaced(self) -> BTreeSet<String> {
        self.surfaced.into_inner()
    }
}

fn locked_compare_key(base: &LockedNode, head: &LockedNode) -> Option<CompareKey> {
    let id = SourceId::from_locked(base).filter(comparable)?;
    if SourceId::from_locked(head).as_ref() != Some(&id) {
        return None;
    }
    Some(CompareKey {
        id,
        base: base.rev()?.to_owned(),
        head: head.rev()?.to_owned(),
    })
}
