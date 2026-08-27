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

  cfg = config.services.z13helperd;
  prog = config.programs.z13helper;

  lightingDeviceType = types.submodule {
    options = {
      enabled = mkOption {
        type = types.bool;
        default = true;
        description = "Whether this Aura device is enabled.";
      };

      mode = mkOption {
        type = types.enum [
          "static"
          "breathe"
          "cycle"
          "rainbow"
          "strobe"
          "off"
        ];
        default = "static";
        description = "Supported Aura animation mode.";
      };

      color = mkOption {
        type = types.strMatching "^[0-9A-Fa-f]{6}$";
        default = "FFFFFF";
        description = "Six hexadecimal RGB digits.";
      };

      brightness = mkOption {
        type = types.ints.between 0 3;
        default = 3;
        description = "Aura brightness level from 0 through 3.";
      };
    };
  };

  lightingType = types.submodule {
    options = {
      keyboard = mkOption {
        type = types.nullOr lightingDeviceType;
        default = null;
      };

      lightbar = mkOption {
        type = types.nullOr lightingDeviceType;
        default = null;
      };
    };
  };

  settingsType = types.submodule {
    options = {
      batteryLimit = mkOption {
        type = types.nullOr (types.ints.between 40 100);
        default = null;
        description = "Battery charge threshold from 40 through 100 percent.";
      };

      batteryChargeOnce = mkOption {
        type = types.nullOr types.bool;
        default = null;
      };

      panelOverdrive = mkOption {
        type = types.nullOr types.bool;
        default = null;
      };

      lighting = mkOption {
        type = types.nullOr lightingType;
        default = null;
      };
    };
  };

  settingsNonEmpty = cfg.settings != null;

  # Render supported daemon knobs as z13helperctl invocations.
  applySettingsScript =
    pkgs.writeShellScript "z13helperd-apply-settings" (
      let
        quote = lib.escapeShellArg;
        ctl = quote "${cfg.package}/bin/z13helperctl";
        s = cfg.settings;
        battery =
          lib.optionalString (s != null && s.batteryLimit != null)
            "${ctl} battery-limit ${toString s.batteryLimit}\n";
        chargeOnce =
          lib.optionalString (s != null && s.batteryChargeOnce != null)
            "${ctl} battery-charge-once ${if s.batteryChargeOnce then "on" else "off"}\n";
        panel =
          lib.optionalString (s != null && s.panelOverdrive != null)
            "${ctl} panel-overdrive ${if s.panelOverdrive then "on" else "off"}\n";
        renderLighting = device: value:
          if !value.enabled || value.mode == "off" then
            "${ctl} lighting ${device} off\n"
          else
            "${ctl} lighting ${device} ${quote value.mode} ${quote value.color} ${toString value.brightness}\n";
        lightingKeyboard =
          lib.optionalString (s != null && s.lighting != null && s.lighting.keyboard != null)
            (renderLighting "keyboard" s.lighting.keyboard);
        lightingLightbar =
          lib.optionalString (s != null && s.lighting != null && s.lighting.lightbar != null)
            (renderLighting "lightbar" s.lighting.lightbar);
      in
      ''
        set -eu
        # Probe the client, not only the socket path, so the daemon is accepting
        # requests before any declarative setting can reach hardware.
        ready=0
        for _ in $(seq 1 50); do
          if [ -S /run/z13helper/z13helperd.sock ] && ${ctl} status >/dev/null 2>&1; then
            ready=1
            break
          fi
          sleep 0.1
        done
        if [ "$ready" -ne 1 ]; then
          echo "z13helperd socket did not become ready within 5 seconds" >&2
          exit 1
        fi
        ${battery}${chargeOnce}${panel}${lightingKeyboard}${lightingLightbar}
      ''
    );
in
{
  options = {
    services.z13helperd = {
      enable = mkEnableOption "z13helper privileged hardware daemon";

      package = mkPackageOption pkgs "z13helper" { };

      users = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [ "alice" ];
        description = ''
          Users added to the `z13helper` group so they can talk to
          `/run/z13helper/z13helperd.sock`.
        '';
      };

      settings = mkOption {
        type = types.nullOr settingsType;
        default = null;
        example = {
          batteryLimit = 80;
          batteryChargeOnce = false;
          panelOverdrive = false;
          lighting = {
            keyboard = {
              mode = "static";
              color = "FF0000";
              brightness = 3;
            };
          };
        };
        description = ''
          Optional machine-level settings applied once after `z13helperd`
          accepts socket requests via `z13helperctl`. Invalid values are
          rejected during Nix evaluation. Supported keys:

          - `batteryLimit` (int 40–100)
          - `batteryChargeOnce` (bool)
          - `panelOverdrive` (bool)
          - `lighting.keyboard` / `lighting.lightbar` — typed attrsets with
            `mode`, six-digit `color`, `brightness` 0–3, or `enabled = false`

          Profile, PPT, fan, and undervolt configuration belongs in the user
          `config.json`, which remains application-managed.
        '';
      };
    };

    programs.z13helper = {
      enable = mkEnableOption "z13helper GUI and CLI";

      package = mkPackageOption pkgs "z13helper" { };

      enablePowerProfilesDaemon = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Enable `services.power-profiles-daemon` when the daemon or GUI is
          enabled. PPD is a soft dependency: z13helperd warns and continues if
          it is absent, but rejects unknown profile selections.
        '';
      };
    };
  };

  config = lib.mkMerge [
    (mkIf (cfg.enable || prog.enable) {
      assertions = [
        {
          assertion = pkgs.stdenv.hostPlatform.isLinux;
          message = "z13helper is only supported on Linux (GZ302EA).";
        }
      ];
    })

    (mkIf cfg.enable {
      users.groups.z13helper = {
        members = cfg.users;
      };

      environment.systemPackages = [ cfg.package ];

      systemd.services.z13helperd = {
        description = "z13helper hardware control daemon";
        documentation = [ "https://github.com/UHDbits/z13helper#readme" ];
        wantedBy = [ "multi-user.target" ];
        after = [ "systemd-sysusers.service" ];
        unitConfig = {
          ConditionPathExists = "/sys/class/dmi/id/product_name";
          StartLimitIntervalSec = "60s";
          StartLimitBurst = 5;
        };
        serviceConfig = {
          Type = "simple";
          User = "root";
          Group = "z13helper";
          ExecStart = "${cfg.package}/libexec/z13helperd";
          ExecStopPost = "${cfg.package}/libexec/z13helperd --release-ec";
          RuntimeDirectory = "z13helper";
          RuntimeDirectoryMode = "0750";
          StateDirectory = "z13helper";
          StateDirectoryMode = "0750";
          UMask = "0077";
          Restart = "on-failure";
          RestartSec = "1s";
          TimeoutStopSec = "5s";
          KillSignal = "SIGTERM";

          CapabilityBoundingSet = "CAP_SYS_RAWIO CAP_BPF CAP_PERFMON";
          AmbientCapabilities = "CAP_SYS_RAWIO CAP_BPF CAP_PERFMON";
          NoNewPrivileges = true;
          DevicePolicy = "closed";
          DeviceAllow = [
            "char-hidraw rw"
            "char-input r"
          ];

          PrivateNetwork = true;
          PrivateTmp = true;
          ProtectSystem = "strict";
          ProtectHome = true;
          ProtectHostname = true;
          ProtectClock = true;
          ProtectControlGroups = true;
          ProtectKernelLogs = true;
          ProtectKernelModules = true;
          ProtectKernelTunables = false;

          ReadOnlyPaths = [
            "/sys"
            "-/proc/sys"
            "-/proc/sysrq-trigger"
            "-/proc/latency_stats"
            "-/proc/acpi"
            "-/proc/timer_stats"
            "-/proc/fs"
            "-/proc/irq"
          ];
          InaccessiblePaths = [
            "-/proc/kallsyms"
            "-/proc/kcore"
          ];
          ReadWritePaths = [
            "/run/z13helper"
            "/var/lib/z13helper"
            "-/sys/devices/platform/AMDI0105:00/platform-profile"
            "-/sys/devices/platform/asus-nb-wmi"
            "-/sys/firmware/acpi/platform_profile"
            "-/sys/class/power_supply/BAT0/charge_control_end_threshold"
            "-/sys/class/power_supply/BAT1/charge_control_end_threshold"
            "-/sys/devices/virtual/firmware-attributes/asus-armoury"
            "-/sys/kernel/ryzen_smu_drv"
          ];

          RestrictAddressFamilies = [ "AF_UNIX" ];
          RestrictNamespaces = true;
          RestrictRealtime = true;
          RestrictSUIDSGID = true;
          LockPersonality = true;
          MemoryDenyWriteExecute = false;
          LimitMEMLOCK = "infinity";
          SystemCallArchitectures = "native";
        };
      };

      systemd.services.z13helperd-apply-settings = mkIf settingsNonEmpty {
        description = "Apply declarative z13helperd machine settings";
        wantedBy = [ "multi-user.target" ];
        after = [ "z13helperd.service" ];
        requires = [ "z13helperd.service" ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          TimeoutStartSec = "10s";
          ExecStart = "${applySettingsScript}";
          # This unit needs only the same explicit full-control socket
          # authorization as an interactive client; it never needs root or
          # direct hardware access.
          DynamicUser = true;
          SupplementaryGroups = [ "z13helper" ];
          UMask = "0077";
          NoNewPrivileges = true;
          CapabilityBoundingSet = "";
          PrivateDevices = true;
          PrivateNetwork = true;
          PrivateTmp = true;
          ProtectSystem = "strict";
          ProtectHome = true;
          ProtectKernelTunables = true;
          ProtectKernelModules = true;
          ProtectKernelLogs = true;
          ProtectControlGroups = true;
          RestrictAddressFamilies = [ "AF_UNIX" ];
          RestrictNamespaces = true;
          RestrictRealtime = true;
          RestrictSUIDSGID = true;
          LockPersonality = true;
          SystemCallArchitectures = "native";
        };
      };
    })

    (mkIf prog.enable {
      environment.systemPackages = [ prog.package ];
    })

    (mkIf ((cfg.enable || prog.enable) && prog.enablePowerProfilesDaemon) {
      services.power-profiles-daemon.enable = true;
    })
  ];
}
