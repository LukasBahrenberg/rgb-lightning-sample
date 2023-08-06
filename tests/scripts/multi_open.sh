#!/usr/bin/env bash

source tests/common.sh

get_node_ids

# create RGB UTXOs
create_utxos 1
create_utxos 2

# issue asset
issue_asset

# open channel (1)
open_channel 1 2 "$NODE2_PORT" "$node2_id" 1000
list_channels 1
list_channels 2

# send assets (1)
keysend 1 2 "$node2_id" 100
list_channels 1
list_channels 2