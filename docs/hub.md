# The hub (optional)

Without a hub, every client pairs with every host (PIN). With one, a device
logs in once and then reaches every computer shared under the same account,
and connections that cannot be made directly (away from home without a
VPN) go through the hub. The hub never sees the stream: video and input
stay end-to-end encrypted between the devices.

## Logging in

Settings → App → Account → Log in (start window only). The hub starts an
OAuth device flow with its OpenID provider (Pocket ID): the app shows a code
and the provider's page; approving it there (passkey) logs the device in.
The hub then issues the device its own credential (stored in `hub.toml` in
the config directory; on Windows also in the service's `pairing`
directory) and keeps no provider tokens. Removing a device in the account
list logs it out.

The hub's database is the account's trust list: anyone who can log in to
the provider's `oriel` client (the `admins` group) can reach every shared
computer of that account.

## What devices do

- Hosts report every 30 s where they listen (port, LAN addresses) and
  accept clients that are devices of their account
  (`oriel-host/src/server.rs`, `report_to_hub`, `hub_trusts`).
- Clients add the account's hosts to their known hosts
  (`oriel_net::hub::store_devices`), so they show up and connect like paired
  ones. Connecting tries the known address and every reported address at
  once, then asks the hub to relay (`oriel-client/src/connection.rs`,
  `open_any`).

## Relaying

`POST /api/relay` returns a one-time token for one host. The client sends
`ORIEL-RELAY1` + token from the UDP socket its connection will use to the
hub's relay port; the hub answers `ORIEL-RELAY-OK` and from then on forwards
that socket's datagrams to the host (on the hub's LAN) and back. A relay
idle for a minute ends. Hosts see relayed clients coming from the hub's
network, so their firewall must accept private addresses (the Windows
installer's rule does).

`ORIEL_DEBUG_FORCE_RELAY=1` makes a client skip direct routes, to test the
relay on one network.

## Push wake-ups

With `ORIEL_HUB_FCM_SERVICE_ACCOUNT` set, `POST /api/wake` sends a
high-priority Firebase data message (`type=wake`) to a device that reported
a push token, to start sharing.

Android asks before every screen share, so by default the phone shows a
notification that starts it. To share unattended, grant two permissions
once over adb:

```sh
adb shell appops set org.fruitiex.oriel PROJECT_MEDIA allow        # approve the share without asking
adb shell appops set org.fruitiex.oriel SYSTEM_ALERT_WINDOW allow  # open from the background
```

A wake-up then lights the screen (the lock screen stays), starts sharing
and moves the app to the back. Without a push, test it with
`adb shell am broadcast -a org.fruitiex.oriel.WAKE -n org.fruitiex.oriel/.WakeReceiver`.

## Deployment

The homelab runs it (`FruitieX/homelab`, `apps/oriel-hub`) at
https://oriel.fruitiex.org; `.github/workflows/hub.yml` builds the container
(`nix build .#hub-container`) on changes and dispatches the digest to the
homelab. The relay listens on UDP 48700, which must be reachable from the
internet (e.g. a LoadBalancer IP with the port forwarded). Configuration is by environment; see
`crates/oriel-hub/src/main.rs`.

Running one locally:

```sh
ORIEL_HUB_OIDC_ISSUER=https://id.example.org ORIEL_HUB_OIDC_CLIENT_ID=oriel \
ORIEL_HUB_OIDC_CLIENT_SECRET=... ORIEL_HUB_RELAY_BIND=0.0.0.0:48700 \
ORIEL_HUB_RELAY_PUBLIC=host:48700 cargo run -p oriel-hub
```
