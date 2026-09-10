{
  description = "git-wt macOS development shell";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { nixpkgs, ... }:
    let
      system = "aarch64-darwin";
      pkgs = import nixpkgs { inherit system; };
      manifest = builtins.fromTOML (builtins.readFile ./Cargo.toml);
    in
    {
      devShells.${system}.default =
        assert pkgs.lib.assertMsg (pkgs.lib.versionAtLeast pkgs.rustc.version manifest.package.rust-version)
          "git-wt requires Rust ${manifest.package.rust-version} or newer";
        pkgs.mkShell {
          packages = with pkgs; [
            cargo
            clippy
            git
            rust-analyzer
            rustc
            rustfmt
          ];

          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };

      formatter.${system} = pkgs.nixfmt-tree;
    };
}
