{
  config,
  lib,
  pkgs,
  ...
}:

let
  inherit (lib)
    mkEnableOption
    mkIf
    mkOption
    mkPackageOption
    types
    ;

  cfg = config.programs.z13helper;
in
{
  options.programs.z13helper = {
    enable = mkEnableOption "z13helper GUI and CLI";

    package = mkPackageOption pkgs "z13helper" { };

    systemd = {
      enable = mkEnableOption "z13helper user systemd service" // {
        default = false;
      };

      startHidden = mkOption {
        type = types.bool;
        default = true;
        description = "Set `Z13HELPER_START_HIDDEN=1` so the resident UI starts hidden.";
      };
    };

    gamescopeScale = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "1.5";
      description = "Optional `Z13HELPER_GAMESCOPE_SCALE` override (clamped 1.0–3.0 by the app).";
    };

  };

  config = mkIf cfg.enable {
    home.packages = [ cfg.package ];

    systemd.user.services.z13helper = mkIf cfg.systemd.enable {
      Unit = {
        Description = "z13helper - GTK4 control panel";
        After = [ "graphical-session.target" ];
        PartOf = [ "graphical-session.target" ];
      };
      Service = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/z13helper";
        Environment =
          [
            "PATH=${cfg.package}/bin:/usr/bin:/bin"
          ]
          ++ lib.optional cfg.systemd.startHidden "Z13HELPER_START_HIDDEN=1"
          ++ lib.optional (cfg.gamescopeScale != null) "Z13HELPER_GAMESCOPE_SCALE=${cfg.gamescopeScale}";
        EnvironmentFile = [ "-%t/gamescope-environment" ];
        Restart = "on-failure";
        RestartSec = 3;
      };
      Install = {
        WantedBy = [ "graphical-session.target" ];
      };
    };
  };
}
