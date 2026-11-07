#!/bin/sh
set -e
dir=$(dirname "$0")

# Blank otadata so the bootloader starts ota_0, where the wired flash lands, even if a
# previous OTA left ota_1 selected
blank=$(mktemp)
trap 'rm -f "$blank"' EXIT
head -c 8192 /dev/zero | LC_ALL=C tr '\0' '\377' > "$blank"
probe-rs download --chip esp32c3 --binary-format bin --base-address 0xd000 "$blank"

probe-rs download --chip esp32c3 --idf-partition-table "$dir/partitions.csv" "$1" \
    && espflash monitor -p /dev/cu.usbmodem* --non-interactive
