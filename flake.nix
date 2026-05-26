{
  description = "reeve — runtime that supervises AI coding agents as named, addressable, supervised actors";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    fenix,
    flake-utils,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = nixpkgs.legacyPackages.${system};

      toolchain = fenix.packages.${system}.stable.withComponents [
        "cargo"
        "clippy"
        "rustc"
        "rustfmt"
        "rust-src"
        "llvm-tools-preview"
      ];

      rustPlatform = pkgs.makeRustPlatform {
        cargo = toolchain;
        rustc = toolchain;
      };
    in {
      packages.default = rustPlatform.buildRustPackage {
        pname = "reeve";
        version = "0.1.0";

        src = self;
        cargoLock.lockFile = self + "/Cargo.lock";

        doCheck = false;

        meta = with pkgs.lib; {
          description = "runtime that supervises AI coding agents as named, addressable, supervised actors";
          homepage = "https://github.com/ericbmerritt/reeve";
          license = licenses.asl20;
          mainProgram = "reeve";
          platforms = platforms.unix;
        };
      };

      devShells.default = pkgs.mkShell {
        name = "reeve";

        packages = with pkgs; [
          toolchain
          fenix.packages.${system}.rust-analyzer
          jujutsu
          just
          alejandra
          statix
          cargo-deny
          cargo-llvm-cov
          cargo-nextest
          mdbook
          ripgrep
          prettier
        ];
      };
    });
}
