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
# mielofon-0's own node identity. The `ts` is the ingest time (matching live
# agents) so the classifier derives a live policy record from the replicated
# sample set. A node derives policy only for links of agents registered with
# it, so the link's owner (`spoke-1`) registers on mielofon-0 first; the
# derived record then reaches the other nodes through gossip, and the raw
# sample travels on its own.
TS="$(date +%s)"
LINK='{"from":"spoke-1","to":"hub-a","interface":"awg0"}'
REGISTER="{\"agent\":\"spoke-1\",\"links\":[${LINK}]}"
PAYLOAD="{\"link\":${LINK},\"ts\":${TS},\"rtt_ms\":21,\"loss_pct\":0.7,\"tcp_mbps\":7,\"util_mbps\":3,\"state\":\"quiet\"}"

# POST a JSON body to a pod's own mTLS clients listener (its node identity).
client_post() {
	local pod="$1" path="$2" body="$3"
	local ip
	ip="$(kubectl exec "${pod}" -n "${NAMESPACE}" -- hostname -i 2>/dev/null | tr -d '[:space:]' || true)"
	# Fall back to the pod IP if hostname -i empties; normally it matches the
	# node cert SAN.
	if [ -z "${ip}" ]; then
		ip="$(kubectl get pod "${pod}" -n "${NAMESPACE}" -o jsonpath='{.status.podIP}')"
	fi
	kubectl exec "${pod}" -n "${NAMESPACE}" -- \
		curl -sf \
		--cacert /etc/mielofon/ca.crt \
		--cert /etc/mielofon/node.crt \
		--key /etc/mielofon/node.key \
		-H 'Content-Type: application/json' \
		-d "${body}" \
		"https://${ip}:9552${path}"
}

ingest() {
	client_post "$1" /v1/agent/register "${REGISTER}"
	echo
	client_post "$1" /v1/quality "${PAYLOAD}"
}

# Both replication paths: the owner's derived record (KV, LWW gossip) and the
# raw sample itself (time-series delta).
wait_until_replicated() {
	local container="$1"
	for i in $(seq 1 30); do
		body="$(kubectl exec "${container}" -n "${NAMESPACE}" -- \
			curl -sf 'http://127.0.0.1:9553/v1/quality?from=spoke-1&to=hub-a&interface=awg0' 2>/dev/null || true)"
		samples="$(kubectl exec "${container}" -n "${NAMESPACE}" -- \
			curl -sf "http://127.0.0.1:9553/v1/ts?from=spoke-1&to=hub-a&interface=awg0&since=$((TS - 1))" 2>/dev/null |
			python3 -c "import sys,json;print(len(json.load(sys.stdin)['samples']))" 2>/dev/null || echo 0)"
		if [ -n "${body}" ] && [ "${samples}" -ge 1 ]; then
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

log "e2e PASS: 3-node cluster formed; derived record and raw sample replicated to all nodes"
exit 0
