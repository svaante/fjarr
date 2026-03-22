#!/bin/sh
probe-rs download --chip esp32c3 "$1" && espflash monitor -p /dev/cu.usbmodem* --non-interactive
