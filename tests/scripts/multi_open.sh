#!/usr/bin/env bash

source tests/common.sh

get_node_ids

# create RGB UTXOs
create_utxos 1
create_utxos 2
create_utxos 3

# issue asset
issue_asset

# open channel (1)
open_channel 1 2 "$NODE2_PORT" "$NODE2_ID" 500
open_channel 1 3 "$NODE3_PORT" "$NODE3_ID" 400

# send assets (1)
keysend 1 2 "$NODE2_ID" 250
keysend 1 3 "$NODE3_ID" 200

list_channels 1
list_channels 3
list_channels 2