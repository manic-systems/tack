// SPDX-License-Identifier: EUPL-1.2

use super::{
    OmitPolicy,
    ScanDocuments,
};
use crate::scan_diagnostic::ScanFile;

#[test]
fn scan_records_gitlab_locked_nodes() {
    let path = vec!["root".to_owned()];
    let result = ScanDocuments {
        flake_lock: Some(
            r#"{
                "root": "root",
                "nodes": {
                    "root": {},
                    "dep": {
                        "locked": {
                            "type": "gitlab",
                            "host": "GitLab.Example.Com:8443",
                            "owner": "Group/Sub",
                            "repo": "Repo",
                            "rev": "abc123",
                            "lastModified": 1700
                        }
                    }
                }
            }"#
            .to_owned(),
        ),
        tack_pins:  None,
        tack_lock:  None,
    }
    .scan(&path, &OmitPolicy::default());

    assert_eq!(result.findings.len(), 1);
    let finding = &result.findings[0];
    assert_eq!(
        finding.identity.to_string(),
        "gitlab:gitlab.example.com:8443/group/sub/repo"
    );
    assert_eq!(finding.entry.rev, "abc123");
    assert_eq!(finding.entry.lm, Some(1_700));
}

#[test]
fn scan_reports_tack_lock_parse_failure_and_continues() {
    let path = vec!["root".to_owned()];
    let result = ScanDocuments {
        flake_lock: None,
        tack_pins:  Some(
            r#"
            [inputs.dep]
            url = "github:Owner/Repo"
            "#
            .to_owned(),
        ),
        tack_lock:  Some("{".to_owned()),
    }
    .scan(&path, &OmitPolicy::default());

    assert_eq!(result.findings.len(), 1);
    assert_eq!(result.diagnostics.len(), 1);
    assert_eq!(result.diagnostics[0].file(), ScanFile::TackLock);
}

#[test]
fn scan_skips_omitted_flake_inputs() {
    let path = vec!["root".to_owned()];
    let omit = OmitPolicy::for_input(
        &["flake-compat".to_owned()].into_iter().collect(),
        &crate::pins::PinsDoc::parse("[inputs.root]\nurl = \"github:o/root\"\n")
            .unwrap()
            .inputs()
            .unwrap()
            .pop()
            .unwrap(),
        &Default::default(),
    );
    let result = ScanDocuments {
        flake_lock: Some(r#"{"root":"root","nodes":{"root":{},"flake-compat":{"locked":{"type":"github","owner":"o","repo":"compat","rev":"abc"}}}}"#.to_owned()),
        tack_pins: None,
        tack_lock: None,
    }
    .scan(&path, &omit);

    assert!(result.findings.is_empty());
}

#[test]
fn keep_inputs_overrides_global_omit() {
    let path = vec!["root".to_owned()];
    let input = crate::pins::PinsDoc::parse(
        "[inputs.root]\nurl = \"github:o/root\"\nkeep_inputs = [\"flake-compat\"]\n",
    )
    .unwrap()
    .inputs()
    .unwrap()
    .pop()
    .unwrap();
    let omit = OmitPolicy::for_input(
        &["flake-compat".to_owned()].into_iter().collect(),
        &input,
        &Default::default(),
    );
    let result = ScanDocuments {
        flake_lock: Some(r#"{"root":"root","nodes":{"root":{},"flake-compat":{"locked":{"type":"github","owner":"o","repo":"compat","rev":"abc"}}}}"#.to_owned()),
        tack_pins: None,
        tack_lock: None,
    }
    .scan(&path, &omit);

    assert_eq!(result.findings.len(), 1);
}
