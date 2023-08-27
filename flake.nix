{
  inputs.nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";
  outputs = { nixpkgs, ... }:
    let
      forSystem = system:
        let
          pkgs = import nixpkgs { inherit system; };
          toml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
          nativeBuildInputs = with pkgs; [
            pkg-config
            # https://gist.github.com/yihuang/b874efb97e99d4b6d12bf039f98ae31e?permalink_comment_id=4311076#gistcomment-4311076
            rustPlatform.bindgenHook
          ];
          buildInputs = with pkgs; [
            openssl
          ];
        in
          {
            packages.${system}.default = pkgs.rustPlatform.buildRustPackage {
              pname = toml.package.name;
              version = toml.package.version;
              cargoLock.lockFile = ./Cargo.lock;
              src = pkgs.lib.cleanSource ./.;
              inherit nativeBuildInputs buildInputs;
            };
            devShells.${system}.default = pkgs.mkShell {
              name = toml.package.name;
              RUST_BACKTRACE = 1;
              nativeBuildInputs = with pkgs; nativeBuildInputs ++ [
                # Toolchain.
                cargo
                clippy
                rust-analyzer
                rustc  # For rustc --print sysroot (RUST_SRC_PATH = pkgs.rustPlatform.rustLibSrc doesn't seem necessary).
                rustfmt
                # Goodies.
                cargo-edit
              ];
              buildInputs = with pkgs; buildInputs ++ [
                # Debugging.
                gdb
              ];
            };
          };
    in
      forSystem "x86_64-linux";
}
