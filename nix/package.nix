# SPDX-License-Identifier: EUPL-1.2
{
  lib,
  rustPlatform,
  stdenv,
  pkg-config,
  openssl,
  libgit2,
  libssh2,
  zlib,
  clang,
  wild ? null,
}:
let
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
    pkg-config
  ]
  ++ lib.optionals hasWild [
    wild
    clang
  ];
  buildInputs = [
    openssl
    libgit2
    libssh2
    zlib
  ];

  # link nixpkgs c libs without vendored copies
  env = {
    LIBGIT2_NO_VENDOR = 1;
    OPENSSL_NO_VENDOR = 1;
  }
  // lib.optionalAttrs hasWild {
    RUSTFLAGS = "-Clinker=${clang}/bin/clang -Clink-arg=--ld-path=wild";
  };

  meta = {
    description = "flake-like toml nix pins, lazily fetched and transformed";
    mainProgram = "tack";
  };
}
