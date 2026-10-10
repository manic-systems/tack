use std::collections::{
    BTreeMap,
    BTreeSet,
};

use super::*;
use crate::pins::PinType;

fn inputs(names: &[&str]) -> Vec<pins::Input> {
    names
        .iter()
        .map(|name| {
            pins::Input {
                name:        (*name).to_owned(),
                url:         format!("github:owner/{name}"),
                submodules:  false,
                pin_type:    PinType::Flake,
                unpack:      None,
                dir:         None,
                follows:     BTreeMap::new(),
                excludes:    BTreeSet::new(),
                omit_inputs: BTreeSet::new(),
                keep_inputs: BTreeSet::new(),
                signers:     Vec::new(),
                patches:     Vec::new(),
                tag:         None,
                group:       None,
                frozen:      None,
            }
        })
        .collect()
}

fn selected(inputs: &[pins::Input], pick: &[&str], skip: &[&str]) -> Vec<String> {
    let names = pick.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
    let exclude = skip.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
    select(inputs, Selection {
        names:   &names,
        exclude: &exclude,
    })
    .unwrap()
    .iter()
    .map(|input| input.name.clone())
    .collect()
}

#[test]
fn exclude_drops_pins_from_an_unnamed_update() {
    let all = inputs(&["nixpkgs", "home-manager", "nixvim"]);
    assert_eq!(selected(&all, &[], &["home-manager"]), [
        "nixpkgs", "nixvim"
    ]);
}

#[test]
fn exclude_outranks_a_pin_named_on_the_same_run() {
    let all = inputs(&["nixpkgs", "home-manager"]);
    assert!(selected(&all, &["nixpkgs"], &["nixpkgs"]).is_empty());
    assert_eq!(
        selected(&all, &["nixpkgs", "home-manager"], &["nixpkgs"]),
        ["home-manager"]
    );
}
