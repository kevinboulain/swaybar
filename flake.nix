{
  inputs.nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";
  outputs = { nixpkgs, ... }:
    let
      forSystem = system:
        let
          pkgs = import nixpkgs { inherit system; };
          toml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
        in
          rec {
            packages.${system}.default = pkgs.callPackage ({ lib, openssl, pkg-config, rustPlatform }:
              rustPlatform.buildRustPackage {
                pname = toml.package.name;
                version = toml.package.version;
                cargoLock.lockFile = ./Cargo.lock;
                src = lib.cleanSource ./.;
                nativeBuildInputs = [
                  pkg-config
                  # https://gist.github.com/yihuang/b874efb97e99d4b6d12bf039f98ae31e?permalink_comment_id=4311076#gistcomment-4311076
                  rustPlatform.bindgenHook
                ];
                buildInputs = [
                  openssl
                ];
              }) {};
            devShells.${system}.default = pkgs.mkShell {
              name = toml.package.name;
              RUST_BACKTRACE = 1;
              inputsFrom = [ packages.${system}.default ];
              nativeBuildInputs = with pkgs; [
                # Toolchain.
                clippy
                rust-analyzer
                rustfmt
                # Goodies.
                cargo-edit
              ];
              buildInputs = with pkgs; [
                # Debugging.
                gdb
              ];
            };
          };
    in
      forSystem "x86_64-linux";
}
