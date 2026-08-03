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

  configJson = builtins.toJSON (
    { version = 1; } // cfg.settings
  );
in
{
  options.programs.z13helper = {
    enable = mkEnableOption "z13helper GUI and CLI";

    package = mkPackageOption pkgs "z13helper" { };

    settings = mkOption {
      type = types.nullOr types.attrs;
      default = null;
      example = {
        active_profile = "balanced";
        auto_switch_on_power_source = true;
        show_hud = true;
      };
      description = ''
        When non-null, written to `$XDG_CONFIG_HOME/z13helper/config.json`
        (schema version 1). Keys match the application config shape
        (`active_profile`, `profiles`, fan curves, etc.).

        Home Manager owns the file when this option is set; GUI edits may be
        overwritten on the next activation. Leave as `null` to let the
        application manage the file itself.
      '';
    };

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

    gtkA11y = mkOption {
      type = types.str;
      default = "none";
      description = "Value for `GTK_A11Y` in the user service (upstream default is `none`).";
    };
  };

  config = mkIf cfg.enable {
    home.packages = [ cfg.package ];

    xdg.configFile."z13helper/config.json" = mkIf (cfg.settings != null) {
      text = configJson;
    };

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
            "GTK_A11Y=${cfg.gtkA11y}"
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
