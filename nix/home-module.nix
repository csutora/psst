flake: { config, lib, pkgs, ... }:

let
  cfg = config.services.psst;
  settingsFormat = pkgs.formats.toml { };
  configFile = settingsFormat.generate "psst-config.toml" cfg.settings;
in
{
  options.services.psst = {
    enable = lib.mkEnableOption "psst, a matrix notification daemon";

    package = lib.mkOption {
      type = lib.types.package;
      default = flake.packages.${pkgs.stdenv.hostPlatform.system}.psst;
      defaultText = lib.literalExpression "inputs.psst.packages.\${system}.psst";
      description = "the psst package to use.";
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
            dms_only = false;
          };
          behavior = {
            show_message_body = true;
            max_body_length = 300;
          };
        }
      '';
    };
  };

  config = lib.mkIf cfg.enable (lib.mkMerge [
    {
      home.packages = [ cfg.package ];
    }

    # macos: .app bundle symlink + launchd agent
    (lib.mkIf pkgs.stdenv.hostPlatform.isDarwin {
      home.file."Applications/psst.app".source =
        "${cfg.package}/Applications/psst.app";

      launchd.agents.psst = {
        enable = true;
        config = {
          Label = "com.csutora.psst";
          ProgramArguments = [
            "${cfg.package}/Applications/psst.app/Contents/MacOS/psst"
            "--config" "${configFile}"
            "daemon"
          ];
          RunAtLoad = true;
          KeepAlive = {
            SuccessfulExit = false;
          };
          ProcessType = "Background";
          StandardOutPath = "/tmp/psst.stdout.log";
          StandardErrorPath = "/tmp/psst.stderr.log";
        };
      };
    })

    # linux: systemd user service
    (lib.mkIf pkgs.stdenv.hostPlatform.isLinux {
      systemd.user.services.psst = {
        Unit = {
          Description = "psst - matrix notification daemon";
          After = [ "network-online.target" ];
          Wants = [ "network-online.target" ];
        };
        Service = {
          Type = "simple";
          ExecStart = "${cfg.package}/bin/psst --config ${configFile} daemon";
          Restart = "on-failure";
          RestartSec = 10;
        };
        Install = {
          WantedBy = [ "default.target" ];
        };
      };
    })
  ]);
}
