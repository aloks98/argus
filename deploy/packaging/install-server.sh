#!/usr/bin/env bash
# Installs the Argus control plane from this tarball's own directory. Run as
# root after extracting the argus-server release tarball:
#   sudo ./install.sh
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
  echo "install-server.sh must be run as root (try: sudo ./install.sh)" >&2
  exit 1
fi

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

install -m 755 "$DIR/argus" /usr/local/bin/argus
install -m 644 "$DIR/argus.service" /etc/systemd/system/argus.service

install -d -m 755 /etc/argus

# Never clobber an existing config -- only seed it the first time.
if [ ! -e /etc/argus/argus.env ]; then
  install -m 600 "$DIR/argus.env.example" /etc/argus/argus.env
  echo "Wrote /etc/argus/argus.env from the example -- edit it before starting."
else
  echo "/etc/argus/argus.env already exists -- leaving it untouched."
fi
# The env file holds ARGUS_DATABASE_URL/ARGUS_FIELD_KEY -- keep it
# unreadable by anyone but root regardless of which branch above ran.
chmod 600 /etc/argus/argus.env

systemctl daemon-reload

cat <<'EOF'

Argus control plane installed.

Next steps:
  1. Edit /etc/argus/argus.env (ARGUS_DATABASE_URL, ARGUS_FIELD_KEY,
     ARGUS_PUBLIC_URL, and OIDC settings if you want SSO).
  2. sudo systemctl enable --now argus
  3. journalctl -u argus -f
EOF
