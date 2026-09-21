{
  description = "Conservative cleanup of stale Git worktrees";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = {self, nixpkgs, ...}: let
    systems = [
      "aarch64-darwin"
      "aarch64-linux"
      "x86_64-linux"
    ];
    forAllSystems = nixpkgs.lib.genAttrs systems;
  in {
    packages = forAllSystems (system: let
      pkgs = nixpkgs.legacyPackages.${system};
      lop = pkgs.callPackage ./nix/package.nix {};
    in {
      default = lop;
      inherit lop;
    });

    checks = forAllSystems (system: {
      inherit (self.packages.${system}) lop;
    });

    formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.alejandra);
  };
}
