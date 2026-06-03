runic-tray — Windows system-tray front-end for the runic SOCKS5 proxy
=====================================================================

WHAT IT IS
  A tray app that runs the runic SOCKS5 proxy and lets you start/stop it and
  see its state from the notification area (the Raidho rune icon).

INSTALL (portable)
  1. Copy runic-tray.exe anywhere (e.g. %LOCALAPPDATA%\Programs\runic).
  2. Create the config folder and copy the example config:
        mkdir %APPDATA%\runic
        copy runic.yaml.example %APPDATA%\runic\runic.yaml
     Edit %APPDATA%\runic\runic.yaml to taste (an empty upstream pool is valid —
     runic then boots unconfigured and is driven over its admin API).
  3. Double-click runic-tray.exe. The rune icon appears in the tray (Windows 11
     hides new icons under the "^" overflow — drag it onto the taskbar, or
     Settings > Personalization > Taskbar > Other system tray icons to pin it).

USING IT
  Right-click the tray icon for the menu: Start / Stop / Restart / Status /
  Show current IP / Open config / Show logs / Quit.

  The icon colour reflects state:
    teal  = running, traffic proxied
    grey  = stopped
    red   = error

CONFIG
  %APPDATA%\runic\runic.yaml   (the proxy config the tray loads)

LICENSE
  MIT OR Apache-2.0. Source: https://github.com/quazardous/runic
