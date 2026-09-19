# The Proxy tab (Nginx Proxy Manager)

The Proxy tab shows every host your **Nginx Proxy Manager** (NPM) routes —
domains, forward target and port, enabled state, and certificate status with
expiry.

## Setup

1. Enable the tab in **Settings → Tabs → Proxy**.
2. Open the tab and fill in the form:
   - **Address** — the admin port of your NPM install, e.g. `http://192.168.1.209:81`
     (the port you open the NPM admin panel on; plain HTTP on your LAN).
   - **Email / password** — your NPM login. A **dedicated NPM user** for Sophon
     is recommended over your admin account.
3. **Connect**. Hosts load immediately and refresh every 30 s while the tab is
   visible; the ↻ button forces a refresh, ✕ forgets the stored login.

The credentials live **only** in `~/.config/sophon/settings.conf` on your
machine (written owner-only, 0600) — see SECURITY.md. The client is read-only:
it logs in, lists proxy hosts and lists certificates; it never changes anything
in NPM.

## Trying it without a server

```bash
cargo run --release -p sophon-npm --example npm-mock        # mock API on 127.0.0.1:2381
```

Then connect the tab to `http://127.0.0.1:2381` with any email and password.
