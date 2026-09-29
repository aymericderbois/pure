{
  description = "A modern terminal-based word processor for Markdown and other structured text documents";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
    in
    {
      packages = forAllSystems (pkgs: {
        pure = pkgs.rustPlatform.buildRustPackage {
          pname = cargoToml.package.name;
          version = cargoToml.package.version;
          src = self;
          cargoLock.lockFile = ./Cargo.lock;

          meta = {
            description = cargoToml.package.description;
            homepage = cargoToml.package.repository;
            license = pkgs.lib.licenses.mit;
            mainProgram = "pure";
          };
        };
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.pure;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.pure ];
          packages = with pkgs; [
            rustfmt
            clippy
            rust-analyzer
          ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
