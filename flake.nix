{
  description = "Minimal, fast pure-Rust coding agent with persistent memory";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems =
        f:
        nixpkgs.lib.genAttrs systems (
          system:
          f {
            inherit system;
            pkgs = nixpkgs.legacyPackages.${system};
          }
        );
    in
    {
      packages = forAllSystems (
        { pkgs, ... }:
        rec {
          dirge = pkgs.callPackage ./nix/package.nix { src = self; };
          dirge-bin = pkgs.callPackage ./nix/bin.nix { };
          default = dirge;
        }
      );

      apps = forAllSystems (
        { system, ... }:
        {
          default = {
            type = "app";
            program = "${self.packages.${system}.default}/bin/dirge";
          };
        }
      );

      devShells = forAllSystems (
        { pkgs, ... }:
        {
          default = pkgs.callPackage ./nix/devshell.nix { };
        }
      );

      checks = forAllSystems (
        { system, ... }:
        {
          build = self.packages.${system}.default;
        }
      );

      formatter = forAllSystems ({ pkgs, ... }: pkgs.nixfmt-rfc-style);

      overlays.default = final: prev: {
        dirge = final.callPackage ./nix/package.nix { src = self; };
        dirge-bin = final.callPackage ./nix/bin.nix { };
      };
    };
}
