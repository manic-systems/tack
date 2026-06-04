// SPDX-License-Identifier: EUPL-1.2

use std::path::PathBuf;

use gix::{
    objs::{
        Tree,
        tree::{
            Entry,
            EntryKind,
        },
    },
    refs::{
        Target,
        transaction::{
            Change,
            LogChange,
            PreviousValue,
            RefEdit,
        },
    },
};

pub(super) struct LocalRemote {
    _tmp:   tempfile::TempDir,
    repo:   gix::Repository,
    remote: PathBuf,
    branch: String,
    tip:    Option<gix::ObjectId>,
    time:   i64,
}

impl LocalRemote {
    pub(super) fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("remote.git");
        let repo = gix::init_bare(&remote).unwrap();
        let this = Self {
            _tmp: tmp,
            repo,
            remote,
            branch: "refs/heads/main".to_owned(),
            tip: None,
            time: 100,
        };
        this.set_head("refs/heads/main");
        this
    }

    pub(super) fn commit(&mut self, body: &str, message: &str) -> String {
        let tree = self.tree(body);
        let signature_text = format!("tack <tack@example.invalid> {} +0000", self.time);
        self.time += 1;
        let signature = gix::actor::SignatureRef::from_bytes(signature_text.as_bytes()).unwrap();
        let parents = self.tip.iter().copied();
        let commit = self
            .repo
            .new_commit_as(signature, signature, message, tree, parents)
            .unwrap()
            .id()
            .detach();
        self.tip = Some(commit);
        self.set_ref(&self.branch, commit);
        commit.to_string()
    }

    pub(super) fn branch_from_current(&mut self, branch: &str) {
        self.branch = branch.to_owned();
        if let Some(tip) = self.tip {
            self.set_ref(branch, tip);
        }
    }

    pub(super) fn reset_to(&mut self, rev: &str) {
        let id = parse_id(rev);
        self.tip = Some(id);
        self.set_ref(&self.branch, id);
    }

    pub(super) fn tag(&self, name: &str, rev: &str) {
        self.repo
            .tag_reference(name, parse_id(rev), PreviousValue::Any)
            .unwrap();
    }

    pub(super) fn url(&self) -> String {
        format!("file://{}", self.remote.display())
    }

    fn tree(&self, body: &str) -> gix::ObjectId {
        let blob = self.repo.write_blob(body.as_bytes()).unwrap().detach();
        let tree = Tree {
            entries: vec![Entry {
                mode:     EntryKind::Blob.into(),
                filename: "file.txt".into(),
                oid:      blob,
            }],
        };
        self.repo.write_object(tree).unwrap().detach()
    }

    fn set_ref(&self, name: &str, target: gix::ObjectId) {
        self.repo
            .reference(name, target, PreviousValue::Any, "test")
            .unwrap();
    }

    fn set_head(&self, branch: &str) {
        self.repo
            .edit_reference(RefEdit {
                change: Change::Update {
                    log:      LogChange::default(),
                    expected: PreviousValue::Any,
                    new:      Target::Symbolic(branch.try_into().unwrap()),
                },
                name:   "HEAD".try_into().unwrap(),
                deref:  false,
            })
            .unwrap();
    }
}

fn parse_id(rev: &str) -> gix::ObjectId {
    gix::ObjectId::from_hex(rev.as_bytes()).unwrap()
}
