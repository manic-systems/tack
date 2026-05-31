// SPDX-License-Identifier: EUPL-1.2

use super::{
    MARKER,
    Path,
    Project,
    RESOLVER_NIX,
    Result,
    SCAFFOLD_FLAKE,
    STARTER_TOML,
    bail,
    env,
    fs,
    project,
};

pub fn init(force: bool, resolver_only: bool, flake: bool) -> Result<()> {
    let project = Project::discover();
    let (pt, lp, rp) = (
        project.pins_path(),
        project.lock_path(),
        project.resolver_path(),
    );

    // `--resolver` only bumps the resolver to the bundled template
    if resolver_only {
        return write_resolver(project.dir(), &rp, force);
    }

    if !force {
        let clash = [&pt, &rp]
            .into_iter()
            .filter_map(|path| path.exists().then_some(path.display().to_string()))
            .collect::<Vec<String>>();
        if !clash.is_empty() {
            bail!("{} already exists (use --force)", clash.join(", "));
        }
    }
    fs::create_dir_all(project.dir())?;
    project::write_atomic(&pt, STARTER_TOML)?;
    if !lp.exists() {
        project::write_atomic(&lp, "{}\n")?;
    }
    project::write_atomic(&rp, RESOLVER_NIX)?;

    println!("initialised tack in {}", project.dir().display());
    println!("  pins.toml       edit shorturls and inputs here");
    println!("  pins.lock.json  written by `tack update`");
    println!("  default.nix     `import ./.tack` from your flake/config");

    flake_awareness(flake, &project)?;
    Ok(())
}

/// (re)write just the resolver to the bundled template. refuses to clobber a
/// forked resolver (marker stripped) unless `force`.
fn write_resolver(dir: &Path, path: &Path, force: bool) -> Result<()> {
    if let Ok(current) = fs::read_to_string(path) {
        if current == RESOLVER_NIX {
            println!("resolver already up to date at {}", path.display());
            return Ok(());
        }
        if !current.contains(MARKER) && !force {
            bail!(
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

/// `--flake` scaffolds a wired flake and marks the project recomposable, but
/// only when no flake.nix exists. an existing flake.nix is the user's, never
/// tack's.
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

    // never overwrite the user's flake, just reflect its wiring into pins.toml
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

/// whether `flake.nix` mentions `tackOverrides` in code rather than only a `#`
/// comment.
pub(in crate::commands) fn wires_overrides(flake: &str) -> bool {
    flake.lines().any(|line| {
        line.split_once('#')
            .map_or(line, |(code, _)| code)
            .contains("tackOverrides")
    })
}

/// set `[tack] recomposable = true`, preserving any existing `[tack]` keys.
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

pub fn help() {
    println!(
        "tack: flake-like toml nix pins, lazily fetched and transformed

usage:
  tack [-h|--help|help]
  tack init [--force] [--resolver] [--flake]
  tack update [names...] [--accept]
  tack look [names...] [--verbose|-v]
  tack add <name> <url> [--fetch|--fixed [--unpack tarball|file]]
                        [--dir <d>] [--submodules] [--follows c=p]...
  tack rm <name>
  tack alias <name> <template> | tack alias --rm <name>
  tack dedup
  tack undo [--list]
  tack redo

pin types: flake (default), fetch (source tree only), fixed (FOD)
follows keys may be scoped flake:<name> or tack:<name> (no prefix implies both)

tack lives in ./.tack/ by default
use `import ./.tack` to use inputs

run `tack init --resolver` to update a drifted resolver

"
    );
}
