#!/usr/bin/env bash
# Build the vfm-server image from source (server/Dockerfile) and push it to a registry.
#
#   server/deploy/build-push.sh                  -> cr-0.rune.local/vfm-server:<version>-<commit> and :latest
#   REGISTRY=ghcr.io/me server/deploy/build-push.sh
#   ENGINE=podman server/deploy/build-push.sh    (default: docker if present, else podman)
#   CHANNEL=stage server/deploy/build-push.sh    -> :<version>-<commit> and :stage (for vfm-server-stage.yaml)
#
# Log in first (docker login cr-0.rune.local / podman login …). If the registry uses a
# self-signed certificate, trust it for the engine (Docker: /etc/docker/certs.d/<host>/ca.crt,
# Podman: /etc/containers/certs.d/<host>/ca.crt).
# Then pin the printed tag in server/deploy/k8s/vfm-server.yaml.
set -euo pipefail
cd "$(dirname "$0")/../.."

REGISTRY="${REGISTRY:-cr-0.rune.local}"
IMAGE="$REGISTRY/vfm-server"
ENGINE="${ENGINE:-$(command -v docker >/dev/null 2>&1 && echo docker || echo podman)}"
PLATFORM="${PLATFORM:-linux/amd64}"
# The moving tag: latest (production) or stage.
CHANNEL="${CHANNEL:-latest}"

VERSION=$(sed -n 's/^version *= *"\(.*\)"/\1/p' server/Cargo.toml | head -n1)
COMMIT=$(git rev-parse --short HEAD 2>/dev/null || echo local)
DIRTY=$(git diff --quiet HEAD -- server crates 2>/dev/null || echo -dirty)
TAG="$VERSION-$COMMIT$DIRTY"

echo "→ Building $IMAGE:$TAG with $ENGINE ($PLATFORM)"
"$ENGINE" build --platform "$PLATFORM" -f server/Dockerfile \
  --label "org.opencontainers.image.version=$TAG" \
  --label "org.opencontainers.image.revision=$COMMIT" \
  -t "$IMAGE:$TAG" -t "$IMAGE:$CHANNEL" .

echo "→ Pushing"
"$ENGINE" push "$IMAGE:$TAG"
"$ENGINE" push "$IMAGE:$CHANNEL"

echo
echo "✓ Pushed $IMAGE:$TAG (also :$CHANNEL)"
if [ "$CHANNEL" = stage ]; then
  echo "  Staging uses :stage: kubectl -n vfm-server-staging rollout restart deploy/vfm-server"
else
  echo "  Pin it in server/deploy/k8s/vfm-server.yaml:  image: $IMAGE:$TAG"
fi
