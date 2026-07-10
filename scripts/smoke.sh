#!/usr/bin/env bash
# Smoke test for runic V0.
#
# Hits api.ipify.org through the local SOCKS5 endpoint and reports the outcome:
#   - HTTP 200 with a JSON `{"ip": ...}` body → chain works end-to-end (real creds).
#   - Anything else (typically a SOCKS5 "general failure" surfaced by curl, with
#     a 407 in `docker logs runic`) → CONNECT roundtrip works but auth is wrong.
#     Still proves the wiring is good — only the creds need to be fixed.
#
# Usage:
#   ./scripts/smoke.sh                 # default: auto-discover the port via the
#                                      # admin status endpoint (ADMIN, :48484)
#   PROXY=127.0.0.1:7878 ./scripts/smoke.sh   # explicit/pinned port
#
# Exits 0 on a 200 response, 1 otherwise.

set -euo pipefail

ADMIN="${ADMIN:-127.0.0.1:48484}"
# Default listen is auto-port (the OS picks): read the actually-bound address
# from the status surface unless the caller pins PROXY explicitly.
if [[ -z "${PROXY:-}" ]]; then
  PROXY="$(curl -sS --max-time 5 "http://${ADMIN}/v1/status" \
    | sed -n 's/.*"listen":"\([^"]*\)".*/\1/p')"
  if [[ -z "${PROXY}" ]]; then
    echo "✗ could not discover the SOCKS5 port from http://${ADMIN}/v1/status" >&2
    echo "  (is runic running? or set PROXY=host:port explicitly)" >&2
    exit 1
  fi
  echo "→ discovered SOCKS5 endpoint ${PROXY} via http://${ADMIN}/v1/status"
fi
TARGET="${TARGET:-https://api.ipify.org?format=json}"

echo "→ curl --socks5 ${PROXY} ${TARGET}"
echo

# We split status from body so we can print both. -sS keeps errors visible without
# the progress meter; -w writes the status on its own line at the end of the body.
response="$(curl --socks5 "${PROXY}" "${TARGET}" \
  -sS \
  --max-time 15 \
  -w '\nHTTP_STATUS=%{http_code}\n' \
  || true)"

echo "${response}"

status="$(echo "${response}" | awk -F= '/^HTTP_STATUS=/ { print $2 }')"

if [[ "${status}" == "200" ]]; then
  echo
  echo "✓ chain OK (real DataImpulse creds)"
  exit 0
else
  echo
  echo "✗ chain failed at SOCKS5/CONNECT layer (status='${status:-none}')"
  echo "  Check 'docker logs runic' — a 407 there means the CONNECT round-trip works,"
  echo "  only the creds are wrong (mock-creds path expected outcome)."
  exit 1
fi
