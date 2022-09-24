{ pkgs ? import <nixpkgs> {} }:
pkgs.mkShell {
  buildInputs = with pkgs; [
    cargo
    clippy
    rust-analyzer
    rustc
    rustfmt
    openssl
    pkg-config
  ];
  RUST_BACKTRACE = 1;
}
