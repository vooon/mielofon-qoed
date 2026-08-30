#!/usr/bin/env bash
# Mielofon 3-node cluster e2e.
#
# Spins up three mielofon-controller pods (StatefulSet, headless service), has
# each pod generate its own mTLS identity from a shared ephemeral CA, then
# asserts:
#   1. every node reports ready=true with all three members; and
#   2. gossip actually converges: a quality record ingested on mielofon-0 is
#      replicated to mielofon-1 and mielofon-2 (the "cluster is formed" proof).
#
# The image must already be built and loaded into the cluster as
# `mielofon-controller:e2e` (see .github/workflows/e2e.yaml / Dockerfile.e2e).
#
# Requirements: kubectl, openssl, python3 (for JSON assertions).
set -euo pipefail

NAMESPACE="${MIELOFON_E2E_NS:-mielofon-e2e}"
PEER_COUNT=3
MANIFESTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/manifests"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "${WORK_DIR}"' EXIT

log() { printf '[e2e] %s\n' "$*"; }

# kubectl must exist and point at a reachable cluster.
command -v kubectl >/dev/null || { echo "kubectl is required" >&2; exit 1; }

# ── 1. Ephemeral CA ────────────────────────────────────────────────────────
log "generating ephemeral e2e CA"
openssl ecparam -name prime256v1 -genkey -noout -out "${WORK_DIR}/ca.key"
openssl req -x509 -new -key "${WORK_DIR}/ca.key" -sha256 -days 1 \
	-subj "/CN=mielofon-e2e-ca" -out "${WORK_DIR}/ca.crt"

kubectl get namespace "${NAMESPACE}" >/dev/null 2>&1 \
	|| kubectl create namespace "${NAMESPACE}"

kubectl -n "${NAMESPACE}" create secret generic mielofon-ca \
	--from-file=ca.key="${WORK_DIR}/ca.key" \
	--from-file=ca.crt="${WORK_DIR}/ca.crt" \
	--dry-run=client -o yaml | kubectl apply -f -

# ── 2. Deploy the cluster ─────────────────────────────────────────────────
log "deploying mielofon StatefulSet (${PEER_COUNT} replicas)"
kubectl apply -f "${MANIFESTS_DIR}/namespace.yaml"
# The CA secret is created first (see step 1); the StatefulSet references it.
kubectl apply -f "${MANIFESTS_DIR}/mielofon.yaml"
kubectl rollout status statefulset/mielofon \
	-n "${NAMESPACE}" --timeout=180s

# ── 3. Wait for all pods ready (readiness = /readyz via exec probe) ──────
log "waiting for controller pods to become ready"
for i in $(seq 0 $((PEER_COUNT - 1))); do
	kubectl wait --for=condition=ready "pod/mielofon-${i}" \
		-n "${NAMESPACE}" --timeout=180s
done

# ── 4. Assertions ──────────────────────────────────────────────────────────
PODS=()
for i in $(seq 0 $((PEER_COUNT - 1))); do
	PODS+=("mielofon-${i}")
done

# Each node: ready + full membership.
for pod in "${PODS[@]}"; do
	status="$(kubectl exec "${pod}" -n "${NAMESPACE}" -- \
		curl -sf http://127.0.0.1:9553/v1/status)"
	log "${pod} /v1/status: ${status}"
	ready="$(python3 -c "import sys,json;d=json.load(sys.stdin);print(d['ready'])" <<<"${status}")"
	[ "${ready}" = "True" ] || { echo "${pod} is not ready" >&2; exit 1; }
	count="$(python3 -c "import sys,json;print(len(json.load(sys.stdin)['members']))" <<<"${status}")"
	[ "${count}" = "${PEER_COUNT}" ] || {
		echo "${pod} sees ${count} members, want ${PEER_COUNT}" >&2
		exit 1
	}
done
log "all nodes ready with ${PEER_COUNT} members"

# Gossip replication: report a link on mielofon-0, watch it appear on the
# other two nodes. The report is pushed over the mTLS clients listener using
# mielofon-0's own node identity.
PAYLOAD='{"link":{"from":"spoke-1","to":"hub-a","interface":"awg0"},"ts":1700000000,"rtt_ms":21,"loss_pct":0.7,"rr_tps":88,"tcp_mbps":7,"udp_mbps":null,"util_mbps":3,"state":"quiet"}'

ingest() {
	local pod="$1"
	local ip
	ip="$(kubectl exec "${pod}" -n "${NAMESPACE}" -- hostname -i 2>/dev/null | tr -d '[:space:]' || true)"
	# Fall back to mTLS to the clients listener on the headless service address
	# if hostname -i empties; normally the pod IP matches the node cert SAN.
	if [ -z "${ip}" ]; then
		ip="$(kubectl get pod "${pod}" -n "${NAMESPACE}" -o jsonpath='{.status.podIP}')"
	fi
	kubectl exec "${pod}" -n "${NAMESPACE}" -- \
		curl -sf \
		--cacert /etc/mielofon/ca.crt \
		--cert /etc/mielofon/node.crt \
		--key /etc/mielofon/node.key \
		-H 'Content-Type: application/json' \
		-d "${PAYLOAD}" \
		"https://${ip}:9552/v1/quality"
}

wait_until_replicated() {
	local container="$1"
	for i in $(seq 1 30); do
		body="$(kubectl exec "${container}" -n "${NAMESPACE}" -- \
			curl -sf 'http://127.0.0.1:9553/v1/quality?from=spoke-1&to=hub-a&interface=awg0' 2>/dev/null || true)"
		if [ -n "${body}" ]; then
			return 0
		fi
		sleep 2
	done
	return 1
}

log "ingesting a quality record on ${PODS[0]}"
ingest "${PODS[0]}"

for consumer in "${PODS[@]:1}"; do
	log "waiting for gossip to replicate to ${consumer}"
	wait_until_replicated "${consumer}" || {
		echo "gossip did not replicate to ${consumer}" >&2
		exit 1
	}
done

log "e2e PASS: 3-node cluster formed; quality record replicated to all nodes"
exit 0