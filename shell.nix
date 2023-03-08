let
  pkgs = import <nixpkgs> {};
  fenix = (import (fetchTarball "https://github.com/nix-community/fenix/archive/main.tar.gz") {}).stable;
in
pkgs.mkShell {
  RUST_BACKTRACE = 1;
  nativeBuildInputs = with pkgs; [
    (fenix.withComponents [
      "cargo"
      "clippy"
      # Only available under the 'complete' (nightly) toolchain, not 'stable'.
      # "miri"
      "rust-analyzer"
      "rust-src"
      "rustc"
      "rustfmt"
    ])
    cargo-edit
    pkg-config
  ];
  buildInputs = with pkgs; [
    openssl
  ];
}
