// SPDX-License-Identifier: EUPL-1.2

use std::{
    borrow::Borrow,
    collections::BTreeMap,
    fmt::{
        Display,
        Formatter,
        Result as FmtResult,
    },
    fs::{
        self,
        File,
        Permissions,
    },
    io::{
        ErrorKind,
        Write as _,
    },
    os::unix::fs::PermissionsExt as _,
    path::{
        Component,
        Path,
        PathBuf,
    },
    process::{
        Command,
        Output,
    },
    str::FromStr,
};

use data_encoding::{
    BASE64,
    HEXLOWER,
};
use gix::{
    bstr::BString,
    hash::Kind as HashKind,
    objs::CommitRefIter,
};
use hmac_sha256::Hash as Sha256;
use misstep::{
    Result,
    ResultExt as _,
};
use tempfile::{
    NamedTempFile,
    TempDir,
};

use crate::{
    error::user_bail,
    fetch::{
        self,
        CommitObject,
        CommitRange,
    },
    lock::LockedNode,
    render::printable,
};

const SSH_SIGNATURE: &str = "-----BEGIN SSH SIGNATURE-----";
const PGP_SIGNATURE: &str = "-----BEGIN PGP SIGNATURE-----";
const PGP_PUBLIC_KEY: &str = "-----BEGIN PGP PUBLIC KEY BLOCK-----";
const SSH_KEY_PREFIXES: [&str; 4] = ["ssh-", "ecdsa-sha2-", "sk-ssh-", "sk-ecdsa-"];

#[derive(Debug)]
pub enum SignerKey {
    Ssh(Vec<String>),
    Gpg(String),
}

impl FromStr for SignerKey {
    type Err = misstep::Report;

    fn from_str(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        if trimmed.starts_with(PGP_PUBLIC_KEY) {
            // GitHub's .gpg exports carry a `Note:` armor header that gpg refuses, and
            // armor headers are optional, so drop them all
            let mut in_headers = false;
            let armored = trimmed
                .lines()
                .filter(|line| {
                    if line.starts_with("-----BEGIN PGP") {
                        in_headers = true;
                    } else if in_headers {
                        in_headers = line.contains(':');
                        return !in_headers;
                    }
                    true
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(Self::Gpg(armored));
        }
        let lines = trimmed
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect::<Vec<_>>();
        let all_ssh = lines.iter().all(|line| {
            let mut fields = line.split_whitespace();
            let kind = fields.next().unwrap_or_default();
            SSH_KEY_PREFIXES
                .iter()
                .any(|prefix| kind.starts_with(prefix))
                && fields
                    .next()
                    .is_some_and(|blob| BASE64.decode(blob.as_bytes()).is_ok())
        });
        if lines.is_empty() || !all_ssh {
            user_bail!(
                "not an SSH public key (a type and base64 key) or an armored PGP public key"
            );
        }
        Ok(Self::Ssh(lines.into_iter().map(str::to_owned).collect()))
    }
}

impl SignerKey {
    /// a value is a key itself or a path under `dir` to a file holding one
    pub fn load(name: &SignerName, value: &str, dir: &Path) -> Result<Self> {
        if let Ok(key) = value.parse::<Self>() {
            return Ok(key);
        }
        let resolved = confined(dir, key_file(value)?)?;
        let Some(contents) = resolved.and_then(|path| fs::read_to_string(path).ok()) else {
            user_bail!(
                "signer '{name}' is neither a public key nor a readable file at {}",
                printable(value)
            );
        };
        contents
            .parse::<Self>()
            .with_context(|| format!("signer '{name}': {}", printable(value)))
    }

    pub const fn extension(&self) -> &'static str {
        match *self {
            Self::Ssh(_) => "keys",
            Self::Gpg(_) => "asc",
        }
    }

    /// one printable line per key, for checking against what the signer
    /// reports from `ssh-add -L` or `gpg -K`
    pub fn fingerprints(&self) -> Result<Vec<String>> {
        match *self {
            Self::Ssh(ref lines) => lines.iter().map(|line| ssh_fingerprint(line)).collect(),
            Self::Gpg(ref armored) => gpg_fingerprints(armored),
        }
    }
}

fn ssh_fingerprint(line: &str) -> Result<String> {
    let mut file = NamedTempFile::new()?;
    writeln!(file, "{line}")?;
    let listed = run(
        "ssh-keygen",
        Command::new("ssh-keygen").arg("-lf").arg(file.path()),
    )?;
    let stdout = String::from_utf8_lossy(&listed.stdout);
    let Some(fingerprint) = stdout
        .split_whitespace()
        .nth(1)
        .filter(|_| listed.status.success())
    else {
        user_bail!("ssh-keygen could not read the key {}", printable(line));
    };
    let mut fields = line.split_whitespace();
    let kind = fields.next().unwrap_or_default();
    let comment = fields.skip(1).collect::<Vec<_>>().join(" ");
    Ok(printable(
        format!("{kind} {fingerprint} {comment}").trim_end(),
    ))
}

fn gpg_fingerprints(armored: &str) -> Result<Vec<String>> {
    let home = tempfile::tempdir()?;
    fs::set_permissions(home.path(), Permissions::from_mode(0o700))?;
    let key_file = home.path().join("key.asc");
    fs::write(&key_file, armored)?;
    let shown = run(
        "gpg",
        Command::new("gpg")
            .env("GNUPGHOME", home.path())
            .args(["--batch", "--no-autostart", "--with-colons", "--show-keys"])
            .arg(&key_file),
    )?;
    if !shown.status.success() {
        user_bail!(
            "gpg could not read the key: {}",
            printable(String::from_utf8_lossy(&shown.stderr).trim())
        );
    }

    let mut keys = Vec::<(String, Option<String>)>::new();
    let mut primary = false;
    for line in String::from_utf8_lossy(&shown.stdout).lines() {
        let fields = line.split(':').collect::<Vec<_>>();
        let value = fields.get(9).copied().unwrap_or_default();
        match fields.first().copied().unwrap_or_default() {
            "pub" => {
                primary = true;
                keys.push((String::new(), None));
            },
            "sub" | "sec" | "ssb" => primary = false,
            "fpr" if primary => {
                if let Some(&mut (ref mut fingerprint, _)) = keys.last_mut()
                    && fingerprint.is_empty()
                {
                    value.clone_into(fingerprint);
                }
            },
            "uid" if primary => {
                if let Some(&mut (_, ref mut uid)) = keys.last_mut() {
                    uid.get_or_insert_with(|| value.replace("\\x3a", ":"));
                }
            },
            _ => {},
        }
    }
    Ok(keys
        .into_iter()
        .map(|(fingerprint, uid)| {
            printable(format!("pgp {fingerprint} {}", uid.unwrap_or_default()).trim_end())
        })
        .collect())
}

/// only what's safe as an `allowed_signers` principal and a file name under
/// `.tack/keys`
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SignerName(String);

impl SignerName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for SignerName {
    type Err = misstep::Report;

    fn from_str(raw: &str) -> Result<Self> {
        let valid = !raw.is_empty()
            && raw
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte));
        if !valid {
            user_bail!(
                "signer '{}' may only use letters, digits, '-', '_' and '.'",
                printable(raw)
            );
        }
        Ok(Self(raw.to_owned()))
    }
}

/// a value naming a key file, relative to `.tack` and never climbing out
pub fn key_file(value: &str) -> Result<&Path> {
    let path = Path::new(value);
    let relative = path.components().next().is_some()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    if !relative {
        user_bail!(
            "key file '{}' must be a relative path inside .tack",
            printable(value)
        );
    }
    Ok(path)
}

/// `relative` resolved under `dir`, or `None` when nothing is there, refusing
/// symlinks that lead outside it
pub fn confined(dir: &Path, relative: &Path) -> Result<Option<PathBuf>> {
    let root = dir.canonicalize()?;
    let resolved = match root.join(relative).canonicalize() {
        Ok(resolved) => resolved,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("resolve {}", printable(&relative.to_string_lossy())));
        },
    };
    if !resolved.starts_with(&root) {
        user_bail!(
            "{} leads outside .tack",
            printable(&relative.to_string_lossy())
        );
    }
    Ok(Some(resolved))
}

impl Display for SignerName {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(&self.0)
    }
}

impl Borrow<str> for SignerName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Default)]
pub struct Keyring {
    signers: BTreeMap<SignerName, Vec<SignerKey>>,
}

impl Keyring {
    pub fn load(declared: Vec<(SignerName, Vec<&str>)>, dir: &Path) -> Result<Self> {
        let mut signers = BTreeMap::new();
        for (name, values) in declared {
            let keys = values
                .into_iter()
                .map(|value| SignerKey::load(&name, value, dir))
                .collect::<Result<Vec<_>>>()?;
            signers.insert(name, keys);
        }
        Ok(Self { signers })
    }

    /// a signer whose values cannot all be read and parsed is left out
    pub fn load_lenient<Read: Fn(&str) -> Option<String>>(
        declared: Vec<(SignerName, Vec<&str>)>,
        read: Read,
    ) -> Self {
        let signers = declared
            .into_iter()
            .filter_map(|(name, values)| {
                let keys = values
                    .into_iter()
                    .map(|value| {
                        value
                            .parse::<SignerKey>()
                            .ok()
                            .or_else(|| read(value)?.parse().ok())
                    })
                    .collect::<Option<Vec<_>>>()?;
                Some((name, keys))
            })
            .collect();
        Self { signers }
    }

    /// stands for one signer's whole key material, so swapping a key file or an
    /// inline key under the same name shows up as a change
    pub fn keys_digest(&self, name: &SignerName) -> Option<String> {
        let mut parts = self
            .signers
            .get(name)?
            .iter()
            .flat_map(|key| {
                match *key {
                    SignerKey::Ssh(ref lines) => {
                        lines.iter().map(|line| format!("ssh {line}")).collect()
                    },
                    SignerKey::Gpg(ref armored) => vec![format!("pgp {armored}")],
                }
            })
            .collect::<Vec<_>>();
        parts.sort();
        parts.dedup();
        let mut hasher = Sha256::new();
        for part in &parts {
            hasher.update(format!("{}:", part.len()));
            hasher.update(part);
        }
        Some(format!("sha256-{}", HEXLOWER.encode(&hasher.finalize())))
    }

    /// the one of `names` who signed the commit `node` locks, after checking
    /// every commit since `anchor`, or a rollback to a commit already checked
    pub fn verify(
        &self,
        names: &[SignerName],
        anchor: Option<Anchor<'_>>,
        node: &LockedNode,
    ) -> Result<Verdict<'_>> {
        let allowed = names
            .iter()
            .filter_map(|name| self.signers.get_key_value(name))
            .map(|(name, keys)| (name, keys.as_slice()))
            .collect::<Vec<_>>();
        let verifier = Verifier::new(&allowed)?;
        let Some(Anchor { rev: base, since }) = anchor else {
            let Some(tip) = fetch::commit_object(node)? else {
                user_bail!("signers need a github, gitlab, or git pin");
            };
            return verifier.signer(&tip).map(|signer| {
                Verdict {
                    signer,
                    rolled_back: false,
                }
            });
        };
        let from = short(base);
        let to = node.forge_rev().map(short).unwrap_or_default();
        let Some(range) = fetch::commit_range(base, node)
            .with_context(|| format!("checking every commit from {from} to {to}"))?
        else {
            user_bail!("signers need a github, gitlab, or git pin");
        };
        let commits = match range {
            CommitRange::Commits(commits) => commits,
            CommitRange::Diverged => {
                user_bail!(
                    "{to} does not descend from the verified {from}, upstream history was \
                     rewritten"
                )
            },
            CommitRange::Ancestor => {
                let Some(first) = since else {
                    user_bail!(
                        "{to} is older than the verified {from}, and nothing records how far back \
                         its history was checked"
                    );
                };
                return match fetch::commit_range(first, node)? {
                    Some(CommitRange::Commits(_)) => {
                        let Some(tip) = fetch::commit_object(node)? else {
                            user_bail!("signers need a github, gitlab, or git pin");
                        };
                        verifier.signer(&tip).map(|signer| {
                            Verdict {
                                signer,
                                rolled_back: true,
                            }
                        })
                    },
                    Some(CommitRange::Ancestor | CommitRange::Diverged) => {
                        user_bail!(
                            "{to} is older than the first verified commit {}, so its history was \
                             never checked",
                            short(first)
                        )
                    },
                    Some(CommitRange::TooLarge) => {
                        user_bail!(
                            "too many commits between {} and {to} to compare",
                            short(first)
                        )
                    },
                    None => user_bail!("signers need a github, gitlab, or git pin"),
                };
            },
            CommitRange::TooLarge => {
                user_bail!(
                    "too many commits between the verified {from} and {to} to check them all"
                )
            },
        };
        let mut tip_signer = None;
        for commit in &commits {
            let signer = verifier
                .signer(commit)
                .with_context(|| format!("checking every commit from {from} to {to}"))?;
            if node.forge_rev() == Some(commit.id.as_str()) {
                tip_signer = Some(signer);
            }
        }
        let Some(signer) = tip_signer else {
            user_bail!("{to} was not among the commits after {from}");
        };
        Ok(Verdict {
            signer,
            rolled_back: false,
        })
    }
}

/// the verified rev a range check starts from, and the oldest rev its chain of
/// checks reaches back to
#[derive(Clone, Copy)]
pub struct Anchor<'rev> {
    pub rev:   &'rev str,
    pub since: Option<&'rev str>,
}

pub struct Verdict<'ring> {
    pub signer:      &'ring SignerName,
    /// the new rev sits between the first verified commit and the anchor
    pub rolled_back: bool,
}

fn short(rev: &str) -> &str {
    rev.get(..7).unwrap_or(rev)
}

/// key material laid out once per update, so checking a long range doesn't
/// rebuild a keyring per commit
struct Verifier<'name> {
    scratch:         TempDir,
    names:           Vec<&'name SignerName>,
    allowed_signers: Option<PathBuf>,
    keyrings:        Vec<(&'name SignerName, TempDir)>,
}

impl<'name> Verifier<'name> {
    fn new(allowed: &[(&'name SignerName, &[SignerKey])]) -> Result<Self> {
        let scratch = tempfile::tempdir()?;
        let entries = allowed
            .iter()
            .flat_map(|&(name, keys)| {
                keys.iter()
                    .filter_map(|key| {
                        match *key {
                            SignerKey::Ssh(ref lines) => Some(lines),
                            SignerKey::Gpg(_) => None,
                        }
                    })
                    .flatten()
                    .map(move |line| format!("{name} namespaces=\"git\" {line}\n"))
            })
            .collect::<String>();
        let allowed_signers = if entries.is_empty() {
            None
        } else {
            let path = scratch.path().join("allowed_signers");
            fs::write(&path, entries)?;
            Some(path)
        };

        let mut keyrings = Vec::new();
        for &(name, keys) in allowed {
            let armored = keys
                .iter()
                .filter_map(|key| {
                    match *key {
                        SignerKey::Gpg(ref armored) => Some(armored.as_str()),
                        SignerKey::Ssh(_) => None,
                    }
                })
                .collect::<Vec<_>>();
            if armored.is_empty() {
                continue;
            }
            let home = tempfile::tempdir()?;
            fs::set_permissions(home.path(), Permissions::from_mode(0o700))?;
            let key_file = home.path().join("key.asc");
            fs::write(&key_file, armored.join("\n"))?;
            let imported = run(
                "gpg",
                Command::new("gpg")
                    .env("GNUPGHOME", home.path())
                    .args(["--batch", "--quiet", "--no-autostart", "--import"])
                    .arg(&key_file),
            )?;
            if !imported.status.success() {
                user_bail!(
                    "signer '{name}': gpg could not import its key: {}",
                    printable(String::from_utf8_lossy(&imported.stderr).trim())
                );
            }
            keyrings.push((name, home));
        }

        Ok(Self {
            scratch,
            names: allowed.iter().map(|&(name, _)| name).collect(),
            allowed_signers,
            keyrings,
        })
    }

    fn signer(&self, commit: &CommitObject) -> Result<&'name SignerName> {
        let id = short(&commit.id);
        let hash_kind = HashKind::from_hex_len(commit.id.len()).unwrap_or_default();
        let Some((signature, signed_data)) = CommitRefIter::signature(&commit.data, hash_kind)
            .with_context(|| format!("parse commit {id}"))?
        else {
            user_bail!("commit {id} is not signed");
        };

        let files = SignedFiles {
            signature: self.scratch.path().join("signature"),
            payload:   self.scratch.path().join("payload"),
        };
        fs::write(&files.signature, signature.as_ref())?;
        fs::write(&files.payload, BString::from(signed_data))?;
        let matched = if signature.starts_with(SSH_SIGNATURE.as_bytes()) {
            self.allowed_signers
                .as_ref()
                .map(|allowed| files.ssh_signer(allowed, &self.names))
                .transpose()
                .with_context(|| format!("commit {id}"))?
                .flatten()
        } else if signature.starts_with(PGP_SIGNATURE.as_bytes()) {
            files.gpg_signer(&self.keyrings)?
        } else {
            user_bail!("commit {id} carries a kind of signature tack can't check");
        };
        let Some(name) = matched else {
            user_bail!(
                "commit {id} is not signed by {}",
                self.names
                    .iter()
                    .map(|name| name.as_str())
                    .collect::<Vec<_>>()
                    .join(" or ")
            );
        };
        Ok(name)
    }
}

struct SignedFiles {
    signature: PathBuf,
    payload:   PathBuf,
}

impl SignedFiles {
    fn ssh_signer<'name>(
        &self,
        allowed_signers: &Path,
        names: &[&'name SignerName],
    ) -> Result<Option<&'name SignerName>> {
        let found = run(
            "ssh-keygen",
            Command::new("ssh-keygen")
                .args(["-Y", "find-principals", "-s"])
                .arg(&self.signature)
                .arg("-f")
                .arg(allowed_signers),
        )?;
        if !found.status.success() {
            if let Some(detail) = ssh_complaint(&found) {
                user_bail!("ssh-keygen: {detail}");
            }
            return Ok(None);
        }
        let principal = String::from_utf8_lossy(&found.stdout);
        let Some(&name) = names
            .iter()
            .find(|&&name| principal.lines().any(|line| line.trim() == name.as_str()))
        else {
            return Ok(None);
        };
        let verified = run(
            "ssh-keygen",
            Command::new("ssh-keygen")
                .args(["-Y", "verify", "-n", "git", "-I", name.as_str(), "-f"])
                .arg(allowed_signers)
                .arg("-s")
                .arg(&self.signature)
                .stdin(File::open(&self.payload)?),
        )?;
        if !verified.status.success() {
            user_bail!(
                "ssh-keygen rejected {name}'s signature: {}",
                ssh_complaint(&verified).unwrap_or_else(|| "no reason given".to_owned())
            );
        }
        Ok(Some(name))
    }

    /// a keyring per signer, so a good signature names the signer it came from
    fn gpg_signer<'name>(
        &self,
        keyrings: &[(&'name SignerName, TempDir)],
    ) -> Result<Option<&'name SignerName>> {
        for &(name, ref home) in keyrings {
            let verified = run(
                "gpg",
                Command::new("gpg")
                    .env("GNUPGHOME", home.path())
                    .args(["--batch", "--no-autostart", "--status-fd", "1", "--verify"])
                    .arg(&self.signature)
                    .arg(&self.payload),
            )?;
            let status = String::from_utf8_lossy(&verified.stdout);
            let keywords = status
                .lines()
                .filter_map(|line| line.strip_prefix("[GNUPG:] "))
                .filter_map(|line| line.split_whitespace().next())
                .collect::<Vec<_>>();
            let rejected = keywords.iter().any(|keyword| {
                matches!(
                    *keyword,
                    "BADSIG" | "ERRSIG" | "EXPSIG" | "EXPKEYSIG" | "REVKEYSIG"
                )
            });
            if !rejected && keywords.contains(&"GOODSIG") && keywords.contains(&"VALIDSIG") {
                return Ok(Some(name));
            }
        }
        Ok(None)
    }
}

/// ssh-keygen skips malformed `allowed_signers` lines with only a warning,
/// then reports no match like any unknown signer
fn ssh_complaint(output: &Output) -> Option<String> {
    let detail = String::from_utf8_lossy(&output.stderr)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && *line != "No principal matched.")
        .collect::<Vec<_>>()
        .join("; ");
    (!detail.is_empty()).then(|| printable(&detail))
}

fn run(program: &str, command: &mut Command) -> Result<Output> {
    match command.output() {
        Ok(output) => Ok(output),
        Err(err) if err.kind() == ErrorKind::NotFound => {
            user_bail!("this needs {program} on PATH")
        },
        Err(err) => Err(err).with_context(|| format!("run {program}")),
    }
}
