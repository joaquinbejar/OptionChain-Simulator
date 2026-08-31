#!/usr/bin/env bash
#
# Proves that a replica whose dependency is gone reports ITSELF unready, while
# its healthy neighbour keeps serving, and that it recovers without a restart.
#
# The integration suite can ask every replica for its readiness, and since #141
# it can tell which replica answered from the `x-ocs-instance` header. What it
# cannot do is BREAK a dependency for exactly one replica: that is an operator
# action on the containers, not something a client of the deployment can
# arrange. This fixture owns the containers, so it can (issue #145).
#
# What it arranges, and why in this shape:
#
#   1. two networks rather than one. Redis lives on its own, MongoDB on the
#      other, and both replicas are on both. Disconnecting a replica from the
#      Redis network takes Redis away from THAT replica and leaves MongoDB
#      answering, so the readiness body says which dependency failed rather
#      than that everything did;
#   2. the dependency is never stopped. Stopping Redis would break both
#      replicas, which proves nothing about a per-instance signal;
#   3. every probe is addressed to ONE replica by container name from a curl
#      container on the same network. No published ports and no balancer, so
#      the answer came from the replica that was asked, and the
#      `x-ocs-instance` header in it is checked against the id that replica
#      reported while healthy. curl rather than the runtime image's busybox
#      wget because wget does not hand back the headers of a 503, which is the
#      answer this whole fixture is about;
#   4. recovery is asserted by reconnecting the network and requiring the same
#      instance id to answer 200 again. The id is generated once per process,
#      so an UNCHANGED id is what proves the recovery happened without a
#      restart, which is what `/ready` promises an orchestrator.
#
# Usage:
#   scripts/replica_isolation_test.sh [SERVICE_IMAGE]
#
# The image defaults to one built from the working tree, which is what makes
# this a test of the code in front of you. Pass a tag to run it against an
# image that is already built.

set -euo pipefail

# Pinned by digest like everything else here: a mutable tag can be republished,
# and then the fixture proves a behaviour against bytes nobody deploys. These
# are the pins from Docker/docker-compose.yml.
REDIS_IMAGE="redis:8.10.1-alpine@sha256:becdda6c7f4b3fb42e42fd7f120bbf5c54c4caaaf16f26da24e4563d2c1f0576"
MONGO_IMAGE="mongo:8.2.12@sha256:e0ce8c35124d4a9f9785532d1f268f39e9728ffa1cb38f46fa482436424c4bd3"
# The probe. It sits on the network the replicas share with MongoDB, which the
# isolation step never touches, so it can still ask the isolated replica how it
# is doing.
CURL_IMAGE="curlimages/curl:8.19.0@sha256:c03110c736db81bbe1be0296f1f1608c81b954b01626bdfb0a8f84e5bd00ff3c"

SERVICE_IMAGE="${1:-}"
BUILT_IMAGE="ocs-replica-isolation:$$"

REDIS_NET="ocs-iso-redis-$$"
CORE_NET="ocs-iso-core-$$"
REDIS="ocs-iso-redis-server-$$"
MONGO="ocs-iso-mongo-$$"
PROBE="ocs-iso-probe-$$"
FIRST="ocs-iso-replica-a-$$"
SECOND="ocs-iso-replica-b-$$"

REDIS_PASSWORD="password"
MONGO_USER="admin"
MONGO_PASSWORD="password"
PORT="7070"

cleanup() {
    for container in "$FIRST" "$SECOND" "$REDIS" "$MONGO" "$PROBE"; do
        docker rm -f "$container" >/dev/null 2>&1 || true
    done
    for network in "$REDIS_NET" "$CORE_NET"; do
        docker network rm "$network" >/dev/null 2>&1 || true
    done
    # Only what this run built. An image passed in belongs to the caller.
    if [ -z "${SERVICE_IMAGE_WAS_GIVEN:-}" ]; then
        docker image rm -f "$BUILT_IMAGE" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

fail() {
    echo "FAIL: $1" >&2
    docker logs "$FIRST" 2>&1 | tail -20 >&2 || true
    exit 1
}

# A command that fails without a message of its own must not end the run in
# silence: `set -e` exits with no output at all, and a fixture that stops
# halfway looks exactly like one that passed a step it never reached.
trap 'status=$?; [ "$status" = 0 ] || echo "ABORTED at line $LINENO with status $status" >&2' ERR

# One request to ONE replica, headers and body together.
#
# `--fail` is deliberately absent: a 503 is an answer here, not an error, and
# its headers are what say which instance produced it.
probe() {
    local target="$1" path="$2"
    docker exec "$PROBE" \
        curl -sS -i --max-time 5 "http://$target:$PORT$path" 2>/dev/null || true
}

# Both readers below print an empty value rather than failing when what they
# look for is absent. Under `set -euo pipefail` a grep that matches nothing
# fails the pipeline, and the assignment that captures it ends the run: the
# retry loop would then abort on the first attempt that arrives before the
# server does, which is every first attempt.
status_code() {
    printf '%s\n' "$1" | awk '
        toupper($1) ~ /^HTTP\/1\.[01]$/ { code = $2 }
        END { print code }'
}

instance_id() {
    printf '%s\n' "$1" | awk '
        tolower($1) == "x-ocs-instance:" { id = $2 }
        END { gsub(/\r/, "", id); print id }'
}

# The body alone, which is everything after the blank line the headers end
# with.
body_of() {
    printf '%s\n' "$1" | awk 'body { print } /^\r?$/ { body = 1 }'
}

# Waits for a container's readiness probe to answer with a given code, and
# leaves that answer, headers and body, in ANSWER.
#
# Through a variable rather than through stdout on purpose: inside a command
# substitution, `fail` would exit the SUBSHELL, and the run would end on the
# assignment with nothing said about which wait ran out.
wait_for_ready_code() {
    local container="$1" want="$2" label="$3" code
    local attempt
    for attempt in $(seq 1 60); do
        ANSWER=$(probe "$container" "/ready")
        code=$(status_code "$ANSWER")
        if [ "$code" = "$want" ]; then
            return 0
        fi
        sleep 2
    done
    fail "$label never answered $want from /ready; the last answer was ${code:-nothing}"
}

if [ -n "$SERVICE_IMAGE" ]; then
    SERVICE_IMAGE_WAS_GIVEN=yes
    echo "== 0. using the image given: $SERVICE_IMAGE"
else
    echo "== 0. building the service image from the working tree"
    docker build -f Docker/Dockerfile -t "$BUILT_IMAGE" . >/dev/null
    SERVICE_IMAGE="$BUILT_IMAGE"
fi

echo "== 1. bringing up two replicas over their dependencies"
docker network create "$REDIS_NET" >/dev/null
docker network create "$CORE_NET" >/dev/null

docker run -d --name "$REDIS" --network "$REDIS_NET" --network-alias redis \
    "$REDIS_IMAGE" redis-server --requirepass "$REDIS_PASSWORD" >/dev/null
docker run -d --name "$MONGO" --network "$CORE_NET" --network-alias mongodb \
    -e MONGO_INITDB_ROOT_USERNAME="$MONGO_USER" \
    -e MONGO_INITDB_ROOT_PASSWORD="$MONGO_PASSWORD" \
    "$MONGO_IMAGE" >/dev/null

# Waits for a dependency to answer, since the service exits at startup without
# Redis and MongoDB and would otherwise fail as a race rather than as a result.
wait_for_dependency() {
    local container="$1" label="$2"
    shift 2
    for _ in $(seq 1 60); do
        if docker exec "$container" "$@" >/dev/null 2>&1; then
            return 0
        fi
        sleep 2
    done
    echo "FAIL: $label never became ready" >&2
    docker logs "$container" 2>&1 | tail -20 >&2
    exit 1
}

start_replica() {
    local name="$1"
    # Created before it is started, so BOTH networks are attached before the
    # process looks for Redis. Started without one it would exit rather than
    # come up unready, and this fixture would be testing the startup path
    # instead of the probes.
    docker create --name "$name" --network "$CORE_NET" \
        -e OCS_BIND_ADDRESS=0.0.0.0 \
        -e OCS_PORT="$PORT" \
        -e REDIS_HOST=redis \
        -e REDIS_PORT=6379 \
        -e REDIS_PASSWORD="$REDIS_PASSWORD" \
        -e REDIS_DB=0 \
        -e REDIS_TIMEOUT=5 \
        -e REDIS_CONNECT_TIMEOUT=2 \
        -e MONGODB_URI="mongodb://$MONGO_USER:$MONGO_PASSWORD@mongodb:27017" \
        -e MONGODB_DATABASE=optionchain_simulator \
        -e LOGLEVEL=INFO \
        "$SERVICE_IMAGE" >/dev/null
    # This attachment is the one the isolation step removes and restores.
    docker network connect "$REDIS_NET" "$name"
    docker start "$name" >/dev/null
}

# The probe container idles on the core network, where both replicas remain
# reachable by name whatever the Redis network is doing.
docker run -d --name "$PROBE" --network "$CORE_NET" --entrypoint sleep \
    "$CURL_IMAGE" infinity >/dev/null

wait_for_dependency "$REDIS" "Redis" redis-cli -a "$REDIS_PASSWORD" ping
wait_for_dependency "$MONGO" "MongoDB" \
    mongosh -u "$MONGO_USER" -p "$MONGO_PASSWORD" --quiet --eval 'db.version()'

start_replica "$FIRST"
start_replica "$SECOND"

wait_for_ready_code "$FIRST" 200 "the first replica"
FIRST_ID=$(instance_id "$ANSWER")
wait_for_ready_code "$SECOND" 200 "the second replica"
SECOND_ID=$(instance_id "$ANSWER")
[ -n "$FIRST_ID" ] || fail "the first replica served no x-ocs-instance header"
[ -n "$SECOND_ID" ] || fail "the second replica served no x-ocs-instance header"
[ "$FIRST_ID" != "$SECOND_ID" ] || \
    fail "both replicas report the instance id $FIRST_ID, so nothing below is about one of them"
echo "   both ready: $FIRST_ID and $SECOND_ID"

echo "== 2. cutting the first replica off from Redis"
docker network disconnect "$REDIS_NET" "$FIRST"

wait_for_ready_code "$FIRST" 503 "the isolated replica"
ISOLATED_ID=$(instance_id "$ANSWER")
[ "$ISOLATED_ID" = "$FIRST_ID" ] || \
    fail "the 503 came from $ISOLATED_ID rather than from the isolated replica $FIRST_ID"

ISOLATED_BODY=$(body_of "$ANSWER")
printf '%s' "$ISOLATED_BODY" | grep -q '"status":"not_ready"' || \
    fail "the isolated replica does not report itself not_ready: $ISOLATED_BODY"
printf '%s' "$ISOLATED_BODY" |
    grep -qE '"name":"redis","status":"down","reason":"(unreachable|timed_out)"' || \
    fail "the isolated replica does not name redis as down with a fixed reason: $ISOLATED_BODY"
printf '%s' "$ISOLATED_BODY" | grep -q '"name":"mongodb","status":"up"' || \
    fail "the isolation took more than Redis away, so the body says nothing about which dependency failed: $ISOLATED_BODY"
echo "   the isolated replica answers 503 naming redis, MongoDB still up"

# Liveness must NOT follow readiness, or an orchestrator restarts an instance
# every time a dependency hiccups and turns one outage into two.
ALIVE_ANSWER=$(probe "$FIRST" "/health")
ALIVE_CODE=$(status_code "$ALIVE_ANSWER")
[ "$ALIVE_CODE" = "200" ] || \
    fail "the isolated replica answers $ALIVE_CODE from /health; a failed dependency must not fail liveness"
echo "   and still answers 200 from /health"

HEALTHY_ANSWER=$(probe "$SECOND" "/ready")
HEALTHY_CODE=$(status_code "$HEALTHY_ANSWER")
HEALTHY_ID=$(instance_id "$HEALTHY_ANSWER")
[ "$HEALTHY_CODE" = "200" ] || \
    fail "the healthy replica answers $HEALTHY_CODE while its neighbour is isolated"
[ "$HEALTHY_ID" = "$SECOND_ID" ] || \
    fail "the 200 came from $HEALTHY_ID rather than from the healthy replica $SECOND_ID"
HEALTHY_BODY=$(body_of "$HEALTHY_ANSWER")
printf '%s' "$HEALTHY_BODY" | grep -q '"status":"ready"' || \
    fail "the healthy replica does not report itself ready: $HEALTHY_BODY"
echo "   the healthy replica keeps answering 200 as $HEALTHY_ID"

echo "== 3. restoring the network"
docker network connect "$REDIS_NET" "$FIRST"

wait_for_ready_code "$FIRST" 200 "the restored replica"
RECOVERED_ID=$(instance_id "$ANSWER")
# The id is generated once per process, so the same id is the proof that the
# recovery needed no restart.
[ "$RECOVERED_ID" = "$FIRST_ID" ] || \
    fail "the restored replica reports $RECOVERED_ID rather than $FIRST_ID, so it restarted rather than recovered"

RECOVERED_BODY=$(body_of "$ANSWER")
printf '%s' "$RECOVERED_BODY" | grep -q '"status":"ready"' || \
    fail "the restored replica does not report itself ready: $RECOVERED_BODY"
echo "   the same process, $RECOVERED_ID, reports itself ready again"

echo
echo "PASS: one replica reported its own broken dependency, the other kept serving, and the isolated one recovered without a restart"
