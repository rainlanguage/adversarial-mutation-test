{
  description = "adversarial-mutation-test — the skill, plus mutation-probe, its probe harness as a tested tool.";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    { nixpkgs, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
        lib = pkgs.lib;
        # The crate is a workspace member, so the Cargo.lock lives at the repo
        # root. buildRustPackage needs the lock inside src, so src is the
        # workspace root — filtered to just the manifests + crate. Without the
        # filter, skill/doc churn would rebuild the bin, and a consumer's
        # `nix run` would pay a rebuild for a SKILL.md edit; `target/` is
        # excluded explicitly because a `path:` ref copies the working
        # directory as-is, gitignore not consulted.
        src = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./LICENSE
            # Subtract rather than whitelist src/: a whitelist silently drops
            # anything the crate gains later (tests/, benches/, build.rs).
            (lib.fileset.difference ./mutation-probe-rs (
              lib.fileset.maybeMissing ./mutation-probe-rs/target
            ))
          ];
        };
        # The probe harness the skill's mutation passes run. Tests run in-build
        # via doCheck; invoked directly as `mutation-probe <mutants.toml>` — no
        # wrapper. Consumers: `nix run github:rainlanguage/adversarial-mutation-test#mutation-probe -- mutants.toml`.
        mutation-probe = pkgs.rustPlatform.buildRustPackage {
          pname = "mutation-probe";
          version = "0.1.0";
          inherit src;
          cargoLock.lockFile = ./Cargo.lock;
        };
      in
      {
        packages = {
          inherit mutation-probe;
          default = mutation-probe;
        };
        devShells.default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rustfmt
          ];
        };
      }
    );
}
