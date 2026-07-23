# jagex-launcher-linux (`rsclient`)

A native Linux launcher for **RuneScape 3**, **Old School RuneScape** and **RuneLite**,
using a Jagex account — an alternative to the Windows-only Jagex Launcher and to the
abandoned Bolt.

It is not a game client. It signs you in and starts the real, unmodified clients.

> [!IMPORTANT]
> **This is an unofficial, third-party project. It is not made by, endorsed by, or
> affiliated with Jagex Ltd.** "RuneScape", "Old School RuneScape" and "Jagex" are
> trademarks of Jagex Ltd. Jagex is not responsible for anything that happens as a result
> of using this software. Use it at your own risk.
>
> Your credentials go directly to Jagex's own servers and nowhere else — there is no
> intermediary server in this project, and the sign-in page you see is Jagex's own, loaded
> in an embedded browser. Your session is stored only on your machine. Everything is
> auditable in `src/auth/`.

## Requirements

```sh
sudo pacman -S --needed webkit2gtk-4.1 jre17-openjdk   # login window + RuneLite
sudo pacman -S --needed wine                           # only for the official OSRS client
```

`webkit2gtk-4.1` renders Jagex's real login page, so 2FA and social sign-in work exactly
as they do in the official launcher. RuneLite needs a JRE. Wine is only needed for
Jagex's own Old School client — RuneLite plays Old School natively.

## Build and run

```sh
cargo build --release
./target/release/rsclient
```

## How signing in works

Two OAuth legs against `account.jagex.com`, then a session exchange against
`auth.jagex.com`:

1. **Launcher leg** — authorization code + PKCE. You sign in on Jagex's own page in the
   login window. It ends at a redirect to `secure.runescape.com/m=weblogin/launcher-redirect`.
2. **Consent leg** — a hybrid `id_token code` request, ending at `http://localhost#…`.
3. **Game session** — the consent token is traded for a session id and your character list.

Both redirects are **cancelled before the request is sent**, so the authorization code and
id token never leave the process. That is also why nothing here needs to bind port 80, the
way some other Linux launchers do.

The clients are then started with the credentials in their environment
(`JX_SESSION_ID`, `JX_CHARACTER_ID`, `JX_DISPLAY_NAME`), which is the same handoff the
official launcher uses. Legacy pre-Jagex-account logins instead get `JX_ACCESS_TOKEN` and
`JX_REFRESH_TOKEN`; the two sets are mutually exclusive, and sending both makes RuneLite
reject the login.

## Where things are kept

| Path | Contents |
|---|---|
| `~/.local/share/rsclient/session.json` | Session and refresh token, mode `0600` |
| `~/.local/share/rsclient/` | Downloaded clients, and the `HOME` the games see |
| `~/.config/rsclient/config.json` | Settings |

Game clients get `HOME` pointed at the data directory, so their dotfiles land there rather
than in your real home. Deleting `session.json` signs you out.

## Settings

- **RuneLite jar** — use your own jar instead of downloading one.
- **RS3 config URI** — override the default `jav_config.ws`.
- **Launch command wrappers** — e.g. `gamemoderun %command%`. `%command%` is replaced by
  the real command; without it, your command is used as a prefix.

## Notes

- RS3's client is Jagex's official Linux build, extracted from their `.deb` and checked
  against the SHA256 in their package index.
- The RS3 client is started with `SDL_VIDEODRIVER=x11`, since its SDL2 has no Wayland
  backend and would otherwise fail to open a window on a Wayland session.
- Games are started detached (`setsid`, null stdio), so closing the launcher does not
  close the game.

## Credits

The authentication flow was worked out by reading two existing implementations:

- [Bolt](https://codeberg.org/Adamcake/Bolt) by Adamcake (AGPL-3.0) — the C++/CEF launcher
  this one replaces.
- [native-linux-jagex-launcher](https://github.com/melxin/native-linux-jagex-launcher) by
  melxin — a Rust launcher for Old School.

## Licence

[AGPL-3.0-or-later](LICENSE).
