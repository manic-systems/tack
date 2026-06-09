// SPDX-License-Identifier: EUPL-1.2

use std::{
    env,
    fs,
    path::Path,
};

use eyre::Result;

use super::{
    InitRequest,
    MARKER,
    RESOLVER_NIX,
    SCAFFOLD_FLAKE,
    STARTER_TOML,
};
use crate::{
    error::user_bail,
    project::{
        self,
        Project,
    },
};

pub fn init(project: &Project, request: InitRequest) -> Result<()> {
    let InitRequest {
        force,
        resolver: resolver_only,
        flake,
        convert,
    } = request;
    let (pt, lp, rp) = (
        project.pins_path(),
        project.lock_path(),
        project.resolver_path(),
    );

    if resolver_only {
        if convert {
            user_bail!("--convert cannot be combined with --resolver");
        }
        return write_resolver(project.dir(), &rp, force);
    }

    let cwd = env::current_dir()?;
    let flake_path = cwd.join("flake.nix");
    if convert && !flake_path.exists() {
        user_bail!("no flake.nix to convert in {}", cwd.display());
    }

    if !force {
        let clash = [&pt, &rp]
            .into_iter()
            .filter_map(|path| path.exists().then_some(path.display().to_string()))
            .collect::<Vec<String>>();
        if !clash.is_empty() {
            user_bail!("{} already exists (use --force)", clash.join(", "));
        }
    }
    fs::create_dir_all(project.dir())?;
    project::write_atomic(&pt, STARTER_TOML)?;
    if !lp.exists() {
        project::write_atomic(&lp, "{}\n")?;
    }
    project::write_atomic(&rp, RESOLVER_NIX)?;

    println!("initialized tack in {}", project.dir().display());
    println!("  pins.toml       edit shorturls and inputs here");
    println!("  pins.lock.json  written by `tack update`");
    println!("  default.nix     `import ./.tack` from your flake/config");

    if convert {
        match super::convert::convert(project, &flake_path)? {
            0 => println!("  flake.nix       no inputs to import"),
            1 => println!("  pins.toml       imported 1 input from flake.nix"),
            n => println!("  pins.toml       imported {n} inputs from flake.nix"),
        }
    }

    flake_awareness(flake, project)?;
    Ok(())
}

/// rewrite the resolver to the bundled template; won't clobber a fork unless
/// `force`
fn write_resolver(dir: &Path, path: &Path, force: bool) -> Result<()> {
    if let Ok(current) = fs::read_to_string(path) {
        if current == RESOLVER_NIX {
            println!("resolver already up to date at {}", path.display());
            return Ok(());
        }
        if !current.contains(MARKER) && !force {
            user_bail!(
                "{} has no tack marker, refusing to overwrite (use --force)",
                path.display()
            );
        }
    }
    fs::create_dir_all(dir)?;
    project::write_atomic(path, RESOLVER_NIX)?;
    println!("updated resolver at {}", path.display());
    Ok(())
}

/// `--flake` scaffolds a wired flake and marks the project recomposable;
/// an existing flake.nix belongs to the user
fn flake_awareness(scaffold: bool, project: &Project) -> Result<()> {
    let cwd = env::current_dir()?;
    let path = cwd.join("flake.nix");

    if !path.exists() {
        if scaffold {
            project::write_atomic(&path, SCAFFOLD_FLAKE)?;
            mark_recomposable(project)?;
            if project.dir() != cwd.join(".tack") {
                eprintln!(
                    "tack: scaffolded flake.nix imports ./.tack but the resolver is at {} (adjust \
                     the import)",
                    project.dir().display()
                );
            }
            println!("  flake.nix       wired resolver entry; edit its outputs");
            println!("  pins.toml       marked recomposable for downstream follows");
        } else {
            println!("  hint: `tack init --flake` scaffolds a recomposable flake.nix");
        }
        return Ok(());
    }

    // don't overwrite the user's flake; reflect its wiring into pins.toml
    if scaffold {
        eprintln!("tack: flake.nix exists; left untouched (tack won't overwrite your flake)");
    }
    if fs::read_to_string(&path).is_ok_and(|text| wires_overrides(&text)) {
        mark_recomposable(project)?;
        println!("  pins.toml       marked recomposable (flake.nix already wired)");
    } else {
        print_wiring_blurb();
    }
    Ok(())
}

/// whether `flake.nix` mentions `tackOverrides` in code, not just a comment
pub(super) fn wires_overrides(flake: &str) -> bool {
    flake.lines().any(|line| {
        line.split_once('#')
            .map_or(line, |(code, _)| code)
            .contains("tackOverrides")
    })
}

/// set `[tack] recomposable = true`, preserving existing `[tack]` keys
fn mark_recomposable(project: &Project) -> Result<()> {
    let mut doc = project.load_pins()?;
    doc.mark_recomposable();
    project.save_pins(&doc)
}

fn print_wiring_blurb() {
    println!(
        "
flake.nix is not marked recomposable. to let downstream tack projects
override your pins, thread tackOverrides through outputs:

  outputs =
    {{ self, ... }}@args:
    let inputs = (import ./.tack) {{ overrides = args.tackOverrides or {{ }}; }};
    in {{ }};

then set `[tack] recomposable = true` in .tack/pins.toml."
    );
}

#[cfg(test)]
mod tests {
    use super::wires_overrides;

    #[test]
    fn wires_overrides_ignores_comments() {
        assert!(
            wires_overrides(
                "outputs = { self, ... }@args: (import ./.tack) { overrides = args.tackOverrides \
                 or +             {}; };"
            )
        );
        assert!(!wires_overrides(
            "# threads args.tackOverrides through outputs\n{ }"
        ));
        assert!(!wires_overrides(
            "outputs = { self }: { }; # no tackOverrides here"
        ));
    }
}
