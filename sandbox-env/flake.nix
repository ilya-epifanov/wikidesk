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
  };

  outputs = { nixpkgs, flake-utils, llm-agents, rust-overlay, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
        claude-code = llm-agents.packages.${system}.claude-code;
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
              extensions = [ "rust-src" "rust-analyzer" ];
            })
            gcc
            pkg-config
            openssl
            openssl.dev
            neovim
          ];
          profile = ''
            export LANG="en_US.UTF-8"
            export SSL_CERT_FILE="${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
            export NIX_SSL_CERT_FILE="${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
          '';
          runScript = "bash";
        };
      });
}
