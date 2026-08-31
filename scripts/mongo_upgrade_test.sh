#!/usr/bin/env bash
#
# Proves that an EXISTING MongoDB deployment survives the upgrade the compose
# file pins.
#
# The Integration job starts every service container on an empty volume, so it
# proves the new image runs and says nothing about a data directory that
# already exists. That is where MongoDB upgrades actually fail: the directory
# carries a feature compatibility version, a binary refuses one above its own,
# and raising it is the step that stops being reversible.
#
# The sequence, which is the documented procedure executed rather than read:
#
#   1. start the PREVIOUS image on a named volume, write a document, record the
#      FCV;
#   2. stop it, start the NEW image on the SAME volume, and require the
#      document to still be there with the FCV unchanged;
#   3. PROVE the rollback: stop the new image and start the OLD one again on
#      the same, now-touched directory, requiring it to become ready with the
#      data and the FCV intact. An unchanged FCV is not by itself proof that
#      the old binary can still open a directory the new one has written to;
#   4. return to the new image and raise the FCV, which is the step that stops
#      being reversible.
#
# ClickHouse is deliberately NOT covered here. Its data directory carries its
# own expectations, but it has no equivalent of the FCV handshake: the server
# migrates its metadata on start and refuses to start on a directory written by
# a newer build, which is a failure an operator sees immediately rather than a
# silent one weeks later. If that changes, this script is the shape to copy.
#
# Usage:
#   scripts/mongo_upgrade_test.sh [FROM_IMAGE] [TO_IMAGE] [TARGET_FCV]
#
# The defaults are the previous and current pins. Pass them explicitly when
# bumping so the test runs against the two images the bump moves between.

set -euo pipefail

# Pinned by digest, like everything else in this repository: a mutable tag can
# be republished, and then the test proves an upgrade path from bytes nobody
# ships. This is the digest the compose file carried before the bump.
FROM_IMAGE="${1:-mongo:8.0.29@sha256:de267922bc1153d923f5c9dc429f21c11faf18299080c1ce04d6d6007097fb06}"
TO_IMAGE="${2:-mongo:8.2.12@sha256:e0ce8c35124d4a9f9785532d1f268f39e9728ffa1cb38f46fa482436424c4bd3}"
TARGET_FCV="${3:-8.2}"

VOLUME="ocs-mongo-upgrade-$$"
CONTAINER="ocs-mongo-upgrade-$$"
USER_NAME="admin"
PASSWORD="password"
PROBE_DB="ocs_upgrade_probe"

cleanup() {
    docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
    docker volume rm -f "$VOLUME" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# Runs one mongosh expression and prints its last line.
mongosh_eval() {
    docker exec "$CONTAINER" mongosh -u "$USER_NAME" -p "$PASSWORD" --quiet --eval "$1" 2>/dev/null | tail -1
}

# Waits for the container to answer, or gives up with the log.
wait_for_mongo() {
    local label="$1"
    for _ in $(seq 1 60); do
        if docker exec "$CONTAINER" mongosh -u "$USER_NAME" -p "$PASSWORD" --quiet --eval 'db.version()' >/dev/null 2>&1; then
            return 0
        fi
        sleep 2
    done
    echo "FAIL: $label never became ready" >&2
    docker logs "$CONTAINER" 2>&1 | tail -30 >&2
    exit 1
}

start_on_volume() {
    local image="$1"
    docker run -d --rm \
        --name "$CONTAINER" \
        -v "$VOLUME:/data/db" \
        -e MONGO_INITDB_ROOT_USERNAME="$USER_NAME" \
        -e MONGO_INITDB_ROOT_PASSWORD="$PASSWORD" \
        "$image" >/dev/null
}

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

echo "== 1. seeding $FROM_IMAGE on a fresh volume"
docker volume create "$VOLUME" >/dev/null
start_on_volume "$FROM_IMAGE"
wait_for_mongo "$FROM_IMAGE"

FROM_VERSION=$(mongosh_eval 'db.version()')
echo "   running $FROM_VERSION"

mongosh_eval "db.getSiblingDB('$PROBE_DB').events.insertOne({probe: 'upgrade', at: new Date()})" >/dev/null
SEEDED=$(mongosh_eval "db.getSiblingDB('$PROBE_DB').events.countDocuments()")
[ "$SEEDED" = "1" ] || fail "the probe document was not written, count was $SEEDED"

FCV_BEFORE=$(mongosh_eval 'db.adminCommand({getParameter: 1, featureCompatibilityVersion: 1}).featureCompatibilityVersion.version')
echo "   seeded one document, FCV $FCV_BEFORE"

echo "== 2. swapping the binary under the same volume"
docker stop "$CONTAINER" >/dev/null
sleep 2
start_on_volume "$TO_IMAGE"
wait_for_mongo "$TO_IMAGE"

TO_VERSION=$(mongosh_eval 'db.version()')
echo "   running $TO_VERSION"

SURVIVED=$(mongosh_eval "db.getSiblingDB('$PROBE_DB').events.countDocuments()")
[ "$SURVIVED" = "1" ] || fail "the document did not survive the swap, count was $SURVIVED"

FCV_AFTER_SWAP=$(mongosh_eval 'db.adminCommand({getParameter: 1, featureCompatibilityVersion: 1}).featureCompatibilityVersion.version')
[ "$FCV_AFTER_SWAP" = "$FCV_BEFORE" ] || \
    fail "the swap moved the FCV from $FCV_BEFORE to $FCV_AFTER_SWAP; it must stay put, or the rollback below cannot work"
echo "   document intact, FCV still $FCV_AFTER_SWAP"

echo "== 3. rolling back to $FROM_IMAGE on the touched volume"
# The claim being tested is that step 2 is reversible. An unchanged FCV does
# not establish that: the new binary has opened the directory and may have
# written to it. The only proof is putting the old image back.
docker stop "$CONTAINER" >/dev/null
sleep 2
start_on_volume "$FROM_IMAGE"
wait_for_mongo "$FROM_IMAGE (rollback)"

ROLLBACK_VERSION=$(mongosh_eval 'db.version()')
[ "$ROLLBACK_VERSION" = "$FROM_VERSION" ] || \
    fail "the rollback is running $ROLLBACK_VERSION rather than $FROM_VERSION"

ROLLED_BACK=$(mongosh_eval "db.getSiblingDB('$PROBE_DB').events.countDocuments()")
[ "$ROLLED_BACK" = "1" ] || fail "the document did not survive the rollback, count was $ROLLED_BACK"

FCV_ROLLBACK=$(mongosh_eval 'db.adminCommand({getParameter: 1, featureCompatibilityVersion: 1}).featureCompatibilityVersion.version')
[ "$FCV_ROLLBACK" = "$FCV_BEFORE" ] || \
    fail "the rollback reports FCV $FCV_ROLLBACK rather than $FCV_BEFORE"
echo "   $ROLLBACK_VERSION came back up on the touched directory, document intact, FCV $FCV_ROLLBACK"

echo "== 4. returning to $TO_IMAGE and raising the feature compatibility version"
docker stop "$CONTAINER" >/dev/null
sleep 2
start_on_volume "$TO_IMAGE"
wait_for_mongo "$TO_IMAGE (second time)"

mongosh_eval "db.adminCommand({setFeatureCompatibilityVersion: '$TARGET_FCV', confirm: true})" >/dev/null
FCV_FINAL=$(mongosh_eval 'db.adminCommand({getParameter: 1, featureCompatibilityVersion: 1}).featureCompatibilityVersion.version')
[ "$FCV_FINAL" = "$TARGET_FCV" ] || fail "the FCV is $FCV_FINAL after asking for $TARGET_FCV"

STILL_THERE=$(mongosh_eval "db.getSiblingDB('$PROBE_DB').events.countDocuments()")
[ "$STILL_THERE" = "1" ] || fail "the document was lost raising the FCV, count was $STILL_THERE"

echo "   FCV $FCV_FINAL, document intact"
echo
echo "PASS: $FROM_VERSION to $TO_VERSION on a persisted volume, rollback to $FROM_VERSION proven, FCV $FCV_BEFORE to $FCV_FINAL"
