# psst

a matrix notification daemon.\
intended to be used with clients like [iamb](https://iamb.chat).

## features

- native notifications on macos (UNUserNotificationCenter) and linux (dbus)
- end-to-end encryption support with device verification and key backup
- highly configurable notification filters with room and sender-specific overrides
- notifications dismissed automatically on read receipts from other sessions
- config hot-reload via file watcher and sighup
- nix flake with home-manager module (macos + linux)

## requirements

- a matrix homeserver with sliding sync support (synapse, continuwuity, etc)
- a nix setup with home-manager
- linux: systemd, dbus, a notification daemon

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

this is handled by home-manager, but psst looks for `config.toml` at:
- macos: `~/Library/Application Support/com.csutora.psst/config.toml`
- linux: `~/.config/psst/config.toml`

if the file doesn't exist, the defaults below are used:

```toml
[notifications]
enabled = true

# sound played for noisy notifications
# macos: name of a sound in /System/Library/Sounds/
# linux: an xdg sound name, e.g. "message-new-instant"
sound = "Blow"

# room invites
invites = "noisy"

[notifications.dms]
# "off" / "silent" / "noisy"
unencrypted = "noisy"
encrypted = "noisy"
mentions_you = "noisy"
mentions_room = "noisy"
keyword_match = "noisy"
calls = "noisy"
reactions = "noisy"

# "off" / "silent" / "noisy" / "replace" (replaces existing notification)
edits = "replace"

# silence notifications that arrive within this many seconds of the previous one in the same room
noisy_debounce_seconds = 0

# case insensitive substring match on message body
keywords = []

[notifications.rooms]
unencrypted = "silent"
encrypted = "silent"
mentions_you = "noisy"
mentions_room = "silent"
keyword_match = "noisy"
calls = "noisy"
reactions = "silent"
edits = "replace"
noisy_debounce_seconds = 0
keywords = []

# per-room override examples
# any field above can be set per-room
# get the room id with 'psst list-rooms'
[notifications.rooms."!noisy_room:example.com"]
mute = true # shortcut to turn everything off

# sender-based override examples
# noisy / silent / off: bypass per-room rules and force this noise level for the sender
[notifications.senders]
noisy = ["@vip:example.com"]
silent = ["@neutral:example.com"]
off = ["@bot:example.com"]

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

**verification request being accepted without user input**
- some clients like iamb do this. make sure to close them if you're verifying with another client

**daemon exits immediately**
- check the logs at `/tmp/psst.stderr.log` (macos) or `journalctl --user -u psst` (linux)
- ensure your homeserver supports sliding sync
