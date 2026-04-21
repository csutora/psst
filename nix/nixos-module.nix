flake: { config, lib, pkgs, ... }:

let
  cfg = config.services.psst;
  settingsFormat = pkgs.formats.toml { };
  configFile = settingsFormat.generate "psst-config.toml" cfg.settings;
in
{
  options.services.psst = {
    enable = lib.mkEnableOption "psst, a matrix notification daemon";

    package = lib.mkPackageOption pkgs "psst" {
      default = flake.packages.${pkgs.stdenv.hostPlatform.system}.psst;
    };

    dataDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/psst";
      description = "directory for psst state (session, crypto store).";
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "psst";
      description = "user account under which psst runs.";
    };

    group = lib.mkOption {
      type = lib.types.str;
      default = "psst";
      description = "group under which psst runs.";
    };

    logFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "path to log file. if null, logs go to journald.";
    };

    settings = lib.mkOption {
      type = settingsFormat.type;
      default = { };
      description = ''
        psst configuration, serialized to TOML and passed as the config file.
      '';
      example = lib.literalExpression ''
        {
          notifications = {
            messages_one_to_one = "noisy";
            messages_group = "silent";
          };
          behavior = {
            show_message_body = true;
            max_body_length = 300;
          };
        }
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    users.users.${cfg.user} = lib.mkIf (cfg.user == "psst") {
      isSystemUser = true;
      group = cfg.group;
      home = cfg.dataDir;
      createHome = true;
    };

    users.groups.${cfg.group} = lib.mkIf (cfg.group == "psst") { };

    systemd.services.psst = {
      description = "psst - matrix notification daemon";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];

      serviceConfig = {
        Type = "simple";
        User = cfg.user;
        Group = cfg.group;
        ExecStart = lib.concatStringsSep " " ([
          "${cfg.package}/bin/psst"
          "--config ${configFile}"
          "--data-dir ${cfg.dataDir}"
          "daemon"
        ] ++ lib.optionals (cfg.logFile != null) [
          "--log-file ${cfg.logFile}"
        ]);
        Restart = "on-failure";
        RestartSec = 10;

        # sandboxing
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        ReadWritePaths = [ cfg.dataDir ] ++ lib.optionals (cfg.logFile != null) [
          (builtins.dirOf cfg.logFile)
        ];
      };
    };
  };
}
