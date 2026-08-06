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
      default = self.packages.${system}.default;
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
