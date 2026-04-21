# psst

a matrix notification daemon. it syncs your rooms via sliding sync, evaluates
push rules and local filters, fires native desktop notifications, and dismisses
them when read on another client. intended to be used with clients like [iamb](https://iamb.chat).

## features

- native notifications on macos (UNUserNotificationCenter) and linux (dbus)
- end-to-end encryption support with device verification and key backup
- local filter pipeline: per-room overrides, sender allow/blocklists, quiet hours
- notifications dismissed automatically on read receipts from other sessions
- config hot-reload via file watcher and sighup
- nix flake with home-manager module (macos + linux)

## requirements

- a matrix homeserver with sliding sync support (synapse 1.98+, conduit, etc)
- linux: systemd, dbus, a notification daemon
- macos: none

## install

### nix (flakes)

add psst to your flake inputs:

```nix
{
    inputs = {
        nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";

        home-manager = {
            url = "github:nix-community/home-manager";
            inputs.nixpkgs.follows = "nixpkgs";
        };

        psst = {
            url = "github:csutora/psst";
            inputs.nixpkgs.follows = "nixpkgs";
        };
    };

    outputs = { self, nixpkgs, home-manager, psst, ... }: {
        # for nix-darwin
        darwinConfigurations.your-hostname = darwin.lib.darwinSystem {
            modules = [
                home-manager.darwinModules.home-manager
                {
                    home-manager.users.your-username = {
                        imports = [ psst.homeModules.default ];
                    };
                }
            ];
        };

        # or for nixos
        nixosConfigurations.your-hostname = nixpkgs.lib.nixosSystem {
            modules = [
                home-manager.nixosModules.home-manager
                {
                    home-manager.users.your-username = {
                        imports = [ psst.homeModules.default ];
                    };
                }
            ];
        };
    };
}
```

then in your home-manager configuration:

```nix
services.psst = {
    enable = true;
    settings = {
        notifications = {
            # example, see options below
            messages_one_to_one = "noisy";
            messages_group = "silent";
        };
    };
};
```

## quick start

```sh
# 1. log in
psst login

# 2. verify the session (emoji verification with another session)
psst verify

# 3. confirm notifications work
psst test-notify

# 4. start the daemon
psst daemon
```

## configuration

psst looks for `config.toml` at:
- macos: `~/Library/Application Support/com.csutora.psst/config.toml`
- linux: `~/.config/psst/config.toml`

if the file doesn't exist, the following defaults are used:

```toml
[notifications]
enabled = true

# per-type levels: "off", "silent", "noisy"
messages_one_to_one = "noisy"
messages_group = "silent"
encrypted_one_to_one = "noisy"
encrypted_group = "silent"
invites = "noisy"
calls = "noisy"
reactions = "off"
edits = "off"

# only notify for direct messages
dms_only = false

# example per-room overrides: "all", "mentions_only", "mute"
[notifications.rooms]
"!room:example.com" = "mute"

# example sender filters
[notifications.senders]
always = ["@vip:example.com"]
never = ["@bot:example.com"]

[behavior]
show_message_body = true
max_body_length = 300
max_event_age_secs = 60

[behavior.quiet_hours]
enabled = false
start = "23:00"
end = "07:00"
```

## troubleshooting

**no notifications on macos**
- check system settings > notifications > psst is enabled

**"no session found"**
- run `psst login` first

**encrypted messages not decrypting**
- run `psst verify` to cross-sign this device and import the key backup

**daemon exits immediately**
- check the logs at `/tmp/psst.stderr.log` (macos) or `journalctl --user -u psst` (linux)
- ensure your homeserver supports sliding sync
