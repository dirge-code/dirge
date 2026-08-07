{ self }:
{ config, lib, pkgs, ... }:

let
  cfg = config.programs.dirge;
  json = pkgs.formats.json { };
  system = pkgs.stdenv.hostPlatform.system;
in
{
  options.programs.dirge = {
    enable = lib.mkEnableOption "dirge";

    package = lib.mkOption {
      type = lib.types.package;
      # `or` catches the whole selection path, so an unsupported system fails
      # with an actionable message naming the option to set rather than a bare
      # `attribute 'x86_64-darwin' missing`. The flake builds for
      # x86_64-linux, aarch64-linux, and aarch64-darwin.
      default =
        self.packages.${system}.default or (throw
          "dirge provides no package for ${system}; set programs.dirge.package explicitly.");
      # Without this, the option docs render the whole derivation.
      defaultText = lib.literalExpression "dirge.packages.\${system}.default";
      description = "The dirge package to use.";
    };

    settings = lib.mkOption {
      type = json.type;
      default = { };
      description = "Configuration written to dirge/config.json.";
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    xdg.configFile."dirge/config.json" = lib.mkIf (cfg.settings != { }) {
      source = json.generate "dirge-config.json" cfg.settings;
    };
  };
}
