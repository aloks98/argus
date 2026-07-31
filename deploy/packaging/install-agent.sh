#!/usr/bin/env bash
# Installs the Argus agent from this tarball's own directory. Run as root
# after extracting the argus-agent release tarball:
#   sudo ./install.sh
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
  echo "install-agent.sh must be run as root (try: sudo ./install.sh)" >&2
  exit 1
fi

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

install -m 755 "$DIR/argus-agent" /usr/local/bin/argus-agent
install -m 644 "$DIR/argus-agent.service" /etc/systemd/system/argus-agent.service

install -d -m 755 /etc/argus
# Holds the agent's private key + issued client cert -- keep it readable
# only by root (the agent itself runs as root, per argus-agent.service).
install -d -m 700 /var/lib/argus-agent

# Never clobber an existing config -- only seed it the first time.
if [ ! -e /etc/argus/agent.env ]; then
  install -m 600 "$DIR/agent.env.example" /etc/argus/agent.env
  echo "Wrote /etc/argus/agent.env from the example -- edit it before starting."
else
  echo "/etc/argus/agent.env already exists -- leaving it untouched."
fi
# The env file holds ARGUS_JOIN_TOKEN -- a real fleet-enrollment credential --
# so keep it unreadable by anyone but root regardless of which branch above ran.
chmod 600 /etc/argus/agent.env

systemctl daemon-reload

cat <<'EOF'

Argus agent installed.

Next steps:
  1. Edit /etc/argus/agent.env (ARGUS_AGENT_ENDPOINT, ARGUS_JOIN_TOKEN,
     ARGUS_CA_CERT, ARGUS_DATA_DIR).
  2. sudo systemctl enable --now argus-agent
  3. journalctl -u argus-agent -f
EOF
