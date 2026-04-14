{
  description = "Sandbox environment for Claude Code in wikidesk";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    llm-agents = {
      url = "github:numtide/llm-agents.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
  };

  outputs = { nixpkgs, flake-utils, llm-agents, rust-overlay, crane, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
          config.allowUnfreePredicate = pkg: builtins.elem (pkgs.lib.getName pkg) [
            "drawio"
          ];
        };
        craneLib = crane.mkLib pkgs;
        claude-code = llm-agents.packages.${system}.claude-code;
        hooksmith =
          let
            hooksmithSrc = pkgs.fetchFromGitHub {
              owner = "TomPlanche";
              repo = "hooksmith";
              rev = "v1.13.0";
              hash = "sha256-03EXvJctt/Ro27rna7DrCR1IdxIH2kFEQobSbK84p0s=";
            };
          in
          craneLib.buildPackage {
            src = hooksmithSrc;
            strictDeps = true;
            doCheck = false;
          };
      in {
        packages.default = pkgs.buildFHSEnv {
          name = "wikidesk-env";
          targetPkgs = pkgs: with pkgs; [
            claude-code
            git
            cacert
            jq
            curl
            (rust-bin.stable.latest.default.override {
              extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
            })
            gcc
            pkg-config
            openssl
            openssl.dev
            nodejs
            drawio-headless
            librsvg
            fontconfig
            inter
            roboto
            dejavu_fonts
            neovim
            hooksmith
          ];
          profile = ''
            export NPM_CONFIG_CACHE="$PWD/.npm-cache"
            export LANG="en_US.UTF-8"
            export SSL_CERT_FILE="${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
            export NIX_SSL_CERT_FILE="${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
          '';
          runScript = "bash";
        };
      });
}
