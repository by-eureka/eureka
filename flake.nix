{
  description = "Eureka runtime source and build tools for CRAFT";

  inputs = {
    self.submodules = true;
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; overlays = [ (import rust-overlay) ]; };
        inherit (pkgs) lib;
        source = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./.dockerignore ./examples ./harness ./lean-beam ./scripts
            ./third_party/codex ./tools/retrieval ./worktrees
          ];
        };
        runtimeSource = pkgs.runCommand "eureka-runtime-source" {} ''
          test -f ${source}/lean-beam/scripts/build-toolchain.cmake || {
            echo "Fetch the Eureka Git flake with its lean-beam submodule." >&2
            exit 1
          }
          mkdir -p "$out/share"
          cp -R ${source} "$out/share/eureka"
        '';
        buildTools = pkgs.buildEnv {
          name = "eureka-build-tools";
          paths = with pkgs; [
            rust-bin.stable."1.91.0".default elan cmake gnumake stdenv.cc gmp
            git pkg-config cacert curl zstd python3 coreutils findutils gnused gawk
            jq which gnutar openssh openssl docker-client docker-compose
          ] ++ lib.optionals pkgs.stdenv.isLinux [ pkgs.bubblewrap pkgs.systemd ];
        };
      in {
        packages = { inherit runtimeSource buildTools; default = runtimeSource; };
        devShells.default = pkgs.mkShell {
          packages = [ buildTools ];
          EUREKA_NIX_SOURCE_PATH = "${runtimeSource}/share/eureka";
        };
      });
}
