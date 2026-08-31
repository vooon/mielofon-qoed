#!/bin/sh
# shellcheck shell=sh
# shellcheck disable=SC2154 # peer_ip is assigned via eval (dynamic var name)
set -eu
# Mielofon e2e per-pod bootstrap.
#
# Runs inside the controller image as the mielofon user. Resolves the three
# StatefulSet peers via the headless service, copies the shared e2e CA into a
# writable dir, generates this node's own mTLS identity (SAN = pod IP), renders
# the TOML and execs the daemon.
#
# Because the CA files come from a read-only Secret but openssl's
# -CAcreateserial writes ca.srl next to the CA cert, we copy the CA into the
# pod's writable emptyDir first.

: "${NODE_NAME:?metadata.namespace / NODE_NAME env required}"
: "${POD_IP:?status.podIP / POD_IP env required}"
SERVICE="${MIELOFON_SERVICE:-mielofon}"
NS="${MIELOFON_NAMESPACE:-mielofon-e2e}"
PEER_COUNT="${MIELOFON_PEER_COUNT:-3}"
WORK_DIR="/etc/mielofon"

mkdir -p "${WORK_DIR}"

# 1. The CA comes from a read-only secret; copy it where we may write (serial).
cp /etc/mielofon-ca/ca.key "${WORK_DIR}/ca.key"
cp /etc/mielofon-ca/ca.crt "${WORK_DIR}/ca.crt"
chmod 600 "${WORK_DIR}/ca.key"

# 2. Resolve every peer pod (including ourselves) to an IPv4 address.
#    Accumulate the TOML `[cluster.members]` lines as we go (POSIX-safe; no
#    arrays needed).
i=0
while [ "$i" -lt 120 ]; do
	unset MISSING
	for n in $(seq 0 $((PEER_COUNT - 1))); do
		peer="mielofon-${n}"
		if [ "$peer" = "$NODE_NAME" ]; then
			eval "PEER_IP_${n}=${POD_IP}"
			continue
		fi
		fqdn="${peer}.${SERVICE}.${NS}.svc.cluster.local"
		ip="$(getent ahosts "${fqdn}" 2>/dev/null | awk '/^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+/ {print $1; exit}')"
		if [ -z "${ip}" ]; then
			MISSING=1
			break
		fi
		eval "PEER_IP_${n}=${ip}"
	done
	[ -z "${MISSING:-}" ] && break
	i=$((i + 1))
	sleep 1
done
if [ -n "${MISSING:-}" ]; then
	echo "timed out resolving mielofon peers" >&2
	exit 1
fi

# 3. Issue this node's own leaf (server+client auth) pinned to the pod IP.
/usr/local/bin/mielofon-controller cert node \
	--name "${NODE_NAME}" \
	--ip "${POD_IP}" \
	--ca-key "${WORK_DIR}/ca.key" \
	--ca-crt "${WORK_DIR}/ca.crt" \
	--key "${WORK_DIR}/node.key" \
	--crt "${WORK_DIR}/node.crt"

# 4. Render the TOML: advertise our pod IP, list all peers.
{
	echo "[node]"
	echo "name = \"${NODE_NAME}\""
	echo "advertise = \"${POD_IP}\""
	echo
	echo "[cluster]"
	echo "grace_ttl_secs = 60"
	echo "gossip_interval_secs = 1"
	echo
	echo "[members]"
	for n in $(seq 0 $((PEER_COUNT - 1))); do
		# shellcheck disable=SC2154 # assigned above via eval
		eval "peer_ip=\${PEER_IP_${n}}"
		echo "\"mielofon-${n}\" = \"${peer_ip}\""
	done
	echo
	echo "[tls]"
	echo "ca = \"${WORK_DIR}/ca.crt\""
	echo "cert = \"${WORK_DIR}/node.crt\""
	echo "key = \"${WORK_DIR}/node.key\""
	echo
	echo "[otel]"
	echo "enabled = false"
} > "${WORK_DIR}/mielofon-controller.toml"

exec /usr/local/bin/mielofon-controller daemon "${WORK_DIR}/mielofon-controller.toml"
