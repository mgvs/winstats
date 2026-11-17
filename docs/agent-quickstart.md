# winstats agent quickstart

Watch other computers from the winstats strip on your taskbar: a Linux server, a Mac, a Raspberry Pi, a cloud VM, another
Windows PC. Each of them runs the small **winstats agent**; winstats on your desk finds it, connects and shows its CPU,
memory, disks, network, GPU, temperatures and fans next to your own.

## Step 1: install the agent on the other machine

Open a terminal **on the machine you want to watch** and paste the command for its system. It downloads the newest
agent from the [releases](https://github.com/mgvs/winstats/releases/latest), installs it, starts it now and at every
boot, gives it a random token and prints the machine's address and token at the end.

### Linux, macOS

```bash
curl -fsSL https://raw.githubusercontent.com/mgvs/winstats/main/install/agent.sh | bash
```

- **Linux** (x86_64, aarch64: Ubuntu, Debian, Fedora, Raspberry Pi OS, Jetson, any distribution): a systemd service
  `winstats-agent`, the agent in `/usr/local/bin`, its config in `/etc/winstats-agent/agent.toml`. It asks for your
  password through `sudo`. Without root or sudo it installs for your user only (a user service).
- **macOS** (Apple silicon and Intel): `winstats-agent.app` in `/Applications` and a LaunchAgent that starts it at
  login. Run it as yourself, without `sudo`.

### Windows

PowerShell (the prompt shows `PS C:\`):

```powershell
irm https://raw.githubusercontent.com/mgvs/winstats/main/install/agent.ps1 | iex
```

Command Prompt (`C:\` without `PS`):

```bat
curl -fsSL https://raw.githubusercontent.com/mgvs/winstats/main/install/agent.cmd -o agent.cmd && agent.cmd
```

The agent goes to `%LOCALAPPDATA%\Programs\winstats-agent` and starts at logon. From an administrator PowerShell the
script also allows it in Windows Firewall on private networks; otherwise Windows asks once, allow it.

> A Windows PC that runs winstats itself needs no agent: right-click its strip, *Remote machines > Share this machine
> on the network*.

## Step 2: add the machine in winstats

Right-click the winstats strip on your taskbar, open **Remote machines**.

- **Same network:** tick *Discover agents on the network*. The machine shows up in the list by its name; open it,
  pick *Connect*, enter the token the installer printed and choose what to show (CPU, RAM, Disk, Network, GPU).
- **Another network** (a cloud VM, another subnet, a VPN): *Add by address…*, type the address and then the token
  the installer printed.

A green dot in the list marks the machines that are connected and answering.

## What the installer printed

```text
==> winstats agent is running on port 47778

    address  192.168.1.20
    token    3f9c0e…
```

Lost it? The token is in the agent's config: `sudo grep token /etc/winstats-agent/agent.toml` (Linux),
`grep token ~/.config/winstats-agent/agent.toml` (macOS), `Select-String token $env:APPDATA\winstats-agent\agent.toml`
(Windows).

## Update

Run the same command again. It replaces the agent and keeps its config, id and token.

## Options

| | Linux, macOS | Windows (set before the command) |
|---|---|---|
| your own token | `curl … \| bash -s -- --token SECRET` | `$env:WINSTATS_TOKEN = 'SECRET'` |
| the name shown in winstats | `curl … \| bash -s -- --name kitchen-pi` | `$env:WINSTATS_NAME = 'kitchen-pi'` |
| no token (anyone on the network may read the machine) | `--no-token` | `$env:WINSTATS_NO_TOKEN = '1'` |
| a given release | `WINSTATS_VERSION=v0.3.4` before `bash` | `$env:WINSTATS_VERSION = 'v0.3.4'` |
| leave Windows Firewall alone | | `$env:WINSTATS_NO_FIREWALL = '1'` |

## Uninstall

```bash
curl -fsSL https://raw.githubusercontent.com/mgvs/winstats/main/install/agent.sh | bash -s -- --uninstall
```

```powershell
$env:WINSTATS_UNINSTALL = '1'; irm https://raw.githubusercontent.com/mgvs/winstats/main/install/agent.ps1 | iex
```

The config with the id and token stays, so a later install comes back as the same machine.

## Network and safety

- The agent listens on **TCP 47778** and announces itself on **UDP 47777** (multicast 239.255.77.77). Discovery works
  within one network segment; across routers, VPNs and clouds use *Add by address…*.
- The data is plain JSON and the token travels in the clear: fine on your own network, **not on the internet**. For a
  cloud VM keep 47778 closed in its security group and come in over a VPN (WireGuard, Tailscale) or an SSH tunnel:
  `ssh -N -L 47779:127.0.0.1:47778 user@vm`, then *Add by address…* `127.0.0.1:47779`.
- On Linux the installer does not open the host firewall. With `ufw` or `firewalld` active it prints the commands that
  let your network in.
- The scripts are short and readable: [agent.sh](../install/agent.sh), [agent.ps1](../install/agent.ps1). To look
  first, download, read, then run: `curl -fsSLO https://raw.githubusercontent.com/mgvs/winstats/main/install/agent.sh`,
  `bash agent.sh`.

## Troubleshooting

| | |
|---|---|
| Not in the list | Discovery is off, or the machine is in another network: use *Add by address…*. Check the firewall on both sides. |
| Not answering, no green dot | The token in winstats differs from the agent's: *Remote machines > the machine > Token…* and enter the one the installer printed. |
| Linux service | `systemctl status winstats-agent`, `journalctl -u winstats-agent -e` |
| macOS | `launchctl list org.mgvs.winstats-agent`, log in `~/Library/Logs/winstats-agent.log` |
| Windows | `Get-Process winstats-agent`; Defender may hold back an unsigned download for a moment |
| What the agent sees | `winstats-agent --dump` prints one report; `winstats-agent watch HOST --token T` connects like winstats |
