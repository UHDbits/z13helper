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

  settingsNonEmpty = cfg.settings != { };

  # Render supported daemon knobs as z13helperctl invocations.
  applySettingsScript =
    pkgs.writeShellScript "z13helperd-apply-settings" (
      let
        ctl = "${cfg.package}/bin/z13helperctl";
        s = cfg.settings;
        battery =
          lib.optionalString (s ? batteryLimit)
            "${ctl} battery-limit ${toString s.batteryLimit}\n";
        chargeOnce =
          lib.optionalString (s ? batteryChargeOnce)
            "${ctl} battery-charge-once ${if s.batteryChargeOnce then "on" else "off"}\n";
        panel =
          lib.optionalString (s ? panelOverdrive)
            "${ctl} panel-overdrive ${if s.panelOverdrive then "on" else "off"}\n";
        lightingKeyboard =
          if s ? lighting && s.lighting ? keyboard then
            let
              k = s.lighting.keyboard;
            in
            if k == null || (k ? enabled && !k.enabled) || (k ? mode && k.mode == "off") then
              "${ctl} lighting keyboard off\n"
            else
              "${ctl} lighting keyboard ${k.mode or "static"} ${k.color or "FFFFFF"} ${toString (k.brightness or 3)}\n"
          else
            "";
        lightingLightbar =
          if s ? lighting && s.lighting ? lightbar then
            let
              k = s.lighting.lightbar;
            in
            if k == null || (k ? enabled && !k.enabled) || (k ? mode && k.mode == "off") then
              "${ctl} lighting lightbar off\n"
            else
              "${ctl} lighting lightbar ${k.mode or "static"} ${k.color or "FFFFFF"} ${toString (k.brightness or 3)}\n"
          else
            "";
      in
      ''
        set -eu
        # Wait briefly for the daemon socket to appear after z13helperd starts.
        for _ in $(seq 1 50); do
          if [ -S /run/z13helper/z13helperd.sock ]; then
            break
          fi
          sleep 0.1
        done
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
        type = types.attrs;
        default = { };
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
          starts via `z13helperctl`. Supported keys:

          - `batteryLimit` (int 40–100)
          - `batteryChargeOnce` (bool)
          - `panelOverdrive` (bool)
          - `lighting.keyboard` / `lighting.lightbar` — attrsets with
            `mode`, `color`, `brightness`, or `mode = "off"` / `enabled = false`

          Profile, PPT, fan, and undervolt configuration belongs in the user
          `config.json` (see the Home Manager module `programs.z13helper.settings`).
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
          UMask = "0007";
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
          ExecStart = "${applySettingsScript}";
          # Client must reach the group-owned socket.
          SupplementaryGroups = [ "z13helper" ];
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
