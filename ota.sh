#!/bin/sh
# Build, sign and push firmware over http.
#
#   ./ota.sh keygen        create keys/ota.key (private, keep safe) and keys/ota.pub
#   ./ota.sh [host]        build + sign + upload (default host: fjarr.local)
#   ./ota.sh sign          build + sign only; writes target/ota/app.signed
set -eu
cd "$(dirname "$0")"

KEY=keys/ota.key
PUB=keys/ota.pub
ELF=target/riscv32imc-unknown-none-elf/release/esp32-ir
OUT=target/ota
PORT=3232

if [ "${1:-}" = keygen ]; then
    [ -e "$KEY" ] && { echo "$KEY exists; refusing to overwrite" >&2; exit 1; }
    mkdir -p keys
    (umask 077 && openssl genpkey -algorithm ed25519 -out "$KEY")
    # Raw 32 byte public key is the tail of the DER SubjectPublicKeyInfo
    openssl pkey -in "$KEY" -pubout -outform DER | tail -c 32 > "$PUB"
    echo "wrote $KEY and $PUB; flash once over the wire so the device trusts the new key"
    exit 0
fi

[ -r "$KEY" ] || { echo "$KEY missing; run ./ota.sh keygen" >&2; exit 1; }

# Refuse to sign with a key the firmware won't accept
if [ "$(openssl pkey -in "$KEY" -pubout -outform DER | tail -c 32 | od -An -tx1)" != "$(od -An -tx1 "$PUB")" ]; then
    echo "$KEY does not match $PUB" >&2
    exit 1
fi

cargo build --release
mkdir -p "$OUT"
espflash save-image --chip esp32c3 --flash-size 4mb --ignore-app-descriptor \
    --partition-table partitions.csv "$ELF" "$OUT/app.bin" >/dev/null
openssl dgst -sha256 -binary "$OUT/app.bin" > "$OUT/app.sha256"
openssl pkeyutl -sign -inkey "$KEY" -rawin -in "$OUT/app.sha256" -out "$OUT/app.sig"
cat "$OUT/app.bin" "$OUT/app.sig" > "$OUT/app.signed"
echo "signed $(wc -c < "$OUT/app.bin" | tr -d ' ') byte image -> $OUT/app.signed"

[ "${1:-}" = sign ] && exit 0

HOST=${1:-fjarr.local}
curl --fail-with-body --data-binary "@$OUT/app.signed" \
    -H 'Content-Type: application/octet-stream' "http://$HOST:$PORT/ota"
