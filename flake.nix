{
  description = "Eureka reproducible build/runtime inputs for CRAFT";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.05";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        lib = pkgs.lib;

        rust = pkgs.rust-bin.stable."1.91.0".default;
        go = if pkgs ? go_1_24 then pkgs.go_1_24 else pkgs.go;

        # Eureka's checked-in source is the immutable input. Generated state and
        # credentials are deliberately excluded: CRAFT materializes those into
        # its per-install state directory with aiprover-eureka-env-init.
        cleanEurekaSource = lib.cleanSourceWith {
          src = ./.;
          filter = path: type:
            let name = baseNameOf path;
            in !(
              lib.elem name [
                ".git"
                ".lake"
                "target"
                "__pycache__"
                ".pytest_cache"
                ".mypy_cache"
                ".secrets"
                "pool-packages"
                "pool-packages-tapis"
                "pool.token"
                "runs"
              ]
            );
        };

        runtimeSource = pkgs.runCommand "eureka-runtime-source" {} ''
          test -f ${cleanEurekaSource}/comparator/lakefile.toml || {
            echo "Eureka source is missing Comparator submodule contents; fetch the flake with submodules enabled." >&2
            exit 1
          }
          test -f ${cleanEurekaSource}/lean-beam/lakefile.lean || {
            echo "Eureka source is missing lean-beam submodule contents; fetch the flake with submodules enabled." >&2
            exit 1
          }

          mkdir -p "$out/share/eureka"
          cp -R ${cleanEurekaSource}/harness "$out/share/eureka/harness"
          cp -R ${cleanEurekaSource}/worktrees "$out/share/eureka/worktrees"
          cp -R ${cleanEurekaSource}/comparator "$out/share/eureka/comparator"
          cp -R ${cleanEurekaSource}/lean-beam "$out/share/eureka/lean-beam"
          mkdir -p "$out/share/eureka/examples"
          cp -R ${cleanEurekaSource}/examples/simple "$out/share/eureka/examples/simple"
          mkdir -p "$out/share/eureka/scripts"
          cp ${cleanEurekaSource}/scripts/env.sh "$out/share/eureka/scripts/env.sh"
        '';

        buildTools = pkgs.buildEnv {
          name = "eureka-build-tools";
          paths = [
            rust
            go
            pkgs.elan
            pkgs.git
            pkgs.which
            pkgs.cacert
            pkgs.stdenv.cc
            pkgs.pkg-config
            pkgs.curl
            pkgs.zstd
            pkgs.python3
            pkgs.coreutils
            pkgs.findutils
            pkgs.gnused
            pkgs.gawk
            pkgs.jq
            pkgs.docker-client
            pkgs.docker-compose
          ] ++ lib.optionals pkgs.stdenv.isLinux [ pkgs.systemd pkgs.glibc.bin ];
        };
      in {
        packages = {
          default = runtimeSource;
          inherit runtimeSource buildTools;
        };

        devShells.default = pkgs.mkShell {
          packages = [ buildTools ];
          shellHook = ''
            export EUREKA_NIX_SOURCE_PATH=${runtimeSource}/share/eureka
          '';
        };
      });
}
