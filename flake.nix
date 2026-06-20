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

        # crates.io began rejecting requests with no/generic User-Agent
        # in late May 2026 (HTTP 403). nixpkgs has two crate-fetching
        # paths; only `fetchCargoVendor` was patched upstream to send
        # an identifying UA and use static.crates.io
        # (NixOS/nixpkgs#512735). `cargoLock.lockFile` routes through
        # the older `importCargoLock` inline `fetchCrate` which is
        # still broken at HEAD. Use `cargoHash` + `useFetchCargoVendor`
        # to take the fixed path.
        useFetchCargoVendor = true;
        cargoHash = "sha256-CZgnBuS35e8uAE8m52PhAhwSuXXasArIQOglYwuiRi8=";

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
          # Use nixpkgs's pre-built rust-analyzer rather than fenix's
          # nightly. Fenix builds rust-analyzer from source, which
          # routes through the broken `importCargoLock` crate fetcher
          # and is currently blocked by crates.io's User-Agent policy
          # (see the cargoHash comment above). nixpkgs ships a binary
          # from cache.nixos.org — no crate fetching at build time.
          rust-analyzer
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
