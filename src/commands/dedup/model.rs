// SPDX-License-Identifier: EUPL-1.2

use crate::{
    lock::LockIdentity,
    pins::Side,
};

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum IdentityKind {
    Rev,
    ContentHash,
    ImmutableUrl,
    SourceUrl,
    PathFingerprint,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Identity {
    pub kind:  IdentityKind,
    pub value: String,
}

impl Identity {
    pub(super) fn from_lock(identity: LockIdentity<'_>) -> Self {
        let (kind, value) = match identity {
            LockIdentity::Rev(value) => (IdentityKind::Rev, value.to_owned()),
            LockIdentity::ContentHash(value) => (IdentityKind::ContentHash, value.to_owned()),
            LockIdentity::ImmutableUrl(value) => (IdentityKind::ImmutableUrl, value.to_owned()),
            LockIdentity::SourceUrl(value) => (IdentityKind::SourceUrl, value.to_owned()),
            LockIdentity::PathFingerprint(value) => (IdentityKind::PathFingerprint, value),
        };
        Self { kind, value }
    }

    pub(super) fn rev(&self) -> Option<&str> {
        matches!(self.kind, IdentityKind::Rev).then_some(self.value.as_str())
    }
}

#[derive(Clone)]
pub(super) struct Entry {
    pub path:     Vec<String>,
    pub name:     String,
    pub side:     Side,
    pub identity: Option<Identity>,
    pub lm:       Option<u64>,
}
