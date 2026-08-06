{
  pkgs,
  ...
}:

{
  packages = [
    pkgs.bashInteractive
    pkgs.cargo-audit
    pkgs.cargo-deny
    pkgs.go-task
    pkgs.dprint
    pkgs.dprint-plugins.dprint-plugin-toml
    # The `badness` CLI is deliberately not declared here: it is not in devenv's
    # pinned nixpkgs. Run the parity check against whatever `badness` is on PATH,
    # or against a local `cargo build --release` of the CLI. CI downloads the
    # binary from the latest jolars/badness release instead.
  ];

  languages = {
    rust = {
      enable = true;
      toolchainFile = ./rust-toolchain.toml;
    };
  };

  git-hooks = {
    hooks = {
      clippy = {
        enable = false;
        settings = {
          allFeatures = true;
        };
      };

      rustfmt = {
        enable = true;
      };
    };
  };
}
