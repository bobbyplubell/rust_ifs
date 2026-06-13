#!/usr/bin/env bash
# Serve web/ over HTTPS so WebCrypto works when you open the site from another
# machine on your LAN.
#
# Why: crypto.subtle (used to generate each peer's Ed25519 identity) is only
# available in a "secure context" — HTTPS, or http://localhost. Plain
# http://<lan-ip>:8000 from another machine disables it, which is the
# "crypto.subtle is undefined / can't access property generateKey" error.
#
# This runs Caddy (in Docker — no Node/Caddy on the host) with a self-signed
# cert from Caddy's internal CA. On the other machine, browse the printed
# https URL and click through the one-time certificate warning: the CA isn't
# installed there, but the origin is still a secure context, so WebCrypto works.
#
# NOTE: this only gets the PAGE loading cross-machine. Two machines won't share
# sheep/votes until the libp2p relay transport is deployed and listed in
# web/config.js RELAYS — BroadcastChannel only connects tabs in one browser.
set -euo pipefail
cd "$(dirname "$0")"

PORT="${PORT:-8443}"
IP="${IP:-$(hostname -I 2>/dev/null | awk '{print $1}')}"
[ -n "$IP" ] || { echo "could not detect a LAN IP; set IP=<your-ip> $0"; exit 1; }

cat > Caddyfile.lan <<EOF
{
	auto_https disable_redirects
	# Connections to a bare IP send no SNI, so Caddy has no hostname to pick a
	# cert by; assume this IP's cert as the default for no-SNI handshakes.
	default_sni $IP
}
https://$IP:$PORT {
	root * /srv
	file_server
	tls internal
}
EOF

echo "Serving web/ over HTTPS."
echo "On the other machine, open:  https://$IP:$PORT   (accept the cert warning once)"
echo "Stop with Ctrl-C."

exec docker run --rm -p "$PORT:$PORT" \
	-v "$PWD/web":/srv:ro,z \
	-v "$PWD/Caddyfile.lan":/etc/caddy/Caddyfile:ro,z \
	caddy
