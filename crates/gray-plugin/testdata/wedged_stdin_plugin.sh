#!/bin/sh
# Sidecar stub: answers the manifest handshake, then never reads stdin
# again — a child that stopped reading is exactly what tears a frame write.
read -r line
printf '%s\n' '{"id":1,"result":{"name":"wedged","version":"0.0.1","protocol":"1.1","tools":[],"commands":[],"hooks":[]}}'
while true; do sleep 1; done
