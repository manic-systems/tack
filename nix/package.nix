# SPDX-License-Identifier: EUPL-1.2
{
  lib,
  rustPlatform,
  stdenv,
  buildPackages,
  installShellFiles,
  pkg-config,
  zstd,
}:
let
  inherit (buildPackages) clang wild;
  # wild + clang are only used on Linux tier-1 arches
  hasWild =
    stdenv.hostPlatform.isLinux && (stdenv.hostPlatform.isx86_64 || stdenv.hostPlatform.isAarch64);
in
rustPlatform.buildRustPackage {
  pname = "tack";
  version = (lib.importTOML ../Cargo.toml).package.version;
  src = ../.;
  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [
    installShellFiles
    pkg-config
  ]
  ++ lib.optionals hasWild [
    wild
    clang
  ];

  buildInputs = [ zstd ];

  env = lib.optionalAttrs hasWild {
    RUSTFLAGS = "-Clinker=${clang}/bin/${clang.targetPrefix}clang -Clink-arg=--ld-path=wild";
  };

  postInstall = lib.optionalString (stdenv.hostPlatform.emulatorAvailable buildPackages) (
    let
      tack = "${stdenv.hostPlatform.emulator buildPackages} $out/bin/tack";
    in
    ''
      installShellCompletion --cmd tack \
        --bash    <(COMPLETE=bash ${tack}) \
        --zsh     <(COMPLETE=zsh  ${tack}) \
        --fish    <(COMPLETE=fish ${tack}) \
        --nushell <(COMPLETE=nu   ${tack})
    ''
  );

  meta = {
    description = "flake-like toml nix pins, lazily fetched and transformed";
    mainProgram = "tack";
  };
}
