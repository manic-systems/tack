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
};

#[derive(PartialEq, Eq, Hash)]
struct CompareKey {
    owner: String,
    repo:  String,
    base:  String,
    head:  String,
}

pub(super) struct CompareAttempt {
    status: Option<CompareStatus>,
    cause:  Option<String>,
}

pub(super) fn compare(owner: &str, repo: &str, base: &str, head: &str) -> CompareAttempt {
    let (maybe_status, cause) = tolerate(fetch::github::compare_status(owner, repo, base, head));
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

        let status = self
            .surfaced
            .record(compare(&key.owner, &key.repo, &key.base, &key.head));
        self.cache.insert(key, status);
        status
    }

    pub(super) fn into_surfaced(self) -> BTreeSet<String> {
        self.surfaced.into_inner()
    }
}

fn locked_compare_key(base: &LockedNode, head: &LockedNode) -> Option<CompareKey> {
    let (
        &LockedNode::Github {
            owner: ref base_owner,
            repo: ref base_repo,
            rev: ref base_rev,
            ..
        },
        &LockedNode::Github {
            owner: ref head_owner,
            repo: ref head_repo,
            rev: ref head_rev,
            ..
        },
    ) = (base, head)
    else {
        return None;
    };
    if !base_owner.eq_ignore_ascii_case(head_owner) || !base_repo.eq_ignore_ascii_case(head_repo) {
        return None;
    }
    Some(CompareKey {
        owner: base_owner.to_owned(),
        repo:  base_repo.to_owned(),
        base:  base_rev.as_ref()?.to_owned(),
        head:  head_rev.as_ref()?.to_owned(),
    })
}
