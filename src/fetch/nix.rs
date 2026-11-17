// SPDX-License-Identifier: EUPL-1.2

use std::{
    env,
    io::Read,
    mem,
    process::{
        Command,
        Stdio,
    },
    sync::{
        Arc,
        Mutex,
        OnceLock,
        PoisonError,
    },
    thread::{
        self,
        JoinHandle,
    },
    time::{
        Duration,
        Instant,
    },
};

use misstep::{
    Result,
    ResultExt as _,
    bail,
};
use serde::{
    Deserialize,
    Serialize,
};

use super::auth::{
    env_flag_enabled,
    nix_access_tokens,
};

/// the `builtins.fetchTree` attrs of a locked rev
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(super) enum TreeAttrs<'a> {
    Github {
        owner: &'a str,
        repo:  &'a str,
        rev:   &'a str,
    },
    Gitlab {
        host:  &'a str,
        owner: &'a str,
        repo:  &'a str,
        rev:   &'a str,
    },
    Git {
        url:        &'a str,
        #[serde(rename = "ref")]
        reff:       &'a str,
        rev:        &'a str,
        submodules: bool,
        shallow:    bool,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NixTree {
    pub nar_hash:      String,
    pub last_modified: i64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavor {
    Nix,
    Lix,
}

fn flavor() -> Option<Flavor> {
    static FLAVOR: OnceLock<Option<Flavor>> = OnceLock::new();
    *FLAVOR.get_or_init(|| {
        let output = Command::new("nix").arg("--version").output().ok()?;
        let printed = String::from_utf8_lossy(&output.stdout);
        output.status.success().then(|| {
            if printed.contains("Lix") {
                Flavor::Lix
            } else {
                Flavor::Nix
            }
        })
    })
}

/// nix's fetchTree and fetchGit look only in its fetcher cache and download a
/// tree tack added to the store again, while lix's check the store first
pub(super) fn fetches_past_store() -> bool {
    flavor() == Some(Flavor::Nix)
}

/// `TACK_NO_STORE` skips adding trees for machines that never build, like CI
/// bots that only update the lock
pub(super) fn seeds_store() -> bool {
    flavor().is_some() && !env_flag_enabled(env::var("TACK_NO_STORE").ok().as_deref())
}

pub(super) fn fetch_tree(attrs: &TreeAttrs<'_>) -> Result<NixTree> {
    const EXPR: &str = r#"let tree = builtins.fetchTree (builtins.fromJSON (builtins.getEnv "TACK_FETCH_TREE")); in { inherit (tree) narHash lastModified; }"#;
    // a stuck cache lock or ssh prompt would otherwise hold the whole update
    const TIMEOUT: Duration = Duration::from_mins(10);

    let mut config = env::var("NIX_CONFIG").unwrap_or_default();
    for token in nix_access_tokens() {
        config.push_str("\nextra-access-tokens = ");
        config.push_str(&token);
    }
    let mut child = Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command",
            "eval",
            "--impure",
            "--json",
        ])
        .args(["--expr", EXPR])
        .env("TACK_FETCH_TREE", serde_json::to_string(attrs)?)
        .env("NIX_CONFIG", config)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("run nix eval")?;

    let stdout_reader = drain(child.stdout.take());
    let stderr_reader = drain(child.stderr.take());
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > TIMEOUT {
            drop(child.kill());
            drop(child.wait());
            bail!(
                "nix eval took longer than {} minutes",
                TIMEOUT.as_secs() / 60
            );
        }
        thread::sleep(Duration::from_millis(50));
    };
    let stdout = stdout_reader.collect();
    if !status.success() {
        let stderr = stderr_reader.collect();
        bail!(
            "nix eval failed: {}",
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    Ok(serde_json::from_slice(&stdout).context("parse nix eval output")?)
}

/// a pipe read on its own thread, since a grandchild like an ssh
/// `ControlPersist` master can hold it open after nix exits
struct Drained {
    text:   Arc<Mutex<Vec<u8>>>,
    reader: JoinHandle<()>,
}

impl Drained {
    fn collect(self) -> Vec<u8> {
        const GRACE: Duration = Duration::from_secs(2);

        let started = Instant::now();
        while !self.reader.is_finished() && started.elapsed() < GRACE {
            thread::sleep(Duration::from_millis(10));
        }
        mem::take(&mut *self.text.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

fn drain<Pipe>(opened: Option<Pipe>) -> Drained
where
    Pipe: Read + Send + 'static,
{
    let text = Arc::<Mutex<Vec<u8>>>::default();
    let sink = Arc::clone(&text);
    let reader = thread::spawn(move || {
        let Some(mut pipe) = opened else {
            return;
        };
        let mut chunk = [0_u8; 8192];
        while let Ok(read) = pipe.read(&mut chunk)
            && read > 0
        {
            sink.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(chunk.get(..read).unwrap_or_default());
        }
    });
    Drained { text, reader }
}

#[cfg(test)]
#[path = "nix_tests.rs"]
mod tests;
